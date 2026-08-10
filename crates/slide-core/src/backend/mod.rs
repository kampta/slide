use crate::classifier::Signals;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::SystemTime;

mod claude;
mod codex;
mod codex_subagents;

pub use claude::ClaudeBackend;
pub use codex::CodexBackend;

/// Per-turn context snapshot for a backend, read from the transcript the
/// backend writes to disk. `used_tokens` is what the model ingested on the
/// last assistant turn (input + cache reads + cache creations); dividing by
/// `window` gives the "% context used" chip in the UI.
#[derive(Debug, Clone, Serialize)]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub window: u64,
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
}

/// A privacy-bounded view of a backend child agent. Provider prompts, tool
/// arguments, command output, and transcript paths deliberately never cross
/// this boundary; the dock only needs identity, hierarchy, and lifecycle.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SubagentSnapshot {
    pub id: String,
    pub parent_id: String,
    pub name: Option<String>,
    pub role: Option<String>,
    pub state: SubagentState,
    /// Unix timestamp in seconds, matching the provider's thread metadata.
    pub created_at: i64,
    /// Unix timestamp in seconds, matching the provider's thread metadata.
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Starting,
    Running,
    Waiting,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SubagentList {
    pub supported: bool,
    pub agents: Vec<SubagentSnapshot>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct BackendInfo {
    pub id: BackendKind,
    pub label: &'static str,
    pub context_usage: bool,
    pub subagents: bool,
}

impl BackendKind {
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(BackendKind::Claude),
            "codex" => Some(BackendKind::Codex),
            _ => None,
        }
    }

    pub fn info(self) -> BackendInfo {
        match self {
            Self::Claude => BackendInfo {
                id: self,
                label: "Claude",
                context_usage: true,
                subagents: false,
            },
            Self::Codex => BackendInfo {
                id: self,
                label: "Codex",
                context_usage: false,
                subagents: true,
            },
        }
    }
}

pub fn available() -> Vec<BackendInfo> {
    BackendKind::ALL
        .into_iter()
        .map(BackendKind::info)
        .collect()
}

pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn argv(&self, cwd: &Path) -> Vec<String>;

    /// The patterns this backend exposes for session-state classification.
    /// See [`crate::classifier`] for how they combine into Active/Waiting.
    /// One `Signals` per backend, built lazily into a `OnceLock`, so calls
    /// are cheap and the regex compile cost is paid once.
    fn signals(&self) -> &'static Signals;

    /// argv that re-enters a previously-started backend conversation. Used
    /// when the supervisor is gone but the backend has its own durable
    /// transcript on disk (e.g. `claude --resume <id>`). `None` means the
    /// backend has no resume story and a fresh session must be started.
    fn resume_argv(&self, _cwd: &Path, _session_id: &str) -> Option<Vec<String>> {
        None
    }

    /// Scan the backend's transcript directory on the host where it runs
    /// for the newest session file whose mtime is after `since`. Returns
    /// the file's session id (its stem), or `None` if no matching file
    /// exists. Only meaningful on the host that owns the transcripts —
    /// callers running remotely must run this over SSH.
    fn discover_session_id(&self, _cwd: &Path, _since: SystemTime) -> Option<String> {
        None
    }

    /// Read the latest turn's context usage from the backend's transcript.
    /// Returns `None` when the backend has no transcript, the session id
    /// hasn't been discovered yet, or no assistant turn has been recorded.
    fn read_context_usage(&self, _cwd: &Path, _session_id: &str) -> Option<ContextUsage> {
        None
    }

    /// Return a bounded, sanitized snapshot of descendants spawned by this
    /// backend session. `ssh_host` is present when the provider and its
    /// metadata live on a remote Slide host. Backends without a structured
    /// child-agent API return `Ok(None)` so the frontend can hide the dock.
    fn read_subagents(
        &self,
        _cwd: &Path,
        _session_id: &str,
        _ssh_host: Option<&str>,
    ) -> Result<Option<Vec<SubagentSnapshot>>> {
        Ok(None)
    }
}

pub fn for_kind(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Claude => Box::new(ClaudeBackend::new()),
        BackendKind::Codex => Box::new(CodexBackend::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn backend_kind_roundtrip() {
        let cases = [
            (BackendKind::Claude, "claude"),
            (BackendKind::Codex, "codex"),
        ];
        for (kind, s) in cases {
            assert_eq!(kind.as_str(), s);
            assert_eq!(BackendKind::from_str(s), Some(kind));
        }
    }

    #[test]
    fn backend_kind_unknown_returns_none() {
        assert_eq!(BackendKind::from_str("gpt"), None);
        assert_eq!(BackendKind::from_str(""), None);
    }

    #[test]
    fn claude_backend_argv_starts_with_claude() {
        let b = for_kind(BackendKind::Claude);
        let argv = b.argv(Path::new("/some/path"));
        assert!(!argv.is_empty());
        assert_eq!(argv[0], "claude");
        assert_eq!(b.kind(), BackendKind::Claude);
    }

    #[test]
    fn codex_backend_argv_starts_with_codex() {
        let b = for_kind(BackendKind::Codex);
        let argv = b.argv(Path::new("/some/path"));
        assert!(!argv.is_empty());
        assert_eq!(argv[0], "codex");
        assert_eq!(b.kind(), BackendKind::Codex);
    }

    /// Smoke test: every backend ships a non-empty `Signals` bundle with a
    /// sensible settle window. Per-pattern assertions live in each backend's
    /// own module.
    #[test]
    fn every_backend_exposes_signals() {
        for kind in BackendKind::ALL {
            let b = for_kind(kind);
            let s = b.signals();
            assert!(
                !s.prompt.is_empty() || !s.idle_hints.is_empty(),
                "{kind:?} has no way to signal Waiting",
            );
            assert!(s.settle_ms > 0, "{kind:?} settle_ms must be > 0");
        }
    }

    #[test]
    fn every_backend_has_runtime_metadata() {
        let available = available();
        assert_eq!(available.len(), BackendKind::ALL.len());
        assert!(available.iter().all(|backend| !backend.label.is_empty()));
    }
}
