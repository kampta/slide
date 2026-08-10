use super::{Backend, BackendKind};
use crate::classifier::Signals;
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

fn permission_args() -> Vec<String> {
    let rules = [
        "Read",
        "Glob",
        "Grep",
        "WebSearch",
        "WebFetch",
        "Edit",
        "Write",
        "NotebookEdit",
        "Bash(git*)",
        "Bash(cargo build*)",
        "Bash(cargo check*)",
        "Bash(cargo test*)",
        "Bash(cargo fmt*)",
        "Bash(cargo clippy*)",
        "Bash(cargo fetch*)",
        "Bash(npm install*)",
        "Bash(npm ci*)",
        "Bash(npm test*)",
        "Bash(npm run build*)",
        "Bash(npm run test*)",
        "Bash(npm run lint*)",
        "Bash(npm run typecheck*)",
        "Bash(pnpm install*)",
        "Bash(pnpm test*)",
        "Bash(pnpm run build*)",
        "Bash(pnpm run test*)",
        "Bash(pnpm run lint*)",
        "Bash(pnpm run typecheck*)",
        "Bash(yarn install*)",
        "Bash(yarn test*)",
        "Bash(yarn run build*)",
        "Bash(yarn run test*)",
        "Bash(yarn run lint*)",
        "Bash(yarn run typecheck*)",
        "Bash(make build*)",
        "Bash(make test*)",
        "Bash(make lint*)",
        "Bash(./scripts/dev.sh*)",
        "Bash(./scripts/bootstrap.sh*)",
        "Bash(gh pr view*)",
        "Bash(gh pr checks*)",
        "Bash(gh run view*)",
        "Bash(gh issue view*)",
        "Bash(ps)",
        "Bash(ps *)",
        "Bash(pgrep)",
        "Bash(pgrep *)",
        "Bash(lsof)",
        "Bash(lsof *)",
        "Bash(kill)",
        "Bash(kill *)",
        "Bash(pkill)",
        "Bash(pkill *)",
    ];
    rules
        .into_iter()
        .flat_map(|rule| ["--allow".into(), rule.into()])
        .collect()
}

fn argv_with_permissions() -> Vec<String> {
    let mut argv = vec!["grok".into()];
    argv.extend(permission_args());
    argv
}

/// Grok Build's idle composer renders `Type a message...`. Depending on
/// screen mode, the composer may instead be represented by a bare `>` prompt.
/// During a turn Grok documents Esc/Ctrl+C as its cancellation keys; releases
/// that render the corresponding status hint are treated as definitively busy.
fn signals() -> &'static Signals {
    SIGNALS.get_or_init(|| Signals {
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
    fn launch_argv_preapproves_daily_development_tools() {
        let argv = GrokBackend::new().argv(Path::new("/tmp"));
        for permission in [
            "Read",
            "WebSearch",
            "Edit",
            "Bash(git*)",
            "Bash(cargo test*)",
            "Bash(npm install*)",
            "Bash(./scripts/dev.sh*)",
            "Bash(gh pr view*)",
            "Bash(lsof *)",
        ] {
            assert!(argv.iter().any(|arg| arg == permission));
        }
        assert!(!argv.iter().any(|arg| arg.contains("gh pr merge")));
    }

    #[test]
    fn signals_match_grok_composer_and_busy_hint() {
        let signals = signals();
        assert!(any(&signals.idle_hints, "Type a message..."));
        assert!(any(&signals.prompt, "> "));
        assert!(any(&signals.working, "Esc to cancel"));
    }
}
