use super::{Backend, BackendKind};
use crate::classifier::Signals;
use regex::Regex;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::OnceLock;
use std::time::SystemTime;

pub struct CodexBackend;

static SIGNALS: OnceLock<Signals> = OnceLock::new();

impl Default for CodexBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexBackend {
    pub fn new() -> Self {
        Self
    }
}

fn argv_with_permissions() -> Vec<String> {
    vec![
        "codex".into(),
        "--dangerously-bypass-approvals-and-sandbox".into(),
    ]
}

/// Classification patterns for codex-cli. The v0.124 prompt is a `›`
/// followed by placeholder hint text (e.g. `› Write tests for @filename`);
/// older builds drew `user>`, `>`, or `▌`. All kept so mixed versions
/// classify correctly.
///
/// No idle-hint regex yet — codex's bottom row is a model/cwd label that
/// is present in *both* states, so it's not a reliable positive signal.
/// The byte-idle gate + prompt match carries the load here. Add an
/// idle-hint entry if a future codex release introduces a state-specific
/// status string.
fn signals() -> &'static Signals {
    SIGNALS.get_or_init(|| Signals {
        working: vec![
            // Covers `Esc to interrupt`, `Ctrl+C to interrupt`, and the
            // space-separated `Ctrl C` variant some TUIs use. Case-
            // insensitive so we catch title-case variants.
            Regex::new(r"(?mi)\b(?:esc|ctrl[\s+-]*c)\s+to\s+(?:interrupt|stop|cancel)\b").unwrap(),
        ],
        // Codex's TUI doesn't draw a persistent status line that paints
        // during both Active and Waiting, so byte-idle plus the prompt
        // regex below suffices today. If a future Codex release introduces
        // a status indicator that survives the settle window (e.g. an
        // always-visible model selector), add a regex here that matches
        // *only* the Waiting form so the classifier doesn't get stuck on
        // Active.
        idle_hints: vec![],
        prompt: vec![
            // v0.124 form: `›` + whitespace + anything (placeholder hint
            // or user-typed text). `›` only appears at the prompt line in
            // rendered panes, so this is safe inside the 24-row viewport
            // we hand to the classifier.
            Regex::new(r"(?m)^›[\s\u{a0}].*$").unwrap(),
            // Legacy forms from earlier codex builds and any backend
            // variant that draws a plain prompt.
            Regex::new(r"(?m)^[\s│▌>]*(?:user\s*>|>|▌)\s*$").unwrap(),
        ],
        settle_ms: 1500,
    })
}

fn transcript_root() -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".codex").join("sessions"))
}

fn session_meta(path: &Path) -> Option<(String, String)> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    let payload = v.get("payload")?;
    let id = payload.get("id").and_then(|x| x.as_str())?;
    let cwd = payload.get("cwd").and_then(|x| x.as_str())?;
    Some((id.to_string(), cwd.to_string()))
}

fn discover_session_id_in(root: &Path, cwd: &Path, since: SystemTime) -> Option<String> {
    fn visit(
        dir: &Path,
        want_cwd: &str,
        since: SystemTime,
        best: &mut Option<(SystemTime, String)>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, want_cwd, since, best);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = match meta.modified() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if mtime <= since {
                continue;
            }
            let Some((id, transcript_cwd)) = session_meta(&path) else {
                continue;
            };
            if transcript_cwd != want_cwd {
                continue;
            }
            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                *best = Some((mtime, id));
            }
        }
    }

    let mut best = None;
    visit(root, &cwd.to_string_lossy(), since, &mut best);
    best.map(|(_, id)| id)
}

