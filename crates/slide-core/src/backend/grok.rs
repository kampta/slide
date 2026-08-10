use super::{Backend, BackendKind};
use crate::classifier::{common_needs_input_signals, Signals};
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

pub struct GrokBackend;

static SIGNALS: OnceLock<Signals> = OnceLock::new();

impl Default for GrokBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl GrokBackend {
    pub fn new() -> Self {
        Self
    }
}

fn argv_with_permissions() -> Vec<String> {
    vec!["grok".into(), "--always-approve".into()]
}

/// Grok Build's idle composer renders `Type a message...`. Depending on
/// screen mode, the composer may instead be represented by a bare `>` prompt.
/// During a turn Grok documents Esc/Ctrl+C as its cancellation keys; releases
/// that render the corresponding status hint are treated as definitively busy.
fn signals() -> &'static Signals {
    SIGNALS.get_or_init(|| Signals {
        needs_input: common_needs_input_signals(),
        working: vec![Regex::new(
            r"(?mi)\b(?:esc|ctrl[\s+-]*c)\s+(?:to\s+)?(?:interrupt|stop|cancel)\b",
        )
        .unwrap()],
        idle_hints: vec![Regex::new(r"(?mi)^\s*Type a message(?:\.\.\.)?\s*$").unwrap()],
        prompt: vec![Regex::new(r"(?m)^\s*[>❯›][\s\u{a0}].*$").unwrap()],
        settle_ms: 1500,
    })
}

impl Backend for GrokBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Grok
    }

    fn argv(&self, _cwd: &Path) -> Vec<String> {
        argv_with_permissions()
    }

    fn signals(&self) -> &'static Signals {
        signals()
    }

    fn resume_argv(&self, _cwd: &Path, session_id: &str) -> Option<Vec<String>> {
        let mut argv = argv_with_permissions();
        argv.extend(["--resume".into(), session_id.into()]);
        Some(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any(regs: &[Regex], pane: &str) -> bool {
        regs.iter().any(|regex| regex.is_match(pane))
    }

    #[test]
    fn resume_argv_uses_resume_flag() {
        let argv = GrokBackend::new()
            .resume_argv(Path::new("/tmp"), "session-123")
            .unwrap();
        assert_eq!(&argv[argv.len() - 2..], ["--resume", "session-123"]);
    }

    #[test]
    fn launch_argv_auto_approves_all_tools() {
        let argv = GrokBackend::new().argv(Path::new("/tmp"));
        assert_eq!(argv, vec!["grok", "--always-approve"]);
    }

    #[test]
    fn signals_match_grok_composer_and_busy_hint() {
        let signals = signals();
        assert!(any(&signals.idle_hints, "Type a message..."));
        assert!(any(&signals.prompt, "> "));
        assert!(any(&signals.working, "Esc to cancel"));
    }
}
