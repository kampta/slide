use crate::backend::BackendKind;
use crate::scheduled::{self, ScheduleKind, ScheduledJob, MAX_JOBS_PER_SESSION};
use crate::session::{Location, Session, SessionState, SupervisorKind};
use crate::turn_diff::{NewTurnDiff, TurnDiff, TurnDiffSummary};
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension, Row};
use std::path::Path;
use tokio_rusqlite::Connection;

/// Schema migrations applied in order. Each entry bumps `user_version` by 1
/// after its SQL runs. Never edit a shipped migration — add a new one.
const MIGRATIONS: &[&str] = &[
    // v0 → v1: initial schema.
    r#"
    CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        backend TEXT NOT NULL,
        location TEXT NOT NULL,
        ssh_host TEXT,
        base_dir TEXT NOT NULL,
        project_path TEXT NOT NULL,
        worktree INTEGER NOT NULL,
        state TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        last_activity INTEGER NOT NULL
    );
    "#,
    // v1 → v2: supervisor strategy, host-side log path + offset, backend session id.
    r#"
    ALTER TABLE sessions ADD COLUMN supervisor TEXT NOT NULL DEFAULT 'direct';
    ALTER TABLE sessions ADD COLUMN host_log_path TEXT;
    ALTER TABLE sessions ADD COLUMN log_offset INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE sessions ADD COLUMN backend_session_id TEXT;
    "#,
    // v2 → v3: collapse Exited + Archived into a single Stopped state.
    r#"
    UPDATE sessions SET state='stopped' WHERE state IN ('exited', 'archived');
    "#,
    // v3 → v4: bounded, per-session Git changes captured at turn boundaries.
    r#"
    CREATE TABLE turn_diffs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL,
        turn INTEGER NOT NULL,
        started_at INTEGER NOT NULL,
        completed_at INTEGER NOT NULL,
        files_changed INTEGER NOT NULL,
        additions INTEGER NOT NULL,
        deletions INTEGER NOT NULL,
        truncated INTEGER NOT NULL DEFAULT 0,
        patch TEXT NOT NULL,
        UNIQUE(session_id, turn)
    );
    CREATE INDEX turn_diffs_session_turn ON turn_diffs(session_id, turn DESC);
    "#,
    // v4 → v5: durable Slide-side fork lineage.
    r#"
    ALTER TABLE sessions ADD COLUMN parent_session_id TEXT;
    CREATE INDEX sessions_parent ON sessions(parent_session_id);
    "#,
    // v5 → v6: durable, session-scoped scheduled prompts.
    r#"
    CREATE TABLE scheduled_jobs (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        title TEXT NOT NULL,
        prompt TEXT NOT NULL,
        schedule_kind TEXT NOT NULL,
        interval_seconds INTEGER,
        next_run_at INTEGER NOT NULL,
        retry_at INTEGER,
        enabled INTEGER NOT NULL,
        last_run_at INTEGER,
        last_error TEXT,
        run_count INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX scheduled_jobs_session ON scheduled_jobs(session_id, created_at DESC);
    CREATE INDEX scheduled_jobs_due
        ON scheduled_jobs(enabled, COALESCE(retry_at, next_run_at));
    "#,
];

const TURN_DIFF_HISTORY_LIMIT: i64 = 50;

async fn migrate(conn: &Connection) -> Result<()> {
    conn.call(|conn| {
        let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let target = i as u32 + 1;
            if current < target {
                conn.execute_batch(sql)?;
                // PRAGMA doesn't accept bind params; an integer is safe to format.
                conn.execute_batch(&format!("PRAGMA user_version = {target}"))?;
            }
        }
        Ok(())
    })
    .await
    .context("apply migrations")?;
    Ok(())
}

/// Async wrapper around the SQLite connection.
///
/// `tokio_rusqlite::Connection` owns a single background thread and a
/// command queue, so every method here serializes through that one thread —
/// the same effective concurrency as the previous `Mutex<Connection>` model
/// but without holding a sync lock from inside an async runtime worker. The
/// caller-facing surface gains `.await` on each method; everything else is
/// unchanged.
pub struct Store {
    conn: Connection,
}