impl Backend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Codex
    }

    fn argv(&self, _cwd: &Path) -> Vec<String> {
        argv_with_permissions()
    }

    fn signals(&self) -> &'static Signals {
        signals()
    }

    fn resume_argv(&self, _cwd: &Path, session_id: &str) -> Option<Vec<String>> {
        let mut argv = argv_with_permissions();
        argv.extend(["resume".into(), session_id.into()]);
        Some(argv)
    }

    fn discover_session_id(&self, cwd: &Path, since: SystemTime) -> Option<String> {
        let root = transcript_root()?;
        discover_session_id_in(&root, cwd, since)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    #[test]
    fn argv_bypasses_approvals_and_sandbox() {
        let b = CodexBackend::new();
        let argv = b.argv(Path::new("/tmp"));
        assert_eq!(
            argv,
            vec!["codex", "--dangerously-bypass-approvals-and-sandbox"]
        );
    }

    #[test]
    fn resume_argv_uses_resume_subcommand_with_full_permissions() {
        let b = CodexBackend::new();
        let argv = b.resume_argv(Path::new("/tmp"), "abc-123").unwrap();
        assert_eq!(
            argv,
            vec![
                "codex",
                "--dangerously-bypass-approvals-and-sandbox",
                "resume",
                "abc-123"
            ]
        );
    }

    fn any(regs: &[Regex], s: &str) -> bool {
        regs.iter().any(|r| r.is_match(s))
    }

    /// Common working-hint variants. Case-insensitive so `Esc to interrupt`
    /// and `esc to interrupt` both classify without duplicating patterns.
    #[test]
    fn working_signal_matches_interrupt_hints() {
        let s = signals();
        for pane in [
            "  Esc to interrupt",
            "  esc to interrupt                  status",
            "  Ctrl+C to interrupt",
            "  Ctrl C to stop",
        ] {
            assert!(any(&s.working, pane), "working missed: {pane:?}");
        }
    }

    /// v0.124 `›` prompt with a placeholder hint after it — exactly what
    /// `capture-pane` sees for an idle codex session.
    #[test]
    fn prompt_matches_v0_124_placeholder_line() {
        let s = signals();
        assert!(any(&s.prompt, "› Write tests for @filename"));
        assert!(any(&s.prompt, "› "));
        assert!(any(&s.prompt, "›\u{a0}"));
    }

    /// Legacy codex prompt forms still classify so older builds aren't
    /// regressed.
    #[test]
    fn prompt_matches_legacy_forms() {
        let s = signals();
        assert!(any(&s.prompt, "user>"));
        assert!(any(&s.prompt, "> "));
        assert!(any(&s.prompt, "▌"));
    }

    /// Random content containing `›` mid-line (e.g. something a tool
    /// output printed) must not flip the session to Waiting.
    #[test]
    fn prompt_does_not_match_prose_with_angle_quote() {
        let s = signals();
        assert!(!any(&s.prompt, "path/to/file › error"));
        assert!(!any(&s.prompt, "Here > result"));
    }

    #[test]
    fn session_meta_reads_id_and_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":",
                "{\"id\":\"sid-123\",\"cwd\":\"/tmp/demo\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n"
            ),
        )
        .unwrap();
        let got = session_meta(&path);
        assert_eq!(got, Some(("sid-123".to_string(), "/tmp/demo".to_string())));
    }

    #[test]
    fn discover_session_id_finds_newest_matching_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let day_dir = root.join("2026").join("04").join("23");
        fs::create_dir_all(&day_dir).unwrap();

        let other = day_dir.join("other.jsonl");
        fs::write(
            &other,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"other\",\"cwd\":\"/wrong\"}}\n",
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(20));
        let older = day_dir.join("older.jsonl");
        fs::write(
            &older,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"older\",\"cwd\":\"/demo/proj\"}}\n",
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(20));
        let newer = day_dir.join("newer.jsonl");
        fs::write(
            &newer,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"newer\",\"cwd\":\"/demo/proj\"}}\n",
        )
        .unwrap();

        let got = discover_session_id_in(root, Path::new("/demo/proj"), SystemTime::UNIX_EPOCH);
        assert_eq!(got.as_deref(), Some("newer"));

        let after = SystemTime::now() + Duration::from_secs(60);
        assert!(discover_session_id_in(root, Path::new("/demo/proj"), after).is_none());
    }
}
