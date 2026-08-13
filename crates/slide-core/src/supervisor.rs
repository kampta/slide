//! Strategy for keeping a session's backend process alive.
//!
//! Two implementations ship:
//!
//! - [`DirectPtySupervisor`]: the daemon spawns the backend as its own
//!   child. When the daemon dies, so does the backend. Historical default.
//! - [`TmuxSupervisor`]: the backend runs inside a `tmux -L slide` session
//!   on the target host. The daemon later attaches to that session from a
//!   local PTY (or `ssh -t host tmux attach` for remote). Surviving daemon
//!   crashes and (for remote) laptop going away is exactly tmux's job.

use crate::session::{Location, Session, SupervisorKind};
use crate::tmux;
use anyhow::{bail, Result};
use async_trait::async_trait;
use std::path::PathBuf;
/// Run synchronous supervisor work off the async runtime. Every tmux/SSH
/// process invoked by the closure has its own hard timeout and output bounds
/// in `tmux::exec_tmux`, so awaiting this worker cannot leak a live command.
async fn spawn_blocking_result<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await?
}

/// Request to spawn a backend under a supervisor.
#[derive(Debug, Clone)]
pub struct SpawnReq {
    /// Session id (used to name the tmux session, log file, etc.).
    pub id: String,
    /// Argv for the backend (the thing that runs inside the supervisor).
    pub argv: Vec<String>,
    /// Environment overrides for the backend process only.
    pub env: Vec<(String, String)>,
    /// Working directory for the backend, on the host that runs it.
    pub cwd: PathBuf,
    /// Absolute path where captured output should land. Under tmux this
    /// is the target of `pipe-pane`; under the direct supervisor the
    /// daemon writes it as it reads from the PTY.
    pub log_path: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

/// What the daemon should do with the backend after the supervisor is set up.
#[derive(Debug, Clone)]
pub struct Spawned {
    /// argv the daemon should run inside a local PTY to get bidi I/O with
    /// the backend. For direct: the backend argv itself. For tmux:
    /// `tmux attach-session -t slide-<id>` (or its ssh-wrapped form).
    pub attach_argv: Vec<String>,
    /// Environment for the local attach process. Direct supervision starts
    /// the backend here; tmux already applied its environment in the pane.
    pub attach_env: Vec<(String, String)>,
    /// cwd for the attach process. For direct: the backend cwd. For tmux:
    /// any valid directory (tmux doesn't use it).
    pub attach_cwd: PathBuf,
    /// Who is responsible for persisting the session log to disk.
    pub writes_log: WritesLog,
}

/// Which side of the daemon boundary writes bytes to `log_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritesLog {
    /// The supervisor itself writes the log (tmux via `pipe-pane`).
    /// Daemon should NOT write or it will duplicate output.
    Supervisor,
    /// The daemon writes the log as it reads from the attach PTY.
    /// This is the historical direct-PTY behavior.
    Daemon,
}

#[async_trait]
pub trait Supervisor: Send + Sync {
    fn kind(&self) -> SupervisorKind;

    /// Start the backend under this supervisor. For tmux this creates the
    /// tmux session and sets up `pipe-pane`; for direct it's a no-op that
    /// just returns the original argv back.
    async fn spawn(&self, req: &SpawnReq) -> Result<Spawned>;

    /// Clean up everything this supervisor owns for `id`. Called on
    /// explicit kill/delete, not on archive.
    async fn teardown(&self, id: &str) -> Result<()>;
}

/// Direct-PTY supervisor: historical behavior. Daemon owns the backend
/// process directly; nothing to clean up or reattach to.
pub struct DirectPtySupervisor;

#[async_trait]
impl Supervisor for DirectPtySupervisor {
    fn kind(&self) -> SupervisorKind {
        SupervisorKind::Direct
    }

    async fn spawn(&self, req: &SpawnReq) -> Result<Spawned> {
        Ok(Spawned {
            attach_argv: req.argv.clone(),
            attach_env: req.env.clone(),
            attach_cwd: req.cwd.clone(),
            writes_log: WritesLog::Daemon,
        })
    }

    async fn teardown(&self, _id: &str) -> Result<()> {
        Ok(())
    }
}

/// Tmux supervisor: runs each session inside `tmux -L slide new-session -d`
/// on `host` (None = local). Survives daemon death and, for remote, client
/// disconnects.
pub struct TmuxSupervisor {
    /// SSH destination if the backend lives on a remote host. `None` runs
    /// tmux on the same machine as the daemon.
    pub host: Option<String>,
}

impl TmuxSupervisor {
    pub fn local() -> Self {
        Self { host: None }
    }

    pub fn remote(host: impl Into<String>) -> Self {
        Self {
            host: Some(host.into()),
        }
    }
}

#[async_trait]
impl Supervisor for TmuxSupervisor {
    fn kind(&self) -> SupervisorKind {
        SupervisorKind::Tmux
    }

