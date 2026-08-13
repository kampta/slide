use crate::backend::BackendKind;
use crate::session::{ExecutionPolicy, Location, Session, SessionState, SupervisorKind};
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
    // v7 → v8: remove automatic per-turn Git diff history.
    r#"
    DROP TABLE IF EXISTS turn_diffs;
    "#,
    // v8 → v9: durable process permission policy. Existing sessions retain
    // the historical unrestricted behavior.
    r#"
    ALTER TABLE sessions ADD COLUMN execution_policy TEXT NOT NULL DEFAULT 'unrestricted';
    "#,
    // v9 → v10: log readers use bounded tails directly; the old incremental
    // cursor was never consumed after that design was removed.
    r#"
    ALTER TABLE sessions DROP COLUMN log_offset;
    "#,
];

async fn migrate(conn: &Connection) -> Result<()> {
    conn.call(|conn| {
        let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let target = i as u32 + 1;
            if current < target {
                apply_migration(conn, sql, target)?;
            }
        }
        Ok(())
    })
    .await
    .context("apply migrations")?;
    Ok(())
}

fn apply_migration(
    conn: &mut rusqlite::Connection,
    sql: &str,
    target: u32,
) -> rusqlite::Result<()> {
    let transaction = conn.transaction()?;
    transaction.execute_batch(sql)?;
    transaction.pragma_update(None, "user_version", target)?;
    transaction.commit()
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
        execution_policy: ExecutionPolicy::from_str(&r.get::<_, String>(15)?)
            .unwrap_or(ExecutionPolicy::Unrestricted),
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
        backend_session_id: r.get(13)?,
        parent_session_id: r.get(14)?,
    })
}