fn session_from_row(r: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: r.get(0)?,
        name: r.get(1)?,
        backend: BackendKind::from_str(&r.get::<_, String>(2)?).unwrap_or(BackendKind::Claude),
        location: Location::from_str(&r.get::<_, String>(3)?).unwrap_or(Location::Local),
        ssh_host: r.get(4)?,
        base_dir: r.get(5)?,
        project_path: r.get(6)?,
        worktree: r.get::<_, i32>(7)? != 0,
        state: SessionState::from_str(&r.get::<_, String>(8)?).unwrap_or(SessionState::Stopped),
        created_at: r.get(9)?,
        last_activity: r.get(10)?,
        supervisor: SupervisorKind::from_str(&r.get::<_, String>(11)?)
            .unwrap_or(SupervisorKind::Direct),
        host_log_path: r.get(12)?,
        log_offset: r.get(13)?,
        backend_session_id: r.get(14)?,
        parent_session_id: r.get(15)?,
    })
}

fn turn_diff_summary_from_row(r: &Row<'_>) -> rusqlite::Result<TurnDiffSummary> {
    Ok(TurnDiffSummary {
        id: r.get(0)?,
        turn: r.get(1)?,
        started_at: r.get(2)?,
        completed_at: r.get(3)?,
        files_changed: r.get::<_, i64>(4)?.max(0) as u64,
        additions: r.get::<_, i64>(5)?.max(0) as u64,
        deletions: r.get::<_, i64>(6)?.max(0) as u64,
        truncated: r.get::<_, i64>(7)? != 0,
    })
}

