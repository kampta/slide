use crate::backend::BackendKind;
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
    // v6 → v7: scheduled prompts were replaced by agent-native loop skills.
    r#"
    DROP TABLE IF EXISTS scheduled_jobs;
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

const SESSION_COLUMNS: &str =
    "id, name, backend, location, ssh_host, base_dir, project_path, worktree, state, \
     created_at, last_activity, supervisor, host_log_path, log_offset, backend_session_id, parent_session_id";
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

    /// Switch the session's LLM backend. Clears `backend_session_id` because
    /// provider conversation ids are not portable across backends — resume
    /// after a switch always starts a fresh conversation.
    pub async fn update_backend(&self, id: &str, backend: BackendKind) -> Result<()> {
        let id = id.to_string();
        let backend = backend.as_str().to_string();
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE sessions SET backend=?1, backend_session_id=NULL WHERE id=?2",
                    params![backend, id],
                )?;
                Ok(())
            })
            .await
            .context("update backend")?;
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
}

fn to_sql_count(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendKind;
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
    async fn update_backend_switches_backend_and_clears_provider_session_id() {
        let store = mem_store().await;
        let mut session = make_session("id1", "s1");
        session.backend = BackendKind::Claude;
        session.backend_session_id = Some("claude-uuid".to_string());
        store.insert(&session).await.unwrap();

        store
            .update_backend("id1", BackendKind::Codex)
            .await
            .unwrap();
        let got = store.get("id1").await.unwrap().unwrap();
        assert_eq!(got.backend, BackendKind::Codex);
        assert!(got.backend_session_id.is_none());
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
    async fn migration_v7_removes_scheduled_jobs() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let conn = SyncConnection::open(path).unwrap();
            for migration in &MIGRATIONS[..6] {
                conn.execute_batch(migration).unwrap();
            }
            conn.execute(
                "INSERT INTO scheduled_jobs
                 (id, session_id, title, prompt, schedule_kind, next_run_at, enabled, created_at, updated_at)
                 VALUES ('job', 'session', 'title', 'prompt', 'once', 1, 1, 1, 1)",
                [],
            )
            .unwrap();
            conn.execute_batch("PRAGMA user_version = 6").unwrap();
        }

        let _ = Store::open(path).await.unwrap();
        let conn = SyncConnection::open(path).unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='scheduled_jobs')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!exists);
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
