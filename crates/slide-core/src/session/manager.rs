use super::metadata::BackendMetadata;
use super::pty;
use super::recovery::RecoveryCoordinator;
use super::running::RunningSession;
use super::{
    CreateSessionRequest, ExecutionPolicy, ForkSessionRequest, HandoffRequest, Location, Session,
    SessionEvent, SessionState, SupervisorKind,
};
use crate::backend::{self, BackendKind, ContextUsage, SubagentList};
use crate::config;
use crate::git;
use crate::history;
use crate::runtime::{RuntimeDiagnosticsCache, RuntimeDiagnosticsSnapshot};
use crate::store::Store;
use crate::supervisor::{self, SpawnReq};
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use tokio::sync::{broadcast, mpsc, RwLock};

const BROADCAST_CAP: usize = 256;
const HANDOFF_TAIL_BYTES: usize = 32 * 1024;
const HANDOFF_CONTEXT_CHARS: usize = 8_000;

/// A browser-owned tmux client. Dropping it kills only the local attach
/// process (or SSH transport); the supervised tmux session keeps running.
pub struct TerminalAttachment {
    pty: pty::Pty,
    output: mpsc::Receiver<Bytes>,
}

impl TerminalAttachment {
    fn new(spawned: pty::Spawned) -> Self {
        Self {
            pty: spawned.pty,
            output: spawned.output,
        }
    }