fn scheduled_job_from_row(r: &Row<'_>) -> rusqlite::Result<ScheduledJob> {
    Ok(ScheduledJob {
        id: r.get(0)?,
        session_id: r.get(1)?,
        title: r.get(2)?,
        prompt: r.get(3)?,
        schedule_kind: ScheduleKind::parse(&r.get::<_, String>(4)?).unwrap_or(ScheduleKind::Once),
        interval_seconds: r.get(5)?,
        next_run_at: r.get(6)?,
        retry_at: r.get(7)?,
        enabled: r.get::<_, i64>(8)? != 0,
        last_run_at: r.get(9)?,
        last_error: r.get(10)?,
        run_count: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

const SESSION_COLUMNS: &str =
    "id, name, backend, location, ssh_host, base_dir, project_path, worktree, state, \
     created_at, last_activity, supervisor, host_log_path, log_offset, backend_session_id, parent_session_id";
const SCHEDULED_JOB_COLUMNS: &str =
    "id, session_id, title, prompt, schedule_kind, interval_seconds, next_run_at, retry_at, \
     enabled, last_run_at, last_error, run_count, created_at, updated_at";

fn claimed_schedule(job: &ScheduledJob, now: i64) -> Result<(i64, Option<i64>, bool)> {
    let due = job.enabled && job.retry_at.unwrap_or(job.next_run_at) <= now;
    if !due {
        return Ok((job.next_run_at, job.retry_at, job.enabled));
    }
    match job.schedule_kind {
        ScheduleKind::Once => Ok((job.next_run_at, None, false)),
        ScheduleKind::Interval => Ok((
            scheduled::next_interval_occurrence(job, now)
                .ok_or_else(|| anyhow::anyhow!("invalid interval schedule"))?,
            None,
            true,
        )),
    }
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).await.context("open sqlite")?;
        migrate(&conn).await?;
        Ok(Self { conn })
    }

    pub async fn insert(&self, s: &Session) -> Result<()> {
        let s = s.clone();
        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO sessions \
                     (id, name, backend, location, ssh_host, base_dir, project_path, worktree, state, created_at, last_activity, \
                      supervisor, host_log_path, log_offset, backend_session_id, parent_session_id) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
                    params![
                        s.id,
                        s.name,
                        s.backend.as_str(),
                        s.location.as_str(),
                        s.ssh_host,
                        s.base_dir,
                        s.project_path,
                        s.worktree as i32,
                        s.state.as_str(),
                        s.created_at,
                        s.last_activity,
                        s.supervisor.as_str(),
                        s.host_log_path,
                        s.log_offset,
                        s.backend_session_id,
                        s.parent_session_id,
                    ],
                )?;
                Ok(())
            })
            .await
            .context("insert session")?;
        Ok(())
    }

    pub async fn update_state(
        &self,
        id: &str,
        state: SessionState,
        last_activity: i64,
    ) -> Result<()> {
        let id = id.to_string();
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE sessions SET state=?1, last_activity=?2 WHERE id=?3",
                    params![state.as_str(), last_activity, id],
                )?;
                Ok(())
            })
            .await
            .context("update state")?;
        Ok(())
    }

    pub async fn update_name(&self, id: &str, name: &str) -> Result<()> {
        let id = id.to_string();
        let name = name.to_string();
        self.conn
            .call(move |c| {
                c.execute("UPDATE sessions SET name=?1 WHERE id=?2", params![name, id])?;
                Ok(())
            })
            .await
            .context("update name")?;
        Ok(())
    }

    /// Record the backend-native session id (e.g. claude's `--resume`
    /// target) once discovery has found it.
    pub async fn update_backend_session_id(
        &self,
        id: &str,
        backend_session_id: &str,
    ) -> Result<()> {
        let id = id.to_string();
        let backend_session_id = backend_session_id.to_string();
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE sessions SET backend_session_id=?1 WHERE id=?2",
                    params![backend_session_id, id],
                )?;
                Ok(())
            })
            .await
            .context("update backend session id")?;
        Ok(())
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.conn
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute("DELETE FROM turn_diffs WHERE session_id=?1", params![id])?;
                tx.execute(
                    "DELETE FROM scheduled_jobs WHERE session_id=?1",
                    params![id],
                )?;
                tx.execute("DELETE FROM sessions WHERE id=?1", params![id])?;
                tx.commit()?;
                Ok(())
            })
            .await
            .context("delete session")?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<Session>> {
        let id = id.to_string();
        self.conn
            .call(move |c| {
                Ok(c.query_row(
                    &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id=?1"),
                    params![id],
                    session_from_row,
                )
                .optional()?)
            })
            .await
            .context("get session")
    }

    pub async fn list(&self) -> Result<Vec<Session>> {
        let rows = self
            .conn
            .call(|c| {
                let mut stmt = c.prepare(&format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions ORDER BY last_activity DESC"
                ))?;
                let rows = stmt
                    .query_map([], session_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .context("list sessions")?;
        Ok(rows)
    }

    /// Persist one completed turn and prune old patch bodies in the same
    /// transaction. The conditional session lookup prevents a queued diff
    /// worker from recreating history after its session was deleted.
    pub async fn insert_turn_diff(
        &self,
        session_id: &str,
        diff: NewTurnDiff,
    ) -> Result<Option<TurnDiffSummary>> {
        let session_id = session_id.to_string();
        let files_changed = to_sql_count(diff.files_changed);
        let additions = to_sql_count(diff.additions);
        let deletions = to_sql_count(diff.deletions);
        self.conn
            .call(move |c| {
                let tx = c.transaction()?;
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    params![session_id],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Ok(None);
                }
                let turn: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(turn), 0) + 1 FROM turn_diffs WHERE session_id=?1",
                    params![session_id],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "INSERT INTO turn_diffs
                     (session_id, turn, started_at, completed_at, files_changed, additions, deletions, truncated, patch)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        session_id,
                        turn,
                        diff.started_at,
                        diff.completed_at,
                        files_changed,
                        additions,
                        deletions,
                        diff.truncated as i32,
                        diff.patch,
                    ],
                )?;
                let id = tx.last_insert_rowid();
                tx.execute(
                    "DELETE FROM turn_diffs
                     WHERE session_id=?1 AND turn <= ?2 - ?3",
                    params![session_id, turn, TURN_DIFF_HISTORY_LIMIT],
                )?;
                tx.commit()?;
                Ok(Some(TurnDiffSummary {
                    id,
                    turn,
                    started_at: diff.started_at,
                    completed_at: diff.completed_at,
                    files_changed: files_changed as u64,
                    additions: additions as u64,
                    deletions: deletions as u64,
                    truncated: diff.truncated,
                }))
            })
            .await
            .context("insert turn diff")
    }

    pub async fn list_turn_diffs(&self, session_id: &str) -> Result<Vec<TurnDiffSummary>> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT id, turn, started_at, completed_at, files_changed, additions, deletions, truncated
                     FROM turn_diffs WHERE session_id=?1 ORDER BY turn DESC LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(
                        params![session_id, TURN_DIFF_HISTORY_LIMIT],
                        turn_diff_summary_from_row,
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .context("list turn diffs")
    }

    pub async fn get_turn_diff(&self, session_id: &str, id: i64) -> Result<Option<TurnDiff>> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |c| {
                Ok(c.query_row(
                    "SELECT id, turn, started_at, completed_at, files_changed, additions, deletions, truncated, patch
                     FROM turn_diffs WHERE session_id=?1 AND id=?2",
                    params![session_id, id],
                    |row| {
                        Ok(TurnDiff {
                            summary: turn_diff_summary_from_row(row)?,
                            patch: row.get(8)?,
                        })
                    },
                )
                .optional()?)
            })
            .await
            .context("get turn diff")
    }

    pub async fn insert_scheduled_job(&self, job: ScheduledJob) -> Result<Option<ScheduledJob>> {
        let returned = job.clone();
        let inserted = self
            .conn
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let session_exists: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
                    params![job.session_id],
                    |row| row.get(0),
                )?;
                if !session_exists {
                    return Ok(false);
                }
                let count: i64 = transaction.query_row(
                    "SELECT COUNT(*) FROM scheduled_jobs WHERE session_id=?1",
                    params![job.session_id],
                    |row| row.get(0),
                )?;
                if count >= MAX_JOBS_PER_SESSION {
                    return Ok(false);
                }
                transaction.execute(
                    "INSERT INTO scheduled_jobs
                     (id, session_id, title, prompt, schedule_kind, interval_seconds, next_run_at,
                      retry_at, enabled, last_run_at, last_error, run_count, created_at, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                    params![
                        job.id,
                        job.session_id,
                        job.title,
                        job.prompt,
                        job.schedule_kind.as_str(),
                        job.interval_seconds,
                        job.next_run_at,
                        job.retry_at,
                        job.enabled as i32,
                        job.last_run_at,
                        job.last_error,
                        job.run_count,
                        job.created_at,
                        job.updated_at,
                    ],
                )?;
                transaction.commit()?;
                Ok(true)
            })
            .await
            .context("insert scheduled job")?;
        Ok(inserted.then_some(returned))
    }

    pub async fn list_scheduled_jobs(&self, session_id: &str) -> Result<Vec<ScheduledJob>> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |connection| {
                let mut statement = connection.prepare(&format!(
                    "SELECT {SCHEDULED_JOB_COLUMNS} FROM scheduled_jobs
                     WHERE session_id=?1 ORDER BY created_at DESC"
                ))?;
                let rows = statement
                    .query_map(params![session_id], scheduled_job_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .context("list scheduled jobs")
    }

    pub async fn get_scheduled_job(
        &self,
        session_id: &str,
        job_id: &str,
    ) -> Result<Option<ScheduledJob>> {
        let session_id = session_id.to_string();
        let job_id = job_id.to_string();
        self.conn
            .call(move |connection| {
                Ok(connection
                    .query_row(
                        &format!(
                            "SELECT {SCHEDULED_JOB_COLUMNS} FROM scheduled_jobs
                             WHERE session_id=?1 AND id=?2"
                        ),
                        params![session_id, job_id],
                        scheduled_job_from_row,
                    )
                    .optional()?)
            })
            .await
            .context("get scheduled job")
    }

    pub async fn set_scheduled_job_enabled(
        &self,
        session_id: &str,
        job_id: &str,
        enabled: bool,
        now: i64,
    ) -> Result<Option<ScheduledJob>> {
        let session_id = session_id.to_string();
        let job_id = job_id.to_string();
        self.conn
            .call(move |connection| {
                let changed = connection.execute(
                    "UPDATE scheduled_jobs
                     SET enabled=?1, retry_at=NULL, last_error=NULL, updated_at=?2
                     WHERE session_id=?3 AND id=?4",
                    params![enabled as i32, now, session_id, job_id],
                )?;
                if changed == 0 {
                    return Ok(None);
                }
                Ok(Some(connection.query_row(
                    &format!("SELECT {SCHEDULED_JOB_COLUMNS} FROM scheduled_jobs WHERE id=?1"),
                    params![job_id],
                    scheduled_job_from_row,
                )?))
            })
            .await
            .context("update scheduled job")
    }

    pub async fn delete_scheduled_job(&self, session_id: &str, job_id: &str) -> Result<bool> {
        let session_id = session_id.to_string();
        let job_id = job_id.to_string();
        self.conn
            .call(move |connection| {
                Ok(connection.execute(
                    "DELETE FROM scheduled_jobs WHERE session_id=?1 AND id=?2",
                    params![session_id, job_id],
                )? > 0)
            })
            .await
            .context("delete scheduled job")
    }

    pub async fn next_scheduled_wake(&self) -> Result<Option<i64>> {
        self.conn
            .call(|connection| {
                Ok(connection.query_row(
                    "SELECT MIN(COALESCE(retry_at, next_run_at))
                     FROM scheduled_jobs WHERE enabled=1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .context("read next scheduled wake")
    }

    pub async fn due_scheduled_jobs(&self, now: i64, limit: i64) -> Result<Vec<ScheduledJob>> {
        self.conn
            .call(move |connection| {
                let mut statement = connection.prepare(&format!(
                    "SELECT {SCHEDULED_JOB_COLUMNS} FROM scheduled_jobs
                     WHERE enabled=1 AND COALESCE(retry_at, next_run_at) <= ?1
                     ORDER BY COALESCE(retry_at, next_run_at), created_at LIMIT ?2"
                ))?;
                let rows = statement
                    .query_map(params![now, limit], scheduled_job_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .context("list due scheduled jobs")
    }

    pub async fn defer_scheduled_job(
        &self,
        job: &ScheduledJob,
        retry_at: i64,
        error: &str,
        now: i64,
    ) -> Result<bool> {
        let job = job.clone();
        let error = error.to_string();
        self.conn
            .call(move |connection| {
                Ok(connection.execute(
                    "UPDATE scheduled_jobs SET retry_at=?1, last_error=?2, updated_at=?3
                     WHERE id=?4 AND session_id=?5 AND enabled=1 AND next_run_at=?6
                       AND retry_at IS ?7",
                    params![
                        retry_at,
                        error,
                        now,
                        job.id,
                        job.session_id,
                        job.next_run_at,
                        job.retry_at,
                    ],
                )? > 0)
            })
            .await
            .context("defer scheduled job")
    }

    /// Atomically advance/disable a due occurrence before it reaches the
    /// terminal. The expected schedule fields prevent a stale scheduler read
    /// from submitting a job that the user edited or disabled meanwhile.
    pub async fn claim_scheduled_job(&self, job: &ScheduledJob, now: i64) -> Result<bool> {
        let (next_run_at, _, enabled) = claimed_schedule(job, now)?;
        let job = job.clone();
        self.conn
            .call(move |connection| {
                Ok(connection.execute(
                    "UPDATE scheduled_jobs
                     SET next_run_at=?1, retry_at=NULL, enabled=?2, last_run_at=?3,
                         last_error=NULL, run_count=run_count+1, updated_at=?3
                     WHERE id=?4 AND session_id=?5 AND enabled=1 AND next_run_at=?6
                       AND retry_at IS ?7 AND COALESCE(retry_at, next_run_at) <= ?3",
                    params![
                        next_run_at,
                        enabled as i32,
                        now,
                        job.id,
                        job.session_id,
                        job.next_run_at,
                        job.retry_at,
                    ],
                )? > 0)
            })
            .await
            .context("claim scheduled job")
    }

    /// Record a manual submission atomically. If the scheduled occurrence is
    /// already due, consume it too; otherwise the future cadence is left
    /// untouched. This prevents Run now and the deadline task from both
    /// submitting the same due occurrence.
    pub async fn claim_manual_scheduled_job(&self, job: &ScheduledJob, now: i64) -> Result<bool> {
        let (next_run_at, retry_at, enabled) = claimed_schedule(job, now)?;
        let job = job.clone();
        self.conn
            .call(move |connection| {
                Ok(connection.execute(
                    "UPDATE scheduled_jobs
                     SET next_run_at=?1, retry_at=?2, enabled=?3, last_run_at=?4,
                         last_error=NULL, run_count=run_count+1, updated_at=?4
                     WHERE id=?5 AND session_id=?6 AND next_run_at=?7
                       AND retry_at IS ?8 AND enabled=?9",
                    params![
                        next_run_at,
                        retry_at,
                        enabled as i32,
                        now,
                        job.id,
                        job.session_id,
                        job.next_run_at,
                        job.retry_at,
                        job.enabled as i32,
                    ],
                )? > 0)
            })
            .await
            .context("claim manual scheduled job")
    }

    /// Restore the exact pre-claim schedule when writing to the PTY fails.
    /// Matching the claimed fields makes this a no-op if another operation
    /// changed the job in the meantime.
    pub async fn rollback_scheduled_job_claim(
        &self,
        job: &ScheduledJob,
        claimed_at: i64,
        message: &str,
    ) -> Result<bool> {
        let (claimed_next_run_at, claimed_retry_at, claimed_enabled) =
            claimed_schedule(job, claimed_at)?;
        let job = job.clone();
        let message = message.to_string();
        self.conn
            .call(move |connection| {
                Ok(connection.execute(
                    "UPDATE scheduled_jobs
                     SET next_run_at=?1, retry_at=?2, enabled=?3, last_run_at=?4,
                         last_error=?5, run_count=MAX(0, run_count-1), updated_at=?6
                     WHERE id=?7 AND session_id=?8 AND next_run_at=?9
                       AND retry_at IS ?10 AND enabled=?11 AND last_run_at IS ?6",
                    params![
                        job.next_run_at,
                        job.retry_at,
                        job.enabled as i32,
                        job.last_run_at,
                        message,
                        claimed_at,
                        job.id,
                        job.session_id,
                        claimed_next_run_at,
                        claimed_retry_at,
                        claimed_enabled as i32,
                    ],
                )? > 0)
            })
            .await
            .context("roll back scheduled job claim")
    }

    pub async fn record_scheduled_job_error(
        &self,
        job_id: &str,
        message: &str,
        now: i64,
    ) -> Result<()> {
        let job_id = job_id.to_string();
        let message = message.to_string();
        self.conn
            .call(move |connection| {
                connection.execute(
                    "UPDATE scheduled_jobs SET last_error=?1, updated_at=?2 WHERE id=?3",
                    params![message, now, job_id],
                )?;
                Ok(())
            })
            .await
            .context("record scheduled job error")
    }
}

fn to_sql_count(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendKind;
    use crate::scheduled::{build_job, CreateScheduledJobRequest, ScheduleKind};
    use crate::session::{Location, SessionState};
    use rusqlite::Connection as SyncConnection;
    use std::path::Path;

    async fn mem_store() -> Store {
        Store::open(Path::new(":memory:")).await.unwrap()
    }

    fn make_session(id: &str, name: &str) -> Session {
        Session {
            id: id.to_string(),
            name: name.to_string(),
            backend: BackendKind::Claude,
            location: Location::Local,
            ssh_host: None,
            base_dir: "/tmp".to_string(),
            project_path: "/tmp/proj".to_string(),
            worktree: false,
            state: SessionState::Waiting,
            created_at: 1_000,
            last_activity: 2_000,
            supervisor: SupervisorKind::Direct,
            host_log_path: None,
            log_offset: 0,
            backend_session_id: None,
            parent_session_id: None,
        }
    }

    fn make_diff(n: i64) -> NewTurnDiff {
        NewTurnDiff {
            started_at: n * 10,
            completed_at: n * 10 + 5,
            files_changed: 2,
            additions: n.max(0) as u64,
            deletions: 1,
            truncated: false,
            patch: format!("diff {n}"),
        }
    }

    #[tokio::test]
    async fn insert_and_list() {
        let store = mem_store().await;
        let s = make_session("id1", "my-session");
        store.insert(&s).await.unwrap();
        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "id1");
        assert_eq!(list[0].name, "my-session");
        assert_eq!(list[0].state, SessionState::Waiting);
    }

    #[tokio::test]
    async fn get_returns_one_session_without_listing() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "first")).await.unwrap();
        store.insert(&make_session("id2", "second")).await.unwrap();

        assert_eq!(store.get("id2").await.unwrap().unwrap().name, "second");
        assert!(store.get("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_empty_returns_empty_vec() {
        let list = mem_store().await.list().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn update_state_changes_state_and_activity() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "s1")).await.unwrap();
        store
            .update_state("id1", SessionState::Active, 9_999)
            .await
            .unwrap();
        let list = store.list().await.unwrap();
        assert_eq!(list[0].state, SessionState::Active);
        assert_eq!(list[0].last_activity, 9_999);
    }

    #[tokio::test]
    async fn update_name_renames_session() {
        let store = mem_store().await;
        store
            .insert(&make_session("id1", "old-name"))
            .await
            .unwrap();
        store.update_name("id1", "new-name").await.unwrap();
        let list = store.list().await.unwrap();
        assert_eq!(list[0].name, "new-name");
    }

    #[tokio::test]
    async fn delete_removes_session() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "s1")).await.unwrap();
        store.insert(&make_session("id2", "s2")).await.unwrap();
        store.delete("id1").await.unwrap();
        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "id2");
    }

    #[tokio::test]
    async fn turn_diffs_roundtrip_and_are_scoped_to_their_session() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "first")).await.unwrap();
        store.insert(&make_session("id2", "second")).await.unwrap();
        let inserted = store
            .insert_turn_diff("id1", make_diff(7))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(inserted.turn, 1);
        assert_eq!(inserted.additions, 7);
        assert!(store.list_turn_diffs("id2").await.unwrap().is_empty());
        let detail = store
            .get_turn_diff("id1", inserted.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary, inserted);
        assert_eq!(detail.patch, "diff 7");
    }

    #[tokio::test]
    async fn turn_diff_history_is_bounded_and_removed_with_session() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "first")).await.unwrap();
        for n in 1..=TURN_DIFF_HISTORY_LIMIT + 2 {
            store.insert_turn_diff("id1", make_diff(n)).await.unwrap();
        }

        let turns = store.list_turn_diffs("id1").await.unwrap();
        assert_eq!(turns.len(), TURN_DIFF_HISTORY_LIMIT as usize);
        assert_eq!(turns.first().unwrap().turn, TURN_DIFF_HISTORY_LIMIT + 2);
        assert_eq!(turns.last().unwrap().turn, 3);

        store.delete("id1").await.unwrap();
        assert!(store.list_turn_diffs("id1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn queued_turn_diff_cannot_outlive_deleted_session() {
        let store = mem_store().await;
        assert!(store
            .insert_turn_diff("missing", make_diff(1))
            .await
            .unwrap()
            .is_none());
        assert!(store.list_turn_diffs("missing").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn scheduled_jobs_are_scoped_deferred_and_claimed_without_drift() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "first")).await.unwrap();
        store.insert(&make_session("id2", "second")).await.unwrap();
        let job = build_job(
            "id1",
            CreateScheduledJobRequest {
                title: "Poll".to_string(),
                prompt: "Check status".to_string(),
                schedule_kind: ScheduleKind::Interval,
                interval_seconds: Some(60),
                next_run_at: 100_000,
                enabled: true,
            },
            0,
        )
        .unwrap();
        store
            .insert_scheduled_job(job.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(store.list_scheduled_jobs("id1").await.unwrap().len(), 1);
        assert!(store.list_scheduled_jobs("id2").await.unwrap().is_empty());

        assert!(store
            .defer_scheduled_job(&job, 200_000, "busy", 150_000)
            .await
            .unwrap());
        assert!(store
            .due_scheduled_jobs(199_999, 10)
            .await
            .unwrap()
            .is_empty());
        let deferred = store
            .due_scheduled_jobs(200_000, 10)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(deferred.next_run_at, 100_000);
        assert_eq!(deferred.retry_at, Some(200_000));
        assert!(store.claim_scheduled_job(&deferred, 275_000).await.unwrap());

        let claimed = store
            .get_scheduled_job("id1", &job.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.next_run_at, 280_000);
        assert_eq!(claimed.retry_at, None);
        assert_eq!(claimed.run_count, 1);
        assert_eq!(claimed.last_run_at, Some(275_000));

        store.delete("id1").await.unwrap();
        assert!(store.list_scheduled_jobs("id1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn one_time_job_disables_after_its_occurrence_is_claimed() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "first")).await.unwrap();
        let job = build_job(
            "id1",
            CreateScheduledJobRequest {
                title: "Once".to_string(),
                prompt: "Run report".to_string(),
                schedule_kind: ScheduleKind::Once,
                interval_seconds: None,
                next_run_at: 10,
                enabled: true,
            },
            0,
        )
        .unwrap();
        store
            .insert_scheduled_job(job.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(store.claim_scheduled_job(&job, 10).await.unwrap());
        let claimed = store
            .get_scheduled_job("id1", &job.id)
            .await
            .unwrap()
            .unwrap();
        assert!(!claimed.enabled);
        assert_eq!(claimed.run_count, 1);
        assert!(store.next_scheduled_wake().await.unwrap().is_none());

        assert!(store
            .rollback_scheduled_job_claim(&job, 10, "terminal submission failed")
            .await
            .unwrap());
        let restored = store
            .get_scheduled_job("id1", &job.id)
            .await
            .unwrap()
            .unwrap();
        assert!(restored.enabled);
        assert_eq!(restored.run_count, 0);
        assert_eq!(restored.last_run_at, None);
        assert_eq!(
            restored.last_error.as_deref(),
            Some("terminal submission failed")
        );
    }

    #[tokio::test]
    async fn manual_run_consumes_an_already_due_occurrence_only_once() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "first")).await.unwrap();
        let job = build_job(
            "id1",
            CreateScheduledJobRequest {
                title: "Manual".to_string(),
                prompt: "Run it".to_string(),
                schedule_kind: ScheduleKind::Interval,
                interval_seconds: Some(60),
                next_run_at: 100_000,
                enabled: true,
            },
            0,
        )
        .unwrap();
        store
            .insert_scheduled_job(job.clone())
            .await
            .unwrap()
            .unwrap();

        assert!(store
            .claim_manual_scheduled_job(&job, 100_000)
            .await
            .unwrap());
        assert!(!store.claim_scheduled_job(&job, 100_000).await.unwrap());
        let current = store
            .get_scheduled_job("id1", &job.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.next_run_at, 160_000);
        assert_eq!(current.run_count, 1);
    }

    #[tokio::test]
    async fn list_ordered_by_last_activity_desc() {
        let store = mem_store().await;
        let mut s1 = make_session("id1", "older");
        s1.last_activity = 100;
        let mut s2 = make_session("id2", "newer");
        s2.last_activity = 200;
        store.insert(&s1).await.unwrap();
        store.insert(&s2).await.unwrap();
        let list = store.list().await.unwrap();
        assert_eq!(list[0].id, "id2");
        assert_eq!(list[1].id, "id1");
    }

    #[tokio::test]
    async fn session_preserves_all_fields() {
        let store = mem_store().await;
        let s = Session {
            id: "abc".to_string(),
            name: "remote-session".to_string(),
            backend: BackendKind::Codex,
            location: Location::Remote,
            ssh_host: Some("user@host".to_string()),
            base_dir: "/home/user".to_string(),
            project_path: "/home/user/proj".to_string(),
            worktree: true,
            state: SessionState::Stopped,
            created_at: 42,
            last_activity: 99,
            supervisor: SupervisorKind::Tmux,
            host_log_path: Some("/home/user/.local/share/slide/logs/abc.log".to_string()),
            log_offset: 12_345,
            backend_session_id: Some("claude-uuid-xyz".to_string()),
            parent_session_id: Some("source-session".to_string()),
        };
        store.insert(&s).await.unwrap();
        let list = store.list().await.unwrap();
        let got = &list[0];
        assert_eq!(got.backend, BackendKind::Codex);
        assert_eq!(got.location, Location::Remote);
        assert_eq!(got.ssh_host.as_deref(), Some("user@host"));
        assert!(got.worktree);
        assert_eq!(got.state, SessionState::Stopped);
        assert_eq!(got.created_at, 42);
        assert_eq!(got.supervisor, SupervisorKind::Tmux);
        assert_eq!(
            got.host_log_path.as_deref(),
            Some("/home/user/.local/share/slide/logs/abc.log"),
        );
        assert_eq!(got.log_offset, 12_345);
        assert_eq!(got.backend_session_id.as_deref(), Some("claude-uuid-xyz"));
        assert_eq!(got.parent_session_id.as_deref(), Some("source-session"));
    }

    #[tokio::test]
    async fn migration_from_legacy_preserves_rows_and_defaults_new_columns() {
        // Simulate a database on v0 schema (pre-migration framework): a
        // fresh SQLite file with only the v1-shape table and user_version=0.
        // We use the synchronous rusqlite::Connection here intentionally —
        // we're populating fixture data before opening through Store.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let conn = SyncConnection::open(path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            // user_version stays 0 — mimics a pre-framework install.
            conn.execute(
                "INSERT INTO sessions \
                 (id, name, backend, location, ssh_host, base_dir, project_path, worktree, state, created_at, last_activity) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    "legacy-id",
                    "legacy-session",
                    "claude",
                    "local",
                    Option::<String>::None,
                    "/tmp",
                    "/tmp/proj",
                    0,
                    "waiting",
                    1_000i64,
                    2_000i64,
                ],
            )
            .unwrap();
        }

        // Opening through Store should re-run v1 (no-op) and v2 (adds columns).
        let store = Store::open(path).await.unwrap();
        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 1);
        let got = &list[0];
        assert_eq!(got.id, "legacy-id");
        assert_eq!(got.name, "legacy-session");
        // New columns should default sensibly.
        assert_eq!(got.supervisor, SupervisorKind::Direct);
        assert!(got.host_log_path.is_none());
        assert_eq!(got.log_offset, 0);
        assert!(got.backend_session_id.is_none());
        assert!(got.parent_session_id.is_none());

        // user_version should now match the current schema.
        let conn = SyncConnection::open(path).unwrap();
        let v: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len() as u32);
    }

    #[tokio::test]
    async fn migration_v3_collapses_exited_and_archived_to_stopped() {
        // Stand up a v2-era DB with legacy state strings, then let Store
        // re-migrate. Both rows should come back as Stopped.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let conn = SyncConnection::open(path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(MIGRATIONS[1]).unwrap();
            conn.execute_batch("PRAGMA user_version = 2").unwrap();
            for (id, state) in [("exited-row", "exited"), ("archived-row", "archived")] {
                conn.execute(
                    "INSERT INTO sessions \
                     (id, name, backend, location, ssh_host, base_dir, project_path, worktree, state, created_at, last_activity, supervisor) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![id, id, "claude", "local", Option::<String>::None, "/tmp", "/tmp/p", 0, state, 1i64, 2i64, "direct"],
                )
                .unwrap();
            }
        }
        let store = Store::open(path).await.unwrap();
        for s in store.list().await.unwrap() {
            assert_eq!(s.state, SessionState::Stopped, "row {} not migrated", s.id);
        }
    }

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        let _ = Store::open(path).await.unwrap();
        let _ = Store::open(path).await.unwrap();
        let conn = SyncConnection::open(path).unwrap();
        let v: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len() as u32);
    }
}
