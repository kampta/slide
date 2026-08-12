pub mod manager;
pub mod pty;
mod running;

use crate::backend::BackendKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Active,
    Waiting,
    /// The backend is running, but the rendered pane has no reliable working
    /// or input signal. Kept distinct from Active so uncertainty is visible.
    Unknown,
    /// Backend is not running. Covers both "process exited on its own" and
    /// "user stopped it" — previously split as Exited/Archived. Resume
    /// brings it back.
    Stopped,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Active => "active",
            SessionState::Waiting => "waiting",
            SessionState::Unknown => "unknown",
            SessionState::Stopped => "stopped",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "waiting" => Some(Self::Waiting),
            "unknown" => Some(Self::Unknown),
            // Migrate legacy names in case a row slips through without the
            // SQL migration (e.g. a test harness writing raw strings).
            "stopped" | "exited" | "archived" => Some(Self::Stopped),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    Local,
    Remote,
}

impl Location {
    pub fn as_str(self) -> &'static str {
        match self {
            Location::Local => "local",
            Location::Remote => "remote",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Self::Local),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }
}

/// Which supervisor keeps a session's backend process alive.
///
/// `Direct` is the historical behavior: the daemon spawns the backend as
/// its own child and loses it on daemon death. `Tmux` runs the backend
/// inside a `tmux -L slide` session on the target host so it survives
/// client disconnects. Populated by [`manager::SessionManager`] on spawn.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SupervisorKind {
    Direct,
    Tmux,
}

impl SupervisorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SupervisorKind::Direct => "direct",
            SupervisorKind::Tmux => "tmux",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "direct" => Some(Self::Direct),
            "tmux" => Some(Self::Tmux),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub backend: BackendKind,
    pub location: Location,
    pub ssh_host: Option<String>,
    pub base_dir: String,
    pub project_path: String,
    pub worktree: bool,
    pub state: SessionState,
    pub created_at: i64,
    pub last_activity: i64,
    /// Supervisor strategy for this session's backend process.
    #[serde(default = "default_supervisor")]
    pub supervisor: SupervisorKind,
    /// Absolute path to the output log on the host that runs the backend.
    /// Populated once the supervisor spawns; `None` on legacy rows.
    #[serde(default)]
    pub host_log_path: Option<String>,
    /// How many bytes of [`host_log_path`] the daemon has already consumed.
    #[serde(default)]
    pub log_offset: i64,
    /// Backend-native session id (e.g. claude's `~/.claude/projects/.../<uuid>.jsonl`),
    /// discovered after spawn. Enables `--resume` recovery when the
    /// supervisor is no longer around.
    #[serde(default)]
    pub backend_session_id: Option<String>,
    /// Slide session whose provider conversation was branched to create this
    /// session. Kept separate from the provider-native id so deletion or
    /// rediscovery never changes lineage.
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

fn default_supervisor() -> SupervisorKind {
    SupervisorKind::Direct
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateSessionRequest {
    pub name: String,
    pub backend: BackendKind,
    pub base_dir: String,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default = "default_local")]
    pub location: Location,
    #[serde(default)]
    pub ssh_host: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForkSessionRequest {
    pub name: String,
    #[serde(default)]
    pub focus: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HandoffRequest {
    pub target_session_id: String,
    pub focus: String,
}

fn default_local() -> Location {
    Location::Local
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionAdded { session: Session },
    SessionUpdated { session: Session },
    SessionRemoved { id: String },
    SessionState { id: String, state: SessionState },
    SessionExit { id: String, code: Option<i32> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_roundtrip() {
        let cases = [
            (SessionState::Active, "active"),
            (SessionState::Waiting, "waiting"),
            (SessionState::Unknown, "unknown"),
            (SessionState::Stopped, "stopped"),
        ];
        for (state, s) in cases {
            assert_eq!(state.as_str(), s);
            assert_eq!(SessionState::from_str(s), Some(state));
        }
    }

    #[test]
    fn session_state_legacy_names_collapse_to_stopped() {
        // Old databases may still hold "exited" / "archived" strings; both
        // should fold into Stopped on read.
        assert_eq!(
            SessionState::from_str("exited"),
            Some(SessionState::Stopped)
        );
        assert_eq!(
            SessionState::from_str("archived"),
            Some(SessionState::Stopped),
        );
    }

    #[test]
    fn session_state_invalid_returns_none() {
        assert_eq!(SessionState::from_str("busy"), None);
        assert_eq!(SessionState::from_str(""), None);
    }

    #[test]
    fn location_roundtrip() {
        let cases = [(Location::Local, "local"), (Location::Remote, "remote")];
        for (loc, s) in cases {
            assert_eq!(loc.as_str(), s);
            assert_eq!(Location::from_str(s), Some(loc));
        }
    }

    #[test]
    fn location_unknown_returns_none() {
        assert_eq!(Location::from_str("ssh"), None);
        assert_eq!(Location::from_str(""), None);
    }

    #[test]
    fn supervisor_kind_roundtrip() {
        let cases = [
            (SupervisorKind::Direct, "direct"),
            (SupervisorKind::Tmux, "tmux"),
        ];
        for (kind, s) in cases {
            assert_eq!(kind.as_str(), s);
            assert_eq!(SupervisorKind::from_str(s), Some(kind));
        }
    }

    #[test]
    fn supervisor_kind_unknown_returns_none() {
        assert_eq!(SupervisorKind::from_str("screen"), None);
        assert_eq!(SupervisorKind::from_str(""), None);
    }
}
