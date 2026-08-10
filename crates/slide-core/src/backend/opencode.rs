use super::{Backend, BackendKind};
use crate::classifier::Signals;
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

pub struct OpenCodeBackend;

static SIGNALS: OnceLock<Signals> = OnceLock::new();

impl Default for OpenCodeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeBackend {
    pub fn new() -> Self {
        Self
    }
}

/// OpenCode accepts a JSON permission value through this per-process override.
/// The string value `"allow"` grants every permission category.
const DEFAULT_PERMISSIONS: &str = r#""allow""#;

/// OpenCode themes change the prompt border but retain either a composer
/// placeholder or a leading prompt glyph in captured terminal text. The
/// settle gate prevents the persistent composer from winning while output is
/// still streaming.
fn signals() -> &'static Signals {
    SIGNALS.get_or_init(|| Signals {
        working: vec![Regex::new(
            r"(?mi)\b(?:esc|ctrl[\s+-]*c)\s+(?:to\s+)?(?:interrupt|stop|cancel)\b",
        )
        .unwrap()],
        idle_hints: vec![Regex::new(
            r"(?mi)^\s*(?:Ask anything|Type a message|What do you want to do\?)(?:\.\.\.)?\s*$",
        )
        .unwrap()],
        prompt: vec![Regex::new(r"(?m)^\s*(?:│\s*)?[>❯›][\s\u{a0}].*$").unwrap()],
        settle_ms: 1500,
    })
}

impl Backend for OpenCodeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::OpenCode
    }

    fn argv(&self, _cwd: &Path) -> Vec<String> {
        vec!["opencode".into()]
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![("OPENCODE_PERMISSION".into(), DEFAULT_PERMISSIONS.into())]
    }

    fn signals(&self) -> &'static Signals {
        signals()
    }

    fn resume_argv(&self, _cwd: &Path, session_id: &str) -> Option<Vec<String>> {
        Some(vec![
            "opencode".into(),
            "--session".into(),
            session_id.into(),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any(regs: &[Regex], pane: &str) -> bool {
        regs.iter().any(|regex| regex.is_match(pane))
    }

    #[test]
    fn resume_argv_uses_session_flag() {
        let argv = OpenCodeBackend::new()
            .resume_argv(Path::new("/tmp"), "session-123")
            .unwrap();
        assert_eq!(argv, vec!["opencode", "--session", "session-123"]);
    }

    #[test]
    fn signals_match_opencode_composer_and_busy_hint() {
        let signals = signals();
        assert!(any(&signals.idle_hints, "Ask anything..."));
        assert!(any(&signals.prompt, "│ > "));
        assert!(any(&signals.working, "esc to interrupt"));
    }

    #[test]
    fn environment_allows_every_permission() {
        let env = OpenCodeBackend::new().env();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "OPENCODE_PERMISSION");
        let permissions: serde_json::Value = serde_json::from_str(&env[0].1).unwrap();
        assert_eq!(permissions, "allow");
    }
}
