use super::{Backend, BackendKind};
use crate::classifier::{common_needs_input_signals, Signals};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

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

fn grok_home() -> Option<PathBuf> {
    std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".grok")))
}

/// Grok stores sessions under a percent-encoded absolute working-directory
/// component. This mirrors the layout used by Grok 1.0 without reading any
/// transcript content.
fn encode_cwd(cwd: &Path) -> String {
    let mut encoded = String::new();
    for byte in cwd.to_string_lossy().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    encoded
}

fn session_group(sessions_root: &Path, cwd: &Path) -> Option<PathBuf> {
    let encoded = encode_cwd(cwd);
    if encoded.len() <= 255 {
        return Some(sessions_root.join(encoded));
    }

    // Grok hashes encoded paths that exceed a filesystem component. It
    // records the original path in `<group>/.cwd`, so discovery does not need
    // to duplicate Grok's private slug/hash algorithm.
    let expected = cwd.to_string_lossy();
    for entry in std::fs::read_dir(sessions_root).ok()?.flatten() {
        let path = entry.path();
        let recorded = std::fs::read_to_string(path.join(".cwd")).ok();
        if recorded.as_deref().map(str::trim_end) == Some(expected.as_ref()) {
            return Some(path);
        }
    }
    None
}

fn discover_in(sessions_root: &Path, cwd: &Path, since: SystemTime) -> Option<String> {
    let group = session_group(sessions_root, cwd)?;
    let mut newest: Option<(SystemTime, String)> = None;
    for entry in std::fs::read_dir(group).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(metadata) = path
            .join("summary.json")
            .metadata()
            .or_else(|_| entry.metadata())
        else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified <= since {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if newest
            .as_ref()
            .map(|(best, _)| modified > *best)
            .unwrap_or(true)
        {
            newest = Some((modified, id));
        }
    }
    newest.map(|(_, id)| id)
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

    fn fork_argv(
        &self,
        _cwd: &Path,
        session_id: &str,
        prompt: Option<&str>,
    ) -> Option<Vec<String>> {
        let mut argv = argv_with_permissions();
        argv.extend([
            "--resume".into(),
            session_id.into(),
            "--fork-session".into(),
        ]);
        if let Some(prompt) = prompt.filter(|prompt| !prompt.is_empty()) {
            argv.push(prompt.into());
        }
        Some(argv)
    }

    fn discover_session_id(&self, cwd: &Path, since: SystemTime) -> Option<String> {
        discover_in(&grok_home()?.join("sessions"), cwd, since)
    }

    fn supports_session_discovery(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn fork_argv_branches_resumed_session() {
        let argv = GrokBackend::new()
            .fork_argv(Path::new("/tmp"), "session-123", Some("try another design"))
            .unwrap();
        assert_eq!(
            &argv[argv.len() - 4..],
            [
                "--resume",
                "session-123",
                "--fork-session",
                "try another design"
            ]
        );
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

    #[test]
    fn cwd_encoding_matches_grok_session_groups() {
        assert_eq!(
            encode_cwd(Path::new("/Users/kampta/my repo")),
            "%2FUsers%2Fkampta%2Fmy%20repo"
        );
    }

    #[test]
    fn discovers_session_directory_for_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/work/project");
        let session = temp.path().join(encode_cwd(cwd)).join("session-123");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("summary.json"), "{}").unwrap();

        assert_eq!(
            discover_in(temp.path(), cwd, SystemTime::UNIX_EPOCH).as_deref(),
            Some("session-123"),
        );
    }

    #[test]
    fn ignores_sessions_older_than_spawn_time() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/work/project");
        let session = temp.path().join(encode_cwd(cwd)).join("old-session");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("summary.json"), "{}").unwrap();

        assert_eq!(discover_in(temp.path(), cwd, SystemTime::now()), None);
    }
}