    pub async fn recv(&mut self) -> Option<Bytes> {
        self.output.recv().await
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        self.pty.write(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.pty.resize(cols, rows)
    }
}

impl Drop for TerminalAttachment {
    fn drop(&mut self) {
        self.pty.kill();
    }
}

/// Reject ids that could escape `logs_dir` when joined as `{id}.log`, or
/// otherwise reach outside the caller's intent. All ids this daemon
/// generates are UUIDs (see `create`), so restricting to UUID-shaped
/// characters is strict enough to block `..`, absolute paths, NULs, and
/// path separators while allowing every legitimate id through.
pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub(super) fn check_id(id: &str) -> Result<()> {
    if !valid_id(id) {
        bail!("invalid session id");
    }
    Ok(())
}

fn validate_execution_policy(backend: BackendKind, policy: ExecutionPolicy) -> Result<()> {
    if backend.info().execution_policies.contains(&policy) {
        Ok(())
    } else {
        bail!(
            "{} does not support the {} execution policy",
            backend.as_str(),
            policy.as_str()
        )
    }
}

fn normalize_focus(value: Option<&str>, required: bool) -> Result<Option<String>> {
    let value = value.unwrap_or("");
    if value
        .chars()
        .any(|character| character.is_control() && !character.is_whitespace())
    {
        bail!("focus must not contain control characters");
    }
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > 2_000 {
        bail!("focus must be at most 2000 characters");
    }
    if compact.is_empty() {
        if required {
            bail!("focus is required");
        }
        Ok(None)
    } else {
        Ok(Some(compact))
    }
}

fn newest_chars(value: &str, limit: usize) -> &str {
    if limit == 0 {
        return "";
    }
    value
        .char_indices()
        .rev()
        .nth(limit)
        .map_or(value, |(index, character)| {
            &value[index + character.len_utf8()..]
        })
}

fn build_handoff_prompt(source_name: &str, focus: &str, context: &str) -> String {
    format!(
        "Slide handoff from session '{}'. Focus: {}. Recent source context: {}. Continue from this context, verify assumptions against the current workspace, and address the focus above.",
        source_name, focus, context
    )
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(super) fn next_activity(previous: i64) -> Result<i64> {
    let next = previous
        .checked_add(1)
        .context("session last_activity cannot advance")?;
    Ok(now_ms().max(next))
}

async fn remove_owned_worktree(session: &Session) -> Result<()> {
    let base = PathBuf::from(&session.base_dir);
    let worktree = PathBuf::from(&session.project_path);
    match session.location {
        Location::Local => {
            tokio::task::spawn_blocking(move || git::remove_worktree(&base, &worktree)).await??;
        }
        Location::Remote => {
            let host = session
                .ssh_host
                .clone()
                .context("remote worktree is missing ssh_host")?;
            tokio::task::spawn_blocking(move || {
                git::remove_remote_worktree(&host, &base, &worktree)
            })
            .await??;
        }
    }
    Ok(())
}

async fn rollback_owned_worktree(
    location: Location,
    ssh_host: Option<String>,
    base: PathBuf,
    worktree: PathBuf,
    name: String,
) -> Result<()> {
    match location {
        Location::Local => {
            tokio::task::spawn_blocking(move || git::rollback_worktree(&base, &worktree, &name))
                .await?;
        }
        Location::Remote => {
            let host = ssh_host.context("remote worktree is missing ssh_host")?;
            tokio::task::spawn_blocking(move || {
                git::rollback_remote_worktree(&host, &base, &worktree, &name)
            })
            .await??;
        }
    }
    Ok(())
}

async fn attach_process(
    sup: &dyn supervisor::Supervisor,
    session_id: &str,
    handoff: &supervisor::Spawned,
) -> Result<pty::Spawned> {
    let spawned = pty::spawn(
        &handoff.attach_argv,
        &handoff.attach_cwd,
        &handoff.attach_env,
    )
    .with_context(|| {
        format!(
            "spawn {} in {}",
            handoff.attach_argv.join(" "),
            handoff.attach_cwd.display()
        )
    });

    match spawned {
        Ok(spawned) => Ok(spawned),
        Err(error) => {
            if handoff.created_backend {
                if let Err(cleanup_error) = sup.teardown(session_id).await {
                    return Err(anyhow!(
                        "{error:#}; failed to tear down newly created supervisor backend: {cleanup_error:#}"
                    ));
                }
            }
            Err(error)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SpawnIntent {
    Fresh,
    Existing,
    Fork {
        provider_session_id: String,
        prompt: Option<String>,
    },
}

/// Default tmux window size for newly-created sessions. The daemon resizes
/// to the client's actual terminal size as soon as the first WS attaches.
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;

pub struct SessionManager {
    pub(super) store: Arc<Store>,
    pub(super) running: RwLock<HashMap<String, Arc<RunningSession>>>,
    // Broadcast carries `Arc<SessionEvent>`, not `SessionEvent`. Each event
    // ends up cloned once into the channel ring buffer plus once per
    // receiver on `recv()`; with N subscribers and a heap-heavy variant
    // like `SessionUpdated { session: Session { …many Strings… } }`, that
    // adds up. Wrapping in Arc collapses every clone to a refcount bump.
    events: broadcast::Sender<Arc<SessionEvent>>,
    /// Monotonic counter for per-WS client IDs. Only needs uniqueness
    /// per (session, client) pair, but a global counter is the simplest
    /// way to guarantee that. Wraps after 2^64 connections — never.
    next_client_id: AtomicU64,
    /// Serialize lifecycle mutations per session. This prevents two Resume
    /// requests from spawning duplicate attach PTYs without making a slow
    /// remote operation block unrelated sessions.
    operation_locks: StdMutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    pub(super) backend_metadata: Arc<BackendMetadata>,
    /// Cached runtime/toolchain health per local or SSH target. The same
    /// snapshot powers the diagnostics UI and create-time preflight, so
    /// health polling does not duplicate version/auth probes.
    runtime_diagnostics: Arc<RuntimeDiagnosticsCache>,
}

impl SessionManager {
    pub async fn new() -> Result<Arc<Self>> {
        config::ensure_dirs().context("create data dir")?;
        let store = Arc::new(Store::open(&config::db_path()).await?);
        let (events, _) = broadcast::channel(BROADCAST_CAP);
        let backend_metadata = BackendMetadata::new(store.clone(), events.clone());
        let mgr = Arc::new(Self {
            store,
            running: RwLock::new(HashMap::new()),
            events,
            next_client_id: AtomicU64::new(1),
            operation_locks: StdMutex::new(HashMap::new()),
            backend_metadata,
            runtime_diagnostics: Arc::new(RuntimeDiagnosticsCache::default()),
        });

        RecoveryCoordinator::reconcile(&mgr).await?;
        Ok(mgr)
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<Arc<SessionEvent>> {
        self.events.subscribe()
    }

    /// Broadcast a session lifecycle event. Wrapping at this seam keeps
    /// every emit site free of `Arc::new` ceremony and gives us one place
    /// to add metrics or filtering later.
    pub(super) fn emit(&self, ev: SessionEvent) {
        let _ = self.events.send(Arc::new(ev));
    }

    /// Persist a state transition and publish the exact timestamp SQLite kept.
    /// Keeping both operations at this seam prevents an event from claiming a
    /// timestamp that a newer database row has already superseded.
    pub(super) async fn persist_state_event(
        &self,
        id: &str,
        state: SessionState,
        last_activity: i64,
    ) -> Result<i64> {
        let last_activity = self.store.update_state(id, state, last_activity).await?;
        self.emit(SessionEvent::SessionState {
            id: id.to_string(),
            state,
            last_activity,
        });
        Ok(last_activity)
    }

    pub(super) async fn persist_classification(
        &self,
        id: &str,
        state: SessionState,
        last_activity: i64,
    ) {
        if let Err(error) = self.persist_state_event(id, state, last_activity).await {
            tracing::warn!(session = id, error = %format!("{error:#}"), "persist classification");
        }
    }

    pub(super) async fn persist_unattached_state(&self, session: &Session, state: SessionState) {
        if session.state == state {
            return;
        }
        let last_activity = match next_activity(session.last_activity) {
            Ok(last_activity) => last_activity,
            Err(error) => {
                tracing::warn!(session = %session.id, error = %format!("{error:#}"), "advance unattached state");
                return;
            }
        };
        if let Err(error) = self
            .persist_state_event(&session.id, state, last_activity)
            .await
        {
            tracing::warn!(session = %session.id, error = %format!("{error:#}"), "persist unattached state");
        }
    }

    pub(super) fn operation_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .operation_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(id.to_string(), Arc::downgrade(&lock));
        lock
    }

    async fn running_session(&self, id: &str) -> Option<Arc<RunningSession>> {
        self.running.read().await.get(id).cloned()
    }

    /// Atomically snapshot the in-memory ring and subscribe to live output.
    /// Used by `/ws/session/{id}` so reconnects don't drop bytes that arrive
    /// between the backfill read and the live subscription. Returns `None`
    /// when the session isn't running — callers can fall back to the on-disk
    /// log without worrying about a race because exited logs are immutable.
    pub async fn subscribe_output_with_snapshot(
        &self,
        id: &str,
    ) -> Option<(Vec<u8>, broadcast::Receiver<Bytes>)> {
        if !valid_id(id) {
            return None;
        }
        let r = self.running_session(id).await?;
        Some(r.subscribe_with_snapshot().await)
    }

    /// Open a fresh, correctly sized tmux client for one browser WebSocket.
    /// Direct sessions keep using the daemon-owned PTY and return `None`.
    pub async fn attach_terminal(
        &self,
        id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Option<TerminalAttachment>> {
        check_id(id)?;
        if cols == 0 || rows == 0 {
            bail!("terminal dimensions must be non-zero");
        }
        let Some(running) = self.running_session(id).await else {
            return Ok(None);
        };
        if !matches!(running.supervisor(), SupervisorKind::Tmux) {
            return Ok(None);
        }

        let session = self.find(id).await?;
        if let Some(host) = session.ssh_host.as_deref() {
            crate::ssh::validate_host(host)?;
        }
        let argv = crate::tmux::attach_argv(session.ssh_host.as_deref(), id);
        let cwd = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let spawned = pty::spawn_sized(&argv, &cwd, &[], cols, rows)
            .with_context(|| format!("attach terminal for session {id}"))?;
        Ok(Some(TerminalAttachment::new(spawned)))
    }

    pub async fn get_log(&self, id: &str) -> Result<Vec<u8>> {
        check_id(id)?;
        // Prefer the live in-memory ring. Once stopped, tmux requires the
        // rendered pane snapshot captured by explicit Stop; Direct retains
        // the existing bounded raw-log fallback.
        if let Some(r) = self.running_session(id).await {
            return Ok(r.snapshot().await);
        }
        let session = self.find(id).await?;
        let direct = matches!(session.supervisor, SupervisorKind::Direct);
        tokio::task::spawn_blocking(move || match history::read_stopped(&session) {
            Ok(bytes) => Ok(bytes),
            Err(error) if direct && error.downcast_ref::<std::io::Error>().is_some() => {
                Ok(Vec::new())
            }
            Err(error) => Err(error),
        })
        .await?
    }

    pub async fn list(&self) -> Result<Vec<Session>> {
        self.store.list().await
    }

    /// Read context usage from the backend's transcript for this session.
    /// Returns `None` when the session is unknown, remote (we'd need to SSH
    /// to the host that owns the transcript — deferred), has no discovered
    /// backend session id yet, or the backend has no transcript concept.
    pub async fn context_usage(&self, id: &str) -> Option<ContextUsage> {
        self.backend_metadata.context_usage(id).await
    }

    /// Fetch a sanitized child-agent snapshot from the provider. Provider
    /// calls are blocking subprocess I/O, so they run off Tokio's workers;
    /// a short success cache amortizes the query across attached browsers.
    pub async fn subagents(&self, id: &str) -> Result<SubagentList> {
        self.backend_metadata.subagents(id).await
    }

    pub async fn runtime_diagnostics(
        &self,
        host: Option<&str>,
        refresh: bool,
    ) -> Result<RuntimeDiagnosticsSnapshot> {
        let cache = self.runtime_diagnostics.clone();
        let host = host.map(str::to_string);
        tokio::task::spawn_blocking(move || cache.get(host.as_deref(), refresh))
            .await
            .context("join runtime diagnostics probe")?
    }

    async fn preflight_runtime(&self, backend: BackendKind, host: Option<&str>) -> Result<()> {
        let cache = self.runtime_diagnostics.clone();
        let host = host.map(str::to_string);
        tokio::task::spawn_blocking(move || cache.preflight(backend, host.as_deref()))
            .await
            .context("join runtime preflight")?
    }

    pub async fn write_input(&self, id: &str, bytes: &[u8]) -> Result<()> {
        check_id(id)?;
        let r = self
            .running_session(id)
            .await
            .context("session not running")?;
        r.write(bytes)?;
        Ok(())
    }

    /// Allocate a fresh per-WS client id. Pair every `set_client_size`
    /// with a matching `forget_client` on disconnect, otherwise the
    /// client's size will keep dragging the effective PTY size down
    /// after it's gone.
    pub fn next_client_id(&self) -> u64 {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Record `client_id`'s viewport size and resize the PTY to the
    /// minimum across all attached clients. Multiple clients share one
    /// PTY (it's a kernel resource); picking the min means the smallest
    /// viewport sees its full content while larger ones letterbox. Same
    /// strategy tmux uses for multiple attached clients.
    pub async fn set_client_size(
        &self,
        id: &str,
        client_id: u64,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        check_id(id)?;
        if cols == 0 || rows == 0 {
            bail!("terminal dimensions must be non-zero");
        }
        let r = self
            .running_session(id)
            .await
            .context("session not running")?;
        r.set_client_size(client_id, cols, rows).await
    }

    /// Drop a disconnected client's size and re-resize to the new min.
    /// Silently no-ops when the session isn't running or the client was
    /// never registered — matches the WS handler's "always call on
    /// cleanup" usage so we don't have to track presence client-side.
    pub async fn forget_client(&self, id: &str, client_id: u64) {
        if !valid_id(id) {
            return;
        }
        let Some(r) = self.running_session(id).await else {
            return;
        };
        r.forget_client(client_id).await;
    }

    pub async fn rename(&self, id: &str, new_name: &str) -> Result<Session> {
        check_id(id)?;
        git::validate_session_name(new_name)?;
        self.store.update_name(id, new_name).await?;
        let session = self.find(id).await?;
        self.emit(SessionEvent::SessionUpdated {
            session: session.clone(),
        });
        Ok(session)
    }

    pub async fn stop(&self, id: &str) -> Result<Session> {
        check_id(id)?;
        let operation = self.operation_lock(id);
        let _guard = operation.lock().await;
        // Tear down the supervised backend (tmux session, etc.) and mark the
        // session Stopped. Resume spawns a fresh backend that either
        // continues the prior conversation (via `--resume`) or starts new.
        let s = self.find(id).await?;
        if matches!(s.supervisor, SupervisorKind::Tmux) {
            let snapshot_session = s.clone();
            let snapshot = tokio::task::spawn_blocking(move || {
                let bytes = crate::tmux::capture_history(
                    snapshot_session.ssh_host.as_deref(),
                    &snapshot_session.id,
                )?;
                if bytes.is_empty() {
                    bail!("tmux session disappeared before history capture");
                }
                history::write_rendered(&snapshot_session.id, &bytes)
            })
            .await;
            if let Err(error) = snapshot
                .context("join rendered terminal history capture")
                .and_then(|result| result)
            {
                tracing::warn!(
                    session = id,
                    error = %format!("{error:#}"),
                    "capture rendered terminal history before stop"
                );
                if let Err(remove_error) = history::remove_rendered(id) {
                    tracing::warn!(
                        session = id,
                        error = %format!("{remove_error:#}"),
                        "invalidate stale rendered terminal history"
                    );
                }
            }
        }
        supervisor::for_session(&s).teardown(id).await?;
        self.kill_running(id).await;
        self.backend_metadata.cancel_discovery(id);
        // Classification is now quiescent. Advance from the latest row so
        // the terminal lifecycle update is both the last write and strictly
        // newer than every state event that preceded it.
        let last_activity = next_activity(self.find(id).await?.last_activity)?;
        self.store
            .update_state(id, SessionState::Stopped, last_activity)
            .await?;
        let session = self.find(id).await?;
        self.emit(SessionEvent::SessionUpdated {
            session: session.clone(),
        });
        Ok(session)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        check_id(id)?;
        let operation = self.operation_lock(id);
        let _guard = operation.lock().await;
        let session = self.find(id).await?;
        // Keep the database row when teardown fails. Losing the record while
        // a remote tmux session is still alive would orphan the backend.
        supervisor::for_session(&session).teardown(id).await?;
        self.kill_running(id).await;
        if session.worktree {
            remove_owned_worktree(&session).await?;
        }
        self.store.delete(id).await?;
        self.backend_metadata.clear_session(id).await;
        if let Err(error) = history::remove_rendered(id) {
            tracing::warn!(
                session = id,
                error = %format!("{error:#}"),
                "remove rendered terminal history after delete"
            );
        }
        match session.location {
            Location::Local => {
                let path = session
                    .host_log_path
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| config::logs_dir().join(format!("{id}.log")));
                let _ = tokio::fs::remove_file(path).await;
            }
            Location::Remote => {
                let _ =
                    tokio::task::spawn_blocking(move || history::remove_remote_log(&session)).await;
            }
        }
        self.emit(SessionEvent::SessionRemoved { id: id.to_string() });
        Ok(())
    }

    pub async fn create(self: &Arc<Self>, req: CreateSessionRequest) -> Result<Session> {
        self.create_with_intent(req, SpawnIntent::Fresh, None, None)
            .await
    }

    async fn create_with_intent(
        self: &Arc<Self>,
        mut req: CreateSessionRequest,
        intent: SpawnIntent,
        parent_session_id: Option<String>,
        worktree_source: Option<PathBuf>,
    ) -> Result<Session> {
        git::validate_session_name(&req.name)?;
        req.ssh_host = req
            .ssh_host
            .take()
            .map(|host| host.trim().to_string())
            .filter(|host| !host.is_empty());
        // Validate ssh_host early: it eventually ends up as an argv element
        // passed to `ssh`, and a leading `-` would be parsed as an option
        // (`-oProxyCommand=…` → arbitrary local code execution). Do this
        // before we touch the filesystem to keep error ordering clean.
        if let Some(host) = req.ssh_host.as_deref() {
            crate::ssh::validate_configured_host(host)?;
        }
        if matches!(req.location, Location::Remote) && req.ssh_host.is_none() {
            bail!("ssh_host is required for remote sessions");
        }
        validate_execution_policy(req.backend, req.execution_policy)?;
        // Fail before creating a branch/worktree when the selected runtime
        // cannot launch. This reuses the diagnostics cache populated by the
        // UI, so the common path is only an in-memory lookup.
        let diagnostic_host = req
            .ssh_host
            .as_deref()
            .filter(|_| matches!(req.location, Location::Remote));
        self.preflight_runtime(req.backend, diagnostic_host).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let base = PathBuf::from(&req.base_dir);
        let (project_path, worktree_owned) = match req.project_path.as_deref() {
            Some(p) if !p.trim().is_empty() => (PathBuf::from(p), false),
            _ => match req.location {
                Location::Local => {
                    let base = base.clone();
                    let name = req.name.clone();
                    let worktree = tokio::task::spawn_blocking(move || {
                        worktree_source.as_deref().map_or_else(
                            || git::add_worktree(&base, &name),
                            |source| git::add_worktree_from(&base, &name, source),
                        )
                    })
                    .await??;
                    (worktree, true)
                }
                Location::Remote => {
                    let host = req
                        .ssh_host
                        .clone()
                        .context("remote worktree is missing ssh_host")?;
                    let base = base.clone();
                    let name = req.name.clone();
                    let worktree = tokio::task::spawn_blocking(move || {
                        git::add_remote_worktree(&host, &base, &name)
                    })
                    .await??;
                    (worktree, true)
                }
            },
        };
        // Pick the supervisor strategy at create time so the row in SQLite
        // records how to reattach on cold start. Local sessions prefer tmux
        // when available; remote sessions optimistically use tmux (so the
        // backend survives the laptop going away) and surface errors at
        // spawn time if tmux isn't installed on the remote.
        let (supervisor_kind, host_log_path) = match req.location {
            Location::Local => {
                let kind = supervisor::local_supervisor().kind();
                let log_path = config::logs_dir().join(format!("{id}.log"));
                (kind, Some(log_path.to_string_lossy().into_owned()))
            }
            Location::Remote => {
                // Use a private per-user directory. `pipe-pane` also enforces
                // mode 0600 before appending so terminal output never inherits
                // a permissive remote umask.
                let remote_log = format!("~/.local/state/slide/logs/{id}.log");
                (SupervisorKind::Tmux, Some(remote_log))
            }
        };
        let now = now_ms();
        let session = Session {
            id: id.clone(),
            name: req.name,
            backend: req.backend,
            execution_policy: req.execution_policy,
            location: req.location,
            ssh_host: req.ssh_host,
            base_dir: base.to_string_lossy().into_owned(),
            project_path: project_path.to_string_lossy().into_owned(),
            worktree: worktree_owned,
            state: SessionState::Active,
            created_at: now,
            last_activity: now,
            supervisor: supervisor_kind,
            host_log_path,
            backend_session_id: None,
            parent_session_id,
        };
        if let Err(error) = self.store.insert(&session).await {
            if worktree_owned {
                let _ = rollback_owned_worktree(
                    session.location,
                    session.ssh_host.clone(),
                    base.clone(),
                    project_path.clone(),
                    session.name.clone(),
                )
                .await;
            }
            return Err(error);
        }
        if let Err(e) = self.spawn_process(&session, intent).await {
            // Roll back so a failed spawn (dead SSH, tmux missing on the
            // remote, …) doesn't leave the row Active in the sidebar — the
            // user would see a green session that hits "session not running"
            // on every WS attach. Also drop the worktree we just created so
            // retrying with the same name isn't blocked by a stale dir.
            if matches!(session.supervisor, SupervisorKind::Tmux) {
                let _ = supervisor::for_session(&session)
                    .teardown(&session.id)
                    .await;
            }
            let _ = self.store.delete(&session.id).await;
            self.runtime_diagnostics
                .record_launch_failure(session.backend, session.ssh_host.as_deref());
            if worktree_owned {
                let _ = rollback_owned_worktree(
                    session.location,
                    session.ssh_host.clone(),
                    base.clone(),
                    PathBuf::from(&session.project_path),
                    session.name.clone(),
                )
                .await;
            }
            return Err(e);
        }
        self.emit(SessionEvent::SessionAdded {
            session: session.clone(),
        });
        self.runtime_diagnostics
            .clear_launch_failure(session.backend, session.ssh_host.as_deref());
        Ok(session)
    }

    pub async fn fork_session(
        self: &Arc<Self>,
        source_id: &str,
        request: ForkSessionRequest,
    ) -> Result<Session> {
        check_id(source_id)?;
        let source = self.find(source_id).await?;
        if !matches!(source.location, Location::Local) {
            bail!("provider-native forks currently require a local source session");
        }
        let provider_session_id = source
            .backend_session_id
            .clone()
            .context("the source session has not exposed a provider conversation id yet")?;
        let prompt = normalize_focus(request.focus.as_deref(), false)?;
        if backend::for_kind(source.backend)
            .fork_argv(
                Path::new(&source.project_path),
                &provider_session_id,
                prompt.as_deref(),
            )
            .is_none()
        {
            bail!(
                "{} does not support provider-native forks",
                source.backend.as_str()
            );
        }
        let create = CreateSessionRequest {
            name: request.name,
            backend: source.backend,
            execution_policy: source.execution_policy,
            base_dir: source.base_dir.clone(),
            project_path: None,
            location: Location::Local,
            ssh_host: None,
        };
        let worktree_source = PathBuf::from(&source.project_path);
        self.create_with_intent(
            create,
            SpawnIntent::Fork {
                provider_session_id,
                prompt,
            },
            Some(source.id),
            Some(worktree_source),
        )
        .await
    }

    pub async fn handoff(&self, source_id: &str, request: HandoffRequest) -> Result<Session> {
        check_id(source_id)?;
        check_id(&request.target_session_id)?;
        if source_id == request.target_session_id {
            bail!("source and target sessions must be different");
        }
        let source = self.find(source_id).await?;
        let target = self.find(&request.target_session_id).await?;
        if !matches!(target.state, SessionState::Waiting) {
            bail!("target session must be waiting before a handoff");
        }
        let focus = normalize_focus(Some(&request.focus), true)?.context("focus is required")?;
        let running = self.running.read().await.get(source_id).cloned();
        let bytes = match running {
            Some(running) => running.tail(HANDOFF_TAIL_BYTES).await,
            None => {
                let source = source.clone();
                tokio::task::spawn_blocking(move || {
                    crate::history::read_tail(&source, HANDOFF_TAIL_BYTES)
                })
                .await
                .context("join handoff history read")??
            }
        };
        let compact = crate::terminal_text::compact(&String::from_utf8_lossy(&bytes));
        if compact.is_empty() {
            bail!("source session has no recent output to hand off");
        }
        let context = newest_chars(&compact, HANDOFF_CONTEXT_CHARS);
        let mut prompt = build_handoff_prompt(&source.name, &focus, context).into_bytes();
        prompt.push(b'\r');
        // Context collection can involve disk or SSH I/O. Re-check immediately
        // before sending so a target that became active in the meantime does not
        // receive an unsolicited turn.
        let operation = self.operation_lock(&target.id);
        let _guard = operation.lock().await;
        let target = self.find(&target.id).await?;
        if !matches!(target.state, SessionState::Waiting) {
            bail!("target session is no longer waiting");
        }
        self.write_input(&target.id, &prompt).await?;
        Ok(target)
    }

    /// Resume a stopped session. When `backend` differs from the stored
    /// backend, the session is switched to that backend first: the prior
    /// provider conversation id is cleared and the new process starts fresh
    /// in the same workspace (provider conversation ids are not portable).
    pub async fn resume(
        self: &Arc<Self>,
        id: &str,
        backend: Option<BackendKind>,
        execution_policy: Option<ExecutionPolicy>,
    ) -> Result<Session> {
        check_id(id)?;
        let operation = self.operation_lock(id);
        let _guard = operation.lock().await;
        let mut session = self.find(id).await?;
        // If already running, no-op unless the launch configuration changes.
        if self.running.read().await.contains_key(id) {
            if backend.is_some_and(|value| value != session.backend)
                || execution_policy.is_some_and(|value| value != session.execution_policy)
            {
                bail!("cannot change launch settings while session is running; stop it first");
            }
            return Ok(session);
        }
        let switch_backend = backend.filter(|b| *b != session.backend);
        let previous_session = session.clone();
        let requested_backend = switch_backend.unwrap_or(session.backend);
        // A backend switch without an explicit policy adopts that backend's
        // safe-to-describe default. Today all non-Codex backends support only
        // unrestricted execution, so retaining a Codex sandbox label would
        // be false and is never allowed.
        let requested_execution_policy = execution_policy.unwrap_or_else(|| {
            if switch_backend.is_some() {
                ExecutionPolicy::Unrestricted
            } else {
                session.execution_policy
            }
        });
        validate_execution_policy(requested_backend, requested_execution_policy)?;
        // Preflight before changing persisted provider identity. A missing or
        // unauthenticated runtime must leave the old resume target untouched.
        let diagnostic_host = session
            .ssh_host
            .as_deref()
            .filter(|_| matches!(session.location, Location::Remote));
        self.preflight_runtime(requested_backend, diagnostic_host)
            .await?;
        session.backend = requested_backend;
        session.execution_policy = requested_execution_policy;
        if switch_backend.is_some() {
            session.backend_session_id = None;
        }
        session.state = SessionState::Active;
        session.last_activity = next_activity(session.last_activity)?;
        self.store
            .begin_resume(&session, switch_backend.is_some())
            .await?;
        if switch_backend.is_some() {
            // Provider-scoped metadata is invalid after a backend switch.
            self.backend_metadata.clear_session(id).await;
        }
        // A backend switch always starts a new conversation; same-backend
        // resume keeps Existing so --resume / resume-latest still apply.
        let intent = if switch_backend.is_some() {
            SpawnIntent::Fresh
        } else {
            SpawnIntent::Existing
        };
        if let Err(e) = self.spawn_process(&session, intent).await {
            self.runtime_diagnostics
                .record_launch_failure(session.backend, session.ssh_host.as_deref());
            let mut restored = previous_session;
            restored.state = SessionState::Stopped;
            restored.last_activity = next_activity(session.last_activity)?;
            if let Err(rollback_error) = self
                .store
                .rollback_resume(&restored, switch_backend.is_some())
                .await
            {
                return Err(anyhow!(
                    "resume launch failed: {e:#}; failed to restore stopped session: {rollback_error:#}"
                ));
            }
            self.emit(SessionEvent::SessionUpdated { session: restored });
            return Err(e);
        }
        self.emit(SessionEvent::SessionUpdated {
            session: session.clone(),
        });
        self.runtime_diagnostics
            .clear_launch_failure(session.backend, session.ssh_host.as_deref());
        Ok(session)
    }

    pub(super) async fn find(&self, id: &str) -> Result<Session> {
        self.store
            .get(id)
            .await?
            .ok_or_else(|| anyhow!("unknown session {id}"))
    }

    async fn kill_running(&self, id: &str) {
        let running = self.running.write().await.remove(id);
        if let Some(r) = running {
            // Exit watchers retain an Arc<RunningSession> until the child reports
            // its status. Quiesce classification explicitly rather than waiting
            // for Drop so the terminal lifecycle write cannot be overtaken.
            r.kill().await;
        }
    }

    /// Best-effort drain on graceful daemon shutdown. Direct-supervised
    /// children are explicitly killed — without that they outlive us as
    /// orphans parented to PID 1 when the daemon is started without a
    /// controlling terminal. Tmux-supervised sessions are left running on
    /// purpose: they're detached from the daemon by design and survive
    /// across daemon restarts.
    pub async fn shutdown(&self) {
        let ids: Vec<String> = {
            let running = self.running.read().await;
            running
                .iter()
                .filter(|(_, session)| matches!(session.supervisor(), SupervisorKind::Direct))
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.kill_running(&id).await;
        }
    }

    /// Boxed because the exit watcher can recover a tmux attachment by
    /// spawning its replacement, which installs another exit watcher. Type
    /// erasure breaks that recursive async-future type while retaining the
    /// `Send` guarantee required by `tokio::spawn`.
    pub(super) fn spawn_process<'a>(
        self: &'a Arc<Self>,
        session: &'a Session,
        intent: SpawnIntent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            // Every backend launch starts a new terminal-generation. A
            // rendered snapshot belongs only to the stopped generation that
            // produced it, so remove it before attaching or spawning.
            if let Err(error) = history::remove_rendered(&session.id) {
                tracing::warn!(
                    session = %session.id,
                    error = %format!("{error:#}"),
                    "invalidate rendered terminal history before launch"
                );
            }
            let backend = backend::for_kind(session.backend);
            // Step 1: build the backend argv and the cwd *on the host that runs
            // the backend*. The supervisor is responsible for wrapping this in
            // whatever transport is needed (direct exec, tmux, ssh+tmux).
            let host_cwd = PathBuf::from(&session.project_path);
            // Existing sessions prefer a discovered provider-native id. A
            // backend may also offer a cwd-scoped latest-session fallback
            // (Codex `resume --last`) so remote conversations without a
            // locally-discovered id survive an explicit stop/resume.
            let backend_argv = match &intent {
                SpawnIntent::Fresh => backend.argv(&host_cwd),
                SpawnIntent::Existing => session
                    .backend_session_id
                    .as_deref()
                    .and_then(|session_id| backend.resume_argv(&host_cwd, session_id))
                    .or_else(|| backend.resume_latest_argv(&host_cwd))
                    .unwrap_or_else(|| backend.argv(&host_cwd)),
                SpawnIntent::Fork {
                    provider_session_id,
                    prompt,
                } => backend
                    .fork_argv(&host_cwd, provider_session_id, prompt.as_deref())
                    .context("backend does not support provider-native forks")?,
            };
            let backend_argv =
                backend.apply_execution_policy(session.execution_policy, backend_argv)?;
            let mut backend_env = backend.env();
            backend_env.push(("SLIDE_SESSION_ID".to_string(), session.id.clone()));

            if matches!(session.location, Location::Remote) && session.ssh_host.is_none() {
                bail!("remote session missing ssh_host");
            }

            // Step 2: hand the backend to its supervisor. For Direct this is a
            // no-op that returns the argv back; for Tmux this creates (or
            // reattaches to) the tmux session, locally or over SSH, and
            // returns the attach argv for our local PTY to run.
            let log_path = session
                .host_log_path
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| config::logs_dir().join(format!("{}.log", session.id)));
            let sup = supervisor::for_session(session);
            let launch_started = std::time::SystemTime::now();
            let spawn_req = SpawnReq {
                id: session.id.clone(),
                argv: backend_argv,
                env: backend_env,
                cwd: host_cwd.clone(),
                log_path: log_path.clone(),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
            };
            let handoff = sup.spawn(&spawn_req).await?;

            // Step 3: open a local PTY for the attach process. For Direct this
            // is just the backend itself; for Tmux it's `tmux attach-session`.
            let spawned = attach_process(sup.as_ref(), &session.id, &handoff).await?;
            let running = RunningSession::start(
                self.clone(),
                session,
                spawned.pty,
                spawned.output,
                handoff.writes_log,
                &log_path,
            )
            .await;
            self.running
                .write()
                .await
                .insert(session.id.clone(), running.clone());

            // Exit watcher. Under tmux, `spawned` is only the local attach
            // client; its exit may be a recoverable SSH transport loss rather
            // than the backend exiting.
            let id2 = session.id.clone();
            let mgr2 = self.clone();
            tokio::spawn(async move {
                let _ = spawned.exit.await;
                RecoveryCoordinator::handle_exit(mgr2, id2, running).await;
            });

            // Discover the backend's native session id so we can `--resume`
            // it if the supervisor is gone next time (remote reboot, user
            // killed the tmux session). Only runs for local sessions today —
            // remote discovery would need to scan the remote filesystem over
            // SSH, which we defer.
            self.backend_metadata
                .start_discovery(session, launch_started);

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        attach_process, build_handoff_prompt, newest_chars, next_activity, normalize_focus,
        valid_id, validate_execution_policy, BackendMetadata, RunningSession, SessionManager,
        TerminalAttachment, BROADCAST_CAP,
    };
    use crate::backend::BackendKind;
    use crate::runtime::RuntimeDiagnosticsCache;
    use crate::session::{
        ExecutionPolicy, Location, Session, SessionEvent, SessionState, SupervisorKind,
    };
    use crate::store::Store;
    use crate::supervisor::{SpawnReq, Spawned, Supervisor, WritesLog};
    use anyhow::Result;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tokio::sync::{broadcast, RwLock};

    #[derive(Default)]
    struct RecordingSupervisor {
        teardown_calls: AtomicUsize,
    }

    #[async_trait]
    impl Supervisor for RecordingSupervisor {
        fn kind(&self) -> SupervisorKind {
            SupervisorKind::Direct
        }

        async fn spawn(&self, _req: &SpawnReq) -> Result<Spawned> {
            unreachable!("attach_process tests do not spawn through the supervisor")
        }

        async fn teardown(&self, _id: &str) -> Result<()> {
            self.teardown_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[cfg(unix)]
    fn long_running_command() -> Vec<String> {
        vec!["sh".into(), "-c".into(), "sleep 30".into()]
    }

    #[cfg(windows)]
    fn long_running_command() -> Vec<String> {
        vec![
            "powershell.exe".into(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "Start-Sleep -Seconds 30".into(),
        ]
    }

    #[test]
    fn valid_id_accepts_uuid_shape() {
        assert!(valid_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(valid_id("abc_123"));
        assert!(valid_id("a"));
    }

    #[test]
    fn valid_id_rejects_path_traversal_and_separators() {
        // Any of these plugged into `logs_dir().join(format!("{id}.log"))`
        // would escape the logs directory or hit a surprising location.
        for bad in [
            "",
            "../etc/passwd",
            "..",
            "/etc/passwd",
            "foo/bar",
            "foo\\bar",
            "foo\0bar",
            "foo.bar",
            "foo bar",
            // Long enough to look like a path but still alphabetic — also
            // rejected once it passes 64 bytes, as a belt-and-braces cap.
            &"a".repeat(65),
        ] {
            assert!(!valid_id(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn sandboxed_auto_is_only_accepted_for_codex() {
        assert!(
            validate_execution_policy(BackendKind::Codex, ExecutionPolicy::SandboxedAuto,).is_ok()
        );
        assert!(
            validate_execution_policy(BackendKind::Claude, ExecutionPolicy::SandboxedAuto,)
                .is_err()
        );
    }

    #[test]
    fn lifecycle_activity_is_strictly_monotonic() {
        let prior = super::now_ms() + 10_000;
        let stopped = next_activity(prior).unwrap();
        let resumed = next_activity(stopped).unwrap();
        let rollback = next_activity(resumed).unwrap();

        assert!(prior < stopped && stopped < resumed && resumed < rollback);
        assert!(next_activity(i64::MAX).is_err());
    }

    #[test]
    fn handoff_focus_is_single_line_bounded_and_control_safe() {
        assert_eq!(
            normalize_focus(Some("  inspect\n  auth   failures  "), true).unwrap(),
            Some("inspect auth failures".to_string()),
        );
        assert!(normalize_focus(Some(" \t\n "), true).is_err());
        assert!(normalize_focus(Some("unsafe\u{1b}escape"), true).is_err());
        assert!(normalize_focus(Some(&"x".repeat(2_001)), false).is_err());
        assert_eq!(normalize_focus(None, false).unwrap(), None);
    }

    #[test]
    fn handoff_context_keeps_newest_unicode_without_allocating_a_copy() {
        assert_eq!(newest_chars("aé日🙂", 2), "日🙂");
        assert_eq!(newest_chars("short", 20), "short");
        assert_eq!(newest_chars("value", 0), "");
        let prompt = build_handoff_prompt("source", "check tests", "latest output");
        assert!(prompt.contains("session 'source'"));
        assert!(prompt.contains("Focus: check tests"));
        assert!(!prompt.contains('\n'));
    }

    #[tokio::test]
    async fn failed_attach_only_tears_down_a_new_supervisor_backend() {
        let cwd = tempfile::tempdir().unwrap();
        for (created_backend, expected_teardowns) in [(true, 1), (false, 0)] {
            let sup = RecordingSupervisor::default();
            let handoff = Spawned {
                attach_argv: Vec::new(),
                attach_env: Vec::new(),
                attach_cwd: PathBuf::from(cwd.path()),
                writes_log: WritesLog::Supervisor,
                created_backend,
            };

            let error = attach_process(&sup, "session", &handoff)
                .await
                .err()
                .expect("an empty attach argv must fail");
            assert!(format!("{error:#}").contains("empty argv"));
            assert_eq!(
                sup.teardown_calls.load(Ordering::Relaxed),
                expected_teardowns
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_terminal_attachment_kills_only_its_child() {
        let cwd = tempfile::tempdir().unwrap();
        let crate::session::pty::Spawned { pty, output, exit } =
            crate::session::pty::spawn(&long_running_command(), cwd.path(), &[]).unwrap();
        let attachment = TerminalAttachment { pty, output };

        drop(attachment);

        tokio::time::timeout(Duration::from_secs(2), exit)
            .await
            .expect("attachment child was not killed")
            .expect("attachment waiter stopped unexpectedly");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_fences_classifier_persistence_and_events() {
        let store = Arc::new(Store::open(Path::new(":memory:")).await.unwrap());
        let (event_tx, _) = broadcast::channel(BROADCAST_CAP);
        let manager = Arc::new(SessionManager {
            store: store.clone(),
            running: RwLock::new(HashMap::new()),
            events: event_tx.clone(),
            next_client_id: AtomicU64::new(1),
            operation_locks: StdMutex::new(HashMap::new()),
            backend_metadata: BackendMetadata::new(store, event_tx),
            runtime_diagnostics: Arc::new(RuntimeDiagnosticsCache::default()),
        });
        let cwd = tempfile::tempdir().unwrap();
        let initial_activity = super::now_ms();
        let session = Session {
            id: "classifier-stop-race".to_string(),
            name: "race".to_string(),
            backend: BackendKind::Claude,
            execution_policy: ExecutionPolicy::Unrestricted,
            location: Location::Local,
            ssh_host: None,
            base_dir: cwd.path().to_string_lossy().into_owned(),
            project_path: cwd.path().to_string_lossy().into_owned(),
            worktree: false,
            state: SessionState::Active,
            created_at: initial_activity,
            last_activity: initial_activity,
            supervisor: SupervisorKind::Direct,
            host_log_path: Some(
                cwd.path()
                    .join("session.log")
                    .to_string_lossy()
                    .into_owned(),
            ),
            backend_session_id: None,
            parent_session_id: None,
        };
        manager.store.insert(&session).await.unwrap();

        let crate::session::pty::Spawned {
            pty,
            output,
            exit: _exit,
        } = crate::session::pty::spawn(&long_running_command(), cwd.path(), &[]).unwrap();
        let running = RunningSession::start(
            manager.clone(),
            &session,
            pty,
            output,
            WritesLog::Daemon,
            &cwd.path().join("session.log"),
        )
        .await;
        manager
            .running
            .write()
            .await
            .insert(session.id.clone(), running.clone());

        let fence = running.classification_fence();
        // Model a classifier that passed the enabled check and owns the
        // commit gate before Stop starts.
        let commit = fence.hold_commit().await;
        let mut events = manager.subscribe_events();
        let stop_manager = manager.clone();
        let stop_id = session.id.clone();
        let stop = tokio::spawn(async move { stop_manager.stop(&stop_id).await });

        tokio::time::timeout(Duration::from_secs(1), async {
            while fence.is_enabled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stop did not disable classification");

        // This is the tail of the already-in-flight commit. Stop must wait
        // for its database write and event before publishing Stopped.
        let live_activity = super::now_ms() + 10_000;
        manager
            .persist_classification(&session.id, SessionState::Waiting, live_activity)
            .await;
        let live_event = events.recv().await.unwrap();
        assert!(matches!(
            &*live_event,
            SessionEvent::SessionState {
                state: SessionState::Waiting,
                last_activity,
                ..
            }
                if *last_activity == live_activity
        ));
        drop(commit);

        let stopped = tokio::time::timeout(Duration::from_secs(2), stop)
            .await
            .expect("stop remained blocked")
            .expect("stop task panicked")
            .expect("stop failed");
        assert_eq!(stopped.state, SessionState::Stopped);
        assert!(stopped.last_activity > live_activity);
        assert_eq!(
            manager.find(&session.id).await.unwrap().state,
            SessionState::Stopped
        );
        assert!(!manager.running.read().await.contains_key(&session.id));

        let stopped_event = events.recv().await.unwrap();
        assert!(matches!(
            &*stopped_event,
            SessionEvent::SessionUpdated { session }
                if session.state == SessionState::Stopped
                    && session.last_activity == stopped.last_activity
        ));

        // A classifier that was queued behind Stop observes the disabled
        // fence and cannot write or emit after Stop completes.
        assert!(
            !fence
                .persist(
                    &manager,
                    &session.id,
                    SessionState::Active,
                    stopped.last_activity + 1,
                )
                .await
        );
        let persisted = manager.find(&session.id).await.unwrap();
        assert_eq!(persisted.state, SessionState::Stopped);
        assert_eq!(persisted.last_activity, stopped.last_activity);
        assert!(matches!(
            events.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }
}