    async fn spawn(&self, req: &SpawnReq) -> Result<Spawned> {
        // Log parent dir only matters locally. Remote hosts are expected
        // to have their own logs dir; tmux will create the log file on
        // first pipe-pane write.
        if self.host.is_none() {
            if let Some(parent) = req.log_path.parent() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
        }
        let host = self.host.clone();
        let id = req.id.clone();
        let cwd = req.cwd.clone();
        let argv = req.argv.clone();
        let env = req.env.clone();
        let log_path = req.log_path.clone();
        let cols = req.cols;
        let rows = req.rows;
        // tmux commands are cheap shell-outs; run them on a blocking thread
        // to avoid blocking the runtime and to keep the signatures sync.
        // Idempotent: if the tmux session is already alive (daemon restart,
        // resume path), we skip new-session and just re-establish pipe-pane.
        spawn_blocking_result(move || -> Result<()> {
            let host = host.as_deref();
            let backend_name = argv.first().cloned().unwrap_or_default();
            // Idempotent: if the tmux session is already alive (daemon
            // restart, resume path), skip new-session and just (re-)attach
            // pipe-pane. Fresh creates take the chained path so the
            // remote-SSH case pays one handshake instead of four.
            match tmux::has_session(host, &id)? {
                tmux::SessionProbe::Present => {
                    // The slide tmux server outlives daemon restarts, so an
                    // older slide build may have left it with `mouse off`
                    // (PRs #41/#42) or default drag bindings. Reapply the
                    // current policy idempotently before we hand the user
                    // a session that doesn't behave as documented.
                    tmux::setup_mouse(host).ok();
                    tmux::pipe_pane(host, &id, &log_path)
                        .map_err(|e| translate_dead_session_err(e, &backend_name))?;
                }
                tmux::SessionProbe::Absent => {
                    // start-server is folded in via the chained call so an
                    // empty server doesn't cause a misleading "no server
                    // running" if the backend exits immediately on exec —
                    // the same race the old separate-calls path papered
                    // over by ordering set-mouse before new-session.
                    tmux::create_session_with_log(
                        host,
                        &id,
                        &cwd,
                        tmux::PaneCommand {
                            argv: &argv,
                            env: &env,
                        },
                        cols,
                        rows,
                        &log_path,
                    )
                    .map_err(|e| translate_dead_session_err(e, &backend_name))?;
                }
                tmux::SessionProbe::Unreachable => {
                    // Don't blindly create-new on an unreachable host: we
                    // can't tell whether the session already exists, and
                    // create_session_with_log would fail next anyway with
                    // a less useful SSH error. Bail with a clear message.
                    bail!("ssh host unreachable — couldn't probe tmux session for {id}");
                }
            }
            Ok(())
        })
        .await?;

        Ok(Spawned {
            attach_argv: tmux::attach_argv(self.host.as_deref(), &req.id),
            attach_env: Vec::new(),
            // The attach process itself just needs a valid cwd; it doesn't
            // matter what since tmux owns the backend's real cwd.
            attach_cwd: dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")),
            writes_log: WritesLog::Supervisor,
        })
    }

    async fn teardown(&self, id: &str) -> Result<()> {
        let host = self.host.clone();
        let id = id.to_string();
        spawn_blocking_result(move || tmux::kill_session(host.as_deref(), &id)).await
    }
}

/// If a tmux error came from operating on a session that vanished (because
/// the backend exited on exec), rewrite it as a clearer user-facing message
/// that points at the likely cause instead of leaking tmux's internals.
fn translate_dead_session_err(err: anyhow::Error, backend: &str) -> anyhow::Error {
    let s = err.to_string();
    if s.contains("can't find session")
        || s.contains("no server running")
        || s.contains("session not found")
        || s.contains("no current target")
    {
        anyhow::anyhow!(
            "tmux session exited immediately after creation — backend `{}` likely failed to start (not on PATH, cwd missing, etc.)",
            backend,
        )
    } else {
        err
    }
}

/// Pick the best supervisor for a local session: tmux if available, else
/// direct-PTY.
pub fn local_supervisor() -> Box<dyn Supervisor> {
    if tmux::is_available() {
        Box::new(TmuxSupervisor::local())
    } else {
        Box::new(DirectPtySupervisor)
    }
}