const SESSION_COLUMNS: &str =
    "id, name, backend, location, ssh_host, base_dir, project_path, worktree, state, \
     created_at, last_activity, supervisor, host_log_path, backend_session_id, parent_session_id, execution_policy";
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
                      supervisor, host_log_path, backend_session_id, parent_session_id, execution_policy) \
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
                        s.backend_session_id,
                        s.parent_session_id,
                        s.execution_policy.as_str(),
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
    ) -> Result<i64> {
        let id = id.to_string();
        let persisted = self
            .conn
            .call(move |c| {
                Ok(c.query_row(
                    "UPDATE sessions
                     SET state=?1, last_activity=MAX(last_activity, ?2)
                     WHERE id=?3
                     RETURNING last_activity",
                    params![state.as_str(), last_activity, id],
                    |row| row.get(0),
                )?)
            })
            .await
            .context("update state")?;
        Ok(persisted)
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

    /// Persist the active resume transition in one statement. Same-backend
    /// resumes preserve any provider id written concurrently by discovery;
    /// backend switches clear it because provider ids are not portable.
    pub async fn begin_resume(
        &self,
        session: &Session,
        clear_backend_session_id: bool,
    ) -> Result<()> {
        let session = session.clone();
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE sessions
                     SET backend=?1, execution_policy=?2,
                         backend_session_id=CASE WHEN ?3 THEN NULL ELSE backend_session_id END,
                         state=?4, last_activity=?5
                     WHERE id=?6",
                    params![
                        session.backend.as_str(),
                        session.execution_policy.as_str(),
                        clear_backend_session_id,
                        session.state.as_str(),
                        session.last_activity,
                        session.id,
                    ],
                )?;
                Ok(())
            })
            .await
            .context("begin resume")?;
        Ok(())
    }

    /// Restore the stopped launch snapshot after resume fails. A backend
    /// switch restores its prior provider id; a same-backend rollback leaves
    /// that id untouched because the transition never changed it.
    pub async fn rollback_resume(
        &self,
        session: &Session,
        restore_backend_session_id: bool,
    ) -> Result<()> {
        let session = session.clone();
        self.conn
            .call(move |c| {
                c.execute(
                    "UPDATE sessions
                     SET backend=?1, execution_policy=?2,
                         backend_session_id=CASE WHEN ?3 THEN ?4 ELSE backend_session_id END,
                         state=?5, last_activity=?6
                     WHERE id=?7",
                    params![
                        session.backend.as_str(),
                        session.execution_policy.as_str(),
                        restore_backend_session_id,
                        session.backend_session_id,
                        session.state.as_str(),
                        session.last_activity,
                        session.id,
                    ],
                )?;
                Ok(())
            })
            .await
            .context("rollback resume")?;
        Ok(())
    }

    /// Record the backend-native session id (e.g. claude's `--resume`
    /// target) once discovery has found it.
    pub async fn set_backend_session_id_if_current(
        &self,
        id: &str,
        backend: BackendKind,
        backend_session_id: &str,
    ) -> Result<bool> {
        let id = id.to_string();
        let backend = backend.as_str().to_string();
        let backend_session_id = backend_session_id.to_string();
        let updated = self
            .conn
            .call(move |c| {
                Ok(c.execute(
                    "UPDATE sessions SET backend_session_id=?1
                     WHERE id=?2 AND backend=?3 AND backend_session_id IS NULL",
                    params![backend_session_id, id, backend],
                )?)
            })
            .await
            .context("update backend session id")?;
        Ok(updated > 0)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.conn
            .call(move |c| {
                c.execute("DELETE FROM sessions WHERE id=?1", params![id])?;
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

    #[test]
    fn failed_migration_rolls_back_schema_and_version_together() {
        let mut conn = SyncConnection::open_in_memory().unwrap();
        apply_migration(&mut conn, "CREATE TABLE stable (id INTEGER);", 1).unwrap();

        let error = apply_migration(
            &mut conn,
            "CREATE TABLE partial (id INTEGER); THIS IS NOT SQL;",
            2,
        );
        assert!(error.is_err());

        let partial_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='partial')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert!(!partial_exists);
        assert_eq!(version, 1);
    }

    fn make_session(id: &str, name: &str) -> Session {
        Session {
            id: id.to_string(),
            name: name.to_string(),
            backend: BackendKind::Claude,
            execution_policy: ExecutionPolicy::Unrestricted,
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
            backend_session_id: None,
            parent_session_id: None,
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
    async fn update_state_changes_state_without_decreasing_activity() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "s1")).await.unwrap();

        let unchanged = store
            .update_state("id1", SessionState::Active, 1_000)
            .await
            .unwrap();
        assert_eq!(unchanged, 2_000);
        let session = store.get("id1").await.unwrap().unwrap();
        assert_eq!(session.state, SessionState::Active);
        assert_eq!(session.last_activity, 2_000);

        let advanced = store
            .update_state("id1", SessionState::Active, 9_999)
            .await
            .unwrap();
        assert_eq!(advanced, 9_999);
        assert_eq!(
            store.get("id1").await.unwrap().unwrap().last_activity,
            9_999,
        );
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
    async fn resume_transition_and_rollback_are_complete_snapshots() {
        let store = mem_store().await;
        let mut session = make_session("id1", "s1");
        session.backend = BackendKind::Claude;
        session.backend_session_id = Some("claude-uuid".to_string());
        store.insert(&session).await.unwrap();

        let previous = session.clone();
        session.backend = BackendKind::Codex;
        session.execution_policy = ExecutionPolicy::SandboxedAuto;
        session.backend_session_id = None;
        session.state = SessionState::Active;
        session.last_activity = 3_000;
        store.begin_resume(&session, true).await.unwrap();
        let got = store.get("id1").await.unwrap().unwrap();
        assert_eq!(got.backend, BackendKind::Codex);
        assert_eq!(got.execution_policy, ExecutionPolicy::SandboxedAuto);
        assert!(got.backend_session_id.is_none());
        assert_eq!(got.state, SessionState::Active);
        assert_eq!(got.last_activity, 3_000);

        let mut rollback = previous;
        rollback.state = SessionState::Stopped;
        rollback.last_activity = 4_000;
        store.rollback_resume(&rollback, true).await.unwrap();
        let restored = store.get("id1").await.unwrap().unwrap();
        assert_eq!(restored.backend, BackendKind::Claude);
        assert_eq!(restored.execution_policy, ExecutionPolicy::Unrestricted);
        assert_eq!(restored.backend_session_id.as_deref(), Some("claude-uuid"));
        assert_eq!(restored.state, SessionState::Stopped);
        assert_eq!(restored.last_activity, 4_000);
    }

    #[tokio::test]
    async fn same_backend_resume_preserves_provider_id_from_store() {
        let store = mem_store().await;
        let mut persisted = make_session("id1", "s1");
        persisted.backend = BackendKind::Codex;
        persisted.backend_session_id = Some("codex-thread".into());
        store.insert(&persisted).await.unwrap();

        let mut stale_snapshot = persisted;
        stale_snapshot.backend_session_id = None;
        stale_snapshot.state = SessionState::Active;
        store.begin_resume(&stale_snapshot, false).await.unwrap();

        let resumed = store.get("id1").await.unwrap().unwrap();
        assert_eq!(resumed.backend_session_id.as_deref(), Some("codex-thread"));

        stale_snapshot.state = SessionState::Stopped;
        store.rollback_resume(&stale_snapshot, false).await.unwrap();
        let rolled_back = store.get("id1").await.unwrap().unwrap();
        assert_eq!(
            rolled_back.backend_session_id.as_deref(),
            Some("codex-thread")
        );
        assert_eq!(rolled_back.state, SessionState::Stopped);
    }

    #[tokio::test]
    async fn failed_resume_transition_changes_no_resume_fields() {
        let store = mem_store().await;
        let mut initial = make_session("id1", "s1");
        initial.backend_session_id = Some("claude-uuid".into());
        store.insert(&initial).await.unwrap();
        store
            .conn
            .call(|connection| {
                connection.execute_batch(
                    "CREATE TRIGGER reject_launch_update
                     BEFORE UPDATE OF backend, execution_policy, backend_session_id, state, last_activity
                     ON sessions BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let mut transition = initial.clone();
        transition.backend = BackendKind::Codex;
        transition.execution_policy = ExecutionPolicy::SandboxedAuto;
        transition.backend_session_id = None;
        transition.state = SessionState::Active;
        transition.last_activity = 3_000;
        assert!(store.begin_resume(&transition, true).await.is_err());

        let unchanged = store.get("id1").await.unwrap().unwrap();
        assert_eq!(unchanged.backend, initial.backend);
        assert_eq!(unchanged.execution_policy, initial.execution_policy);
        assert_eq!(unchanged.backend_session_id, initial.backend_session_id);
        assert_eq!(unchanged.state, initial.state);
        assert_eq!(unchanged.last_activity, initial.last_activity);
    }

    #[tokio::test]
    async fn provider_session_id_only_updates_matching_backend() {
        let store = mem_store().await;
        store.insert(&make_session("id1", "session")).await.unwrap();

        assert!(store
            .set_backend_session_id_if_current("id1", BackendKind::Claude, "claude-id")
            .await
            .unwrap());
        let mut switched = store.get("id1").await.unwrap().unwrap();
        switched.backend = BackendKind::Codex;
        switched.backend_session_id = None;
        store.begin_resume(&switched, true).await.unwrap();
        assert!(!store
            .set_backend_session_id_if_current("id1", BackendKind::Claude, "stale-id")
            .await
            .unwrap());

        let session = store.get("id1").await.unwrap().unwrap();
        assert_eq!(session.backend, BackendKind::Codex);
        assert_eq!(session.backend_session_id, None);
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
            execution_policy: ExecutionPolicy::SandboxedAuto,
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
            backend_session_id: Some("claude-uuid-xyz".to_string()),
            parent_session_id: Some("source-session".to_string()),
        };
        store.insert(&s).await.unwrap();
        let list = store.list().await.unwrap();
        let got = &list[0];
        assert_eq!(got.backend, BackendKind::Codex);
        assert_eq!(got.execution_policy, ExecutionPolicy::SandboxedAuto);
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
        assert!(got.backend_session_id.is_none());
        assert!(got.parent_session_id.is_none());
        assert_eq!(got.execution_policy, ExecutionPolicy::Unrestricted);

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
    async fn migration_v8_removes_turn_diffs() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let conn = SyncConnection::open(path).unwrap();
            for migration in &MIGRATIONS[..7] {
                conn.execute_batch(migration).unwrap();
            }
            conn.execute(
                "INSERT INTO turn_diffs
                 (session_id, turn, started_at, completed_at, files_changed, additions, deletions, patch)
                 VALUES ('session', 1, 1, 2, 1, 1, 0, 'diff')",
                [],
            )
            .unwrap();
            conn.execute_batch("PRAGMA user_version = 7").unwrap();
        }

        let _ = Store::open(path).await.unwrap();
        let conn = SyncConnection::open(path).unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='turn_diffs')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!exists);
    }

    #[tokio::test]
    async fn migration_v9_preserves_unrestricted_behavior() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let conn = SyncConnection::open(path).unwrap();
            for migration in &MIGRATIONS[..8] {
                conn.execute_batch(migration).unwrap();
            }
            conn.execute(
                "INSERT INTO sessions
                 (id, name, backend, location, base_dir, project_path, worktree, state,
                  created_at, last_activity, supervisor)
                 VALUES ('existing', 'existing', 'codex', 'local', '/tmp', '/tmp/p',
                         0, 'stopped', 1, 2, 'direct')",
                [],
            )
            .unwrap();
            conn.execute_batch("PRAGMA user_version = 8").unwrap();
        }

        let store = Store::open(path).await.unwrap();
        let session = store.get("existing").await.unwrap().unwrap();
        assert_eq!(session.execution_policy, ExecutionPolicy::Unrestricted);
    }

    #[tokio::test]
    async fn migration_v10_removes_unused_log_offset() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        {
            let conn = SyncConnection::open(path).unwrap();
            for migration in &MIGRATIONS[..9] {
                conn.execute_batch(migration).unwrap();
            }
            conn.execute_batch("PRAGMA user_version = 9").unwrap();
        }

        let _ = Store::open(path).await.unwrap();
        let conn = SyncConnection::open(path).unwrap();
        let has_log_offset = conn
            .prepare("PRAGMA table_info(sessions)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .iter()
            .any(|column| column == "log_offset");
        assert!(!has_log_offset);
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
