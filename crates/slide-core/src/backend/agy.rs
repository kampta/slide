use super::{Backend, BackendKind};
use crate::classifier::Signals;
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// Google Antigravity CLI. The product name is Antigravity; its executable
/// and canonical backend id are both `agy`.
pub struct AgyBackend;

static SIGNALS: OnceLock<Signals> = OnceLock::new();

impl Default for AgyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AgyBackend {
    pub fn new() -> Self {
        Self
    }
}

/// Antigravity can auto-approve commands that remain inside its OS sandbox.
/// Fine-grained overrides additionally approve every Git command, including
/// an unsandboxed retry when a Git operation cannot remain contained.
fn argv_with_permissions() -> Vec<String> {
    vec![
        "agy".into(),
        "--sandbox".into(),
        "--tool-permission=proceed-in-sandbox".into(),
        "--permissions.allow=command(git)".into(),
        "--permissions.allow=unsandboxed(git)".into(),
        "--permissions.allow=command(cargo (build|check|test|fmt|clippy|fetch))".into(),
        "--permissions.allow=command(npm (install|ci|test|run))".into(),
        "--permissions.allow=command(pnpm (install|test|run))".into(),
        "--permissions.allow=command(yarn (install|test|run))".into(),
        "--permissions.allow=command(make (build|test|lint))".into(),
        "--permissions.allow=command(./scripts/(dev\\.sh|bootstrap\\.sh))".into(),
        "--permissions.allow=command(gh pr (view|checks))".into(),
        "--permissions.allow=command(gh run view)".into(),
        "--permissions.allow=command(gh issue view)".into(),
        "--permissions.allow=command((ps|pgrep|lsof|kill|pkill))".into(),
        "--permissions.allow=read_url(github.com)".into(),
        "--permissions.allow=read_url(npmjs.org)".into(),
        "--permissions.allow=read_url(crates.io)".into(),
    ]
}

/// Antigravity descends from the Gemini CLI TUI and keeps its `>` composer.
/// Newer releases also render a text placeholder at the prompt. Busy hints
/// have changed from Esc to Ctrl+C, so accept both documented key labels.
fn signals() -> &'static Signals {
    SIGNALS.get_or_init(|| Signals {
        working: vec![Regex::new(
            r"(?mi)\b(?:esc|ctrl[\s+-]*c)\s+(?:to\s+)?(?:interrupt|stop|cancel)\b",
        )
        .unwrap()],
        idle_hints: vec![Regex::new(
            r"(?mi)^\s*(?:Type (?:a|your) message(?:\.\.\.)?|Press \? for help)\s*$",
        )
        .unwrap()],
        prompt: vec![Regex::new(r"(?m)^\s*(?:│\s*)?[>❯][\s\u{a0}].*$").unwrap()],
        settle_ms: 1500,
    })
}

impl Backend for AgyBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Antigravity
    }

    fn argv(&self, _cwd: &Path) -> Vec<String> {
        argv_with_permissions()
    }

    fn signals(&self) -> &'static Signals {
        signals()
    }

    fn resume_argv(&self, _cwd: &Path, session_id: &str) -> Option<Vec<String>> {
        let mut argv = argv_with_permissions();
        argv.extend(["--conversation".into(), session_id.into()]);
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
    fn resume_argv_uses_conversation_flag() {
        let argv = AgyBackend::new()
            .resume_argv(Path::new("/tmp"), "conversation-123")
            .unwrap();
        assert_eq!(
            &argv[argv.len() - 2..],
            ["--conversation", "conversation-123"]
        );
    }

    #[test]
    fn launch_argv_auto_approves_sandboxed_operations_and_all_git() {
        let argv = AgyBackend::new().argv(Path::new("/tmp"));
        for permission in [
            "--permissions.allow=command(git)",
            "--permissions.allow=command(cargo (build|check|test|fmt|clippy|fetch))",
            "--permissions.allow=command(npm (install|ci|test|run))",
            "--permissions.allow=command(gh pr (view|checks))",
            "--permissions.allow=command((ps|pgrep|lsof|kill|pkill))",
            "--permissions.allow=read_url(crates.io)",
        ] {
            assert!(argv.iter().any(|arg| arg == permission));
        }
        assert!(!argv.iter().any(|arg| arg.contains("gh pr merge")));
    }

    #[test]
    fn signals_match_antigravity_composer_and_busy_hint() {
        let signals = signals();
        assert!(any(&signals.idle_hints, "Type your message..."));
        assert!(any(&signals.prompt, "> "));
        assert!(any(&signals.prompt, "│ > "));
        assert!(any(&signals.working, "Ctrl+C to interrupt"));
    }
}