/// Rebuild a supervisor from a persisted session row. Captures both the
/// supervisor kind and the ssh host so cold-start reattachment can reach
/// the right target.
pub fn for_session(s: &Session) -> Box<dyn Supervisor> {
    match s.supervisor {
        SupervisorKind::Direct => Box::new(DirectPtySupervisor),
        SupervisorKind::Tmux => match s.location {
            Location::Local => Box::new(TmuxSupervisor::local()),
            Location::Remote => match s.ssh_host.clone() {
                Some(h) => Box::new(TmuxSupervisor::remote(h)),
                // A tmux-supervised remote session without a host is a bug
                // — fall back to local so callers fail loudly rather than
                // silently talking to the wrong machine.
                None => Box::new(TmuxSupervisor::local()),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendKind;
    use crate::session::{Location, SessionState};

    fn local_session(supervisor: SupervisorKind) -> Session {
        Session {
            id: "abc".into(),
            name: "smoke".into(),
            backend: BackendKind::Claude,
            location: Location::Local,
            ssh_host: None,
            base_dir: "/tmp".into(),
            project_path: "/tmp/proj".into(),
            worktree: false,
            state: SessionState::Active,
            created_at: 1,
            last_activity: 1,
            supervisor,
            host_log_path: None,
            log_offset: 0,
            backend_session_id: None,
            parent_session_id: None,
        }
    }

    #[tokio::test]
    async fn direct_spawn_returns_argv_unchanged() {
        let sup = DirectPtySupervisor;
        let req = SpawnReq {
            id: "abc".into(),
            argv: vec!["claude".into(), "--arg".into()],
            env: vec![("SLIDE_TEST".into(), "value".into())],
            cwd: PathBuf::from("/tmp"),
            log_path: PathBuf::from("/tmp/abc.log"),
            cols: 80,
            rows: 24,
        };
        let s = sup.spawn(&req).await.unwrap();
        assert_eq!(s.attach_argv, req.argv);
        assert_eq!(s.attach_env, req.env);
        assert_eq!(s.attach_cwd, req.cwd);
        assert_eq!(s.writes_log, WritesLog::Daemon);
    }

    #[tokio::test]
    async fn tmux_spawn_and_teardown_roundtrip() {
        if !tmux::is_available() {
            eprintln!("tmux not on PATH; skipping");
            return;
        }
        let sup = TmuxSupervisor::local();
        let tmp = tempfile::tempdir().unwrap();
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let req = SpawnReq {
            id: id.clone(),
            argv: vec!["sleep".into(), "60".into()],
            env: Vec::new(),
            cwd: tmp.path().to_path_buf(),
            log_path: tmp.path().join(format!("{id}.log")),
            cols: 80,
            rows: 24,
        };
        let s = sup.spawn(&req).await.unwrap();
        assert_eq!(s.writes_log, WritesLog::Supervisor);
        assert_eq!(
            tmux::has_session(None, &id).unwrap(),
            tmux::SessionProbe::Present
        );
        assert_eq!(s.attach_argv[0], "tmux");

        sup.teardown(&id).await.unwrap();
        assert_eq!(
            tmux::has_session(None, &id).unwrap(),
            tmux::SessionProbe::Absent
        );
    }

    /// Spawn maps tmux's "can't find session" / "no server running" errors
    /// into a clearer "backend failed to start" message, so a backend that
    /// exited on exec doesn't show up as a confusing tmux-internals error.
    #[test]
    fn translate_dead_session_err_rewrites_missing_session() {
        let err = anyhow::anyhow!("tmux pipe-pane failed: can't find session: slide-abc");
        let out = translate_dead_session_err(err, "claude").to_string();
        assert!(out.contains("exited immediately"), "got: {out}");
        assert!(out.contains("claude"), "got: {out}");
    }

    #[test]
    fn translate_dead_session_err_rewrites_no_server() {
        let err = anyhow::anyhow!("tmux set-option mouse failed: no server running on /tmp/x");
        let out = translate_dead_session_err(err, "codex").to_string();
        assert!(out.contains("exited immediately"), "got: {out}");
    }

    #[test]
    fn translate_dead_session_err_passes_through_unrelated() {
        let err = anyhow::anyhow!("tmux new-session failed: some other problem");
        let out = translate_dead_session_err(err, "claude").to_string();
        // Unrelated errors must not be rewritten — we'd hide the real cause.
        assert!(out.contains("some other problem"), "got: {out}");
        assert!(!out.contains("exited immediately"), "got: {out}");
    }

    #[test]
    fn remote_tmux_attach_argv_is_ssh_wrapped() {
        let sup = TmuxSupervisor::remote("host.example");
        let argv = tmux::attach_argv(sup.host.as_deref(), "abc");
        assert_eq!(argv[0], "ssh");
        // Host position floats — multiplex options live between `-t` and
        // the host. Find it by value rather than fixed index.
        let host_idx = argv.iter().position(|a| a == "host.example").unwrap();
        assert!(argv[host_idx + 1].contains("slide-abc"));
    }

    #[test]
    fn local_supervisor_matches_tmux_availability() {
        let kind = local_supervisor().kind();
        let expected = if tmux::is_available() {
            SupervisorKind::Tmux
        } else {
            SupervisorKind::Direct
        };
        assert_eq!(kind, expected);
    }

    #[test]
    fn for_session_rebuilds_local_supervisor_from_persisted_row() {
        let kind = for_session(&local_session(SupervisorKind::Direct)).kind();
        assert_eq!(kind, SupervisorKind::Direct);

        let kind = for_session(&local_session(SupervisorKind::Tmux)).kind();
        assert_eq!(kind, SupervisorKind::Tmux);
    }
}
