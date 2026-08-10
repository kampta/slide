use crate::classifier::Signals;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::SystemTime;

mod agy;
mod claude;
mod codex;
mod grok;
mod opencode;

pub use agy::AgyBackend;
pub use claude::ClaudeBackend;
pub use codex::CodexBackend;
pub use grok::GrokBackend;
pub use opencode::OpenCodeBackend;

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Claude,
    Codex,
    Grok,
    #[serde(rename = "agy", alias = "antigravity")]
    Antigravity,
    OpenCode,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct BackendInfo {
    pub id: BackendKind,
    pub label: &'static str,
    pub context_usage: bool,
}

impl BackendKind {
    pub const ALL: [Self; 5] = [
        Self::Claude,
        Self::Codex,
        Self::Grok,
        Self::Antigravity,
        Self::OpenCode,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
            BackendKind::Grok => "grok",
            BackendKind::Antigravity => "agy",
            BackendKind::OpenCode => "opencode",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(BackendKind::Claude),
            "codex" => Some(BackendKind::Codex),
            "grok" => Some(BackendKind::Grok),
            "agy" | "antigravity" => Some(BackendKind::Antigravity),
            "opencode" => Some(BackendKind::OpenCode),
            _ => None,
        }
    }

    pub fn info(self) -> BackendInfo {
        match self {
            Self::Claude => BackendInfo {
                id: self,
                label: "Claude",
                context_usage: true,
            },
            Self::Codex => BackendInfo {
                id: self,
                label: "Codex",
                context_usage: false,
            },
            Self::Grok => BackendInfo {
                id: self,
                label: "Grok",
                context_usage: false,
            },
            Self::Antigravity => BackendInfo {
                id: self,
                label: "Antigravity",
                context_usage: false,
            },
            Self::OpenCode => BackendInfo {
                id: self,
                label: "OpenCode",
                context_usage: false,
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

    /// Environment overrides applied only to the backend process. This is
    /// preferable to editing a user's global CLI configuration when a backend
    /// exposes per-process configuration through environment variables.
    fn env(&self) -> Vec<(String, String)> {
        Vec::new()
    }

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
}

pub fn for_kind(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Claude => Box::new(ClaudeBackend::new()),
        BackendKind::Codex => Box::new(CodexBackend::new()),
        BackendKind::Grok => Box::new(GrokBackend::new()),
        BackendKind::Antigravity => Box::new(AgyBackend::new()),
        BackendKind::OpenCode => Box::new(OpenCodeBackend::new()),
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
            (BackendKind::Grok, "grok"),
            (BackendKind::Antigravity, "agy"),
            (BackendKind::OpenCode, "opencode"),
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
        assert_eq!(
            BackendKind::from_str("antigravity"),
            Some(BackendKind::Antigravity)
        );
    }

    #[test]
    fn antigravity_serializes_as_its_cli_id_and_accepts_product_name() {
        assert_eq!(
            serde_json::to_string(&BackendKind::Antigravity).unwrap(),
            "\"agy\""
        );
        assert_eq!(
            serde_json::from_str::<BackendKind>("\"antigravity\"").unwrap(),
            BackendKind::Antigravity
        );
    }

    #[test]
    fn every_backend_argv_starts_with_its_command() {
        let cases = [
            (BackendKind::Claude, "claude"),
            (BackendKind::Codex, "codex"),
            (BackendKind::Grok, "grok"),
            (BackendKind::Antigravity, "agy"),
            (BackendKind::OpenCode, "opencode"),
        ];
        for (kind, command) in cases {
            let backend = for_kind(kind);
            let argv = backend.argv(Path::new("/some/path"));
            assert_eq!(argv.first().map(String::as_str), Some(command));
            assert_eq!(backend.kind(), kind);
        }
    }

    #[test]
    fn every_backend_launches_with_unrestricted_permissions() {
        for kind in BackendKind::ALL {
            let backend = for_kind(kind);
            let argv = backend.argv(Path::new("/some/path"));
            let unrestricted = match kind {
                BackendKind::Claude | BackendKind::Antigravity => argv
                    .iter()
                    .any(|arg| arg == "--dangerously-skip-permissions"),
                BackendKind::Codex => argv
                    .iter()
                    .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"),
                BackendKind::Grok => argv.iter().any(|arg| arg == "--always-approve"),
                BackendKind::OpenCode => backend
                    .env()
                    .iter()
                    .any(|(key, value)| key == "OPENCODE_PERMISSION" && value == r#""allow""#),
            };
            assert!(unrestricted, "{kind:?} does not launch unrestricted");
        }
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
