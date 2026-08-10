use super::{Backend, BackendKind, ContextUsage};
use crate::classifier::{common_needs_input_signals, Signals};
use regex::Regex;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::OnceLock;
use std::time::SystemTime;

pub struct ClaudeBackend;

static SIGNALS: OnceLock<Signals> = OnceLock::new();

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeBackend {
    pub fn new() -> Self {
        Self
    }
}

fn argv_with_permissions() -> Vec<String> {
    vec!["claude".into(), "--dangerously-skip-permissions".into()]
}

/// Patterns observed from `tmux capture-pane -p` against a live Claude
/// Code session. Working hint is the bottom-row "esc to interrupt" label;
/// idle hints are the default `? for shortcuts` plus the three `⏵⏵`
/// mode banners; prompt is the v2 `❯` glyph with v1 `│ > │` kept so older
/// installs still classify.
fn signals() -> &'static Signals {
    SIGNALS.get_or_init(|| Signals {
        needs_input: common_needs_input_signals(),
        // De-anchored: newer Claude Code renders the bottom row as a single
        // `·`-separated line where the mode banner sits to the left of `esc
        // to interrupt`, so requiring start-of-line lets the idle-hint
        // regex win and the session misclassifies as Waiting while
        // generating.
        working: vec![Regex::new(r"(?mi)\besc to interrupt\b").unwrap()],
        idle_hints: vec![Regex::new(
            r"(?m)^\s*(?:\?\s+for shortcuts|⏵⏵ (?:accept edits|plan mode|bypass permissions) on)\b",
        )
        .unwrap()],
        prompt: vec![
            // v2: `❯` (or legacy `>`) at start of line, then any typed
            // content. The settle gate in `classify` keeps this from
            // false-matching while the spinner repaints.
            Regex::new(r"(?m)^[❯>][\s\u{a0}].*$").unwrap(),
            // v1 boxed form.
            Regex::new(r"(?m)^\s*│\s*>[^│]*│\s*$").unwrap(),
        ],
        settle_ms: 1500,
    })
}

/// Translate a working directory to Claude Code's on-disk project slug
/// (`/` → `-`), which lives at `~/.claude/projects/<slug>/`.
fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
}

fn transcript_dir(cwd: &Path) -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".claude")
            .join("projects")
            .join(project_slug(cwd)),
    )
}

/// Map a model id like `claude-opus-4-7` or `claude-opus-4-7-1m` to its
/// context window. We don't have a live price/spec feed, so this is a small
/// hardcoded table that errs toward the common 200k default; the `-1m`
/// suffix is treated as the long-context Opus variant.
fn window_for_model(model: &str) -> u64 {
    if model.contains("-1m") {
        1_000_000
    } else {
        200_000
    }
}

/// Scan the JSONL transcript at `path` and return the usage object on the
/// last `type: "assistant"` line. Lines are iterated sequentially; the file
/// is read lazily so we only hold one line in memory at a time.
fn last_assistant_usage(path: &Path) -> Option<ContextUsage> {
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut latest: Option<ContextUsage> = None;
    for line in reader.lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        // Cheap reject before paying for JSON parsing.
        if !line.contains("\"assistant\"") || !line.contains("\"usage\"") {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let usage = match msg.get("usage") {
            Some(u) => u,
            None => continue,
        };
        let model = msg
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let input = usage
            .get("input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let cache_create = usage
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let window = window_for_model(&model);
        latest = Some(ContextUsage {
            used_tokens: input + cache_read + cache_create,
            window,
            model,
            input_tokens: input,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_create,
            output_tokens: output,
        });
    }
    latest
}

impl Backend for ClaudeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Claude
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

    fn read_context_usage(&self, cwd: &Path, session_id: &str) -> Option<ContextUsage> {
        let path = transcript_dir(cwd)?.join(format!("{session_id}.jsonl"));
        last_assistant_usage(&path)
    }

    fn discover_session_id(&self, cwd: &Path, since: SystemTime) -> Option<String> {
        let dir = transcript_dir(cwd)?;
        let mut best: Option<(SystemTime, String)> = None;
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
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
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                best = Some((mtime, stem));
            }
        }
        best.map(|(_, id)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    #[test]
    fn argv_skips_permission_prompts() {
        let b = ClaudeBackend::new();
        let argv = b.argv(Path::new("/tmp"));
        assert_eq!(argv, vec!["claude", "--dangerously-skip-permissions"]);
    }

    #[test]
    fn resume_argv_skips_permission_prompts() {
        let b = ClaudeBackend::new();
        let argv = b.resume_argv(Path::new("/tmp"), "abc-123").unwrap();
        assert_eq!(
            argv,
            vec![
                "claude",
                "--dangerously-skip-permissions",
                "--resume",
                "abc-123"
            ]
        );
    }

    #[test]
    fn fork_argv_creates_a_new_session_with_optional_focus() {
        let argv = ClaudeBackend::new()
            .fork_argv(Path::new("/tmp"), "abc-123", Some("try another design"))
            .unwrap();
        assert_eq!(
            argv,
            vec![
                "claude",
                "--dangerously-skip-permissions",
                "--resume",
                "abc-123",
                "--fork-session",
                "try another design",
            ]
        );
    }

    fn any(regs: &[Regex], s: &str) -> bool {
        regs.iter().any(|r| r.is_match(s))
    }

    /// The `working` signal fires on the bottom status row that Claude
    /// draws while generating. Representative of live `capture-pane` output
    /// (leading whitespace from the box gutter + a trailing label).
    #[test]
    fn working_signal_matches_live_pane() {
        let s = signals();
        let pane = "  esc to interrupt                                                   Now using extra usage";
        assert!(any(&s.working, pane));
    }

    /// Newer Claude Code collapses the bottom row into one `·`-separated
    /// line where the mode banner precedes `esc to interrupt`. Both the
    /// working and idle-hint regexes match the same line; the classifier's
    /// priority order picks Active. Regression for sessions stuck on
    /// Waiting while generating.
    #[test]
    fn inline_status_row_classifies_active_while_generating() {
        let s = signals();
        let pane = "\
            ❯ \n  ⏵⏵ accept edits on · ◆ ultraplan ready · ↓ to view · esc to interrupt · ctrl+t to hide tasks\n\
        ";
        assert!(any(&s.working, pane));
        // Idle hint also matches — that's expected, the classifier resolves
        // by priority. Verify the combined classification is Active.
        assert!(any(&s.idle_hints, pane));
        let snap = crate::classifier::Snapshot {
            pane,
            idle_ms: 5_000,
        };
        assert_eq!(
            crate::classifier::classify(&snap, s).state,
            crate::session::SessionState::Active,
        );
    }

    /// All four idle-hint variants Claude emits — the default `?` hint and
    /// the three `⏵⏵` mode banners. These are the strongest "at prompt"
    /// signal and must all classify.
    #[test]
    fn idle_hints_match_each_mode_banner() {
        let s = signals();
        for pane in [
            "  ? for shortcuts",
            "  ⏵⏵ accept edits on (shift+tab to cycle)",
            "  ⏵⏵ plan mode on (shift+tab to cycle)",
            "  ⏵⏵ bypass permissions on",
        ] {
            assert!(any(&s.idle_hints, pane), "idle hint missed: {pane:?}");
        }
    }

    /// The v2 `❯` prompt glyph including the non-breaking space Claude
    /// emits — this was the regression that kept sessions stuck on Active.
    /// `\s` in Rust's regex matches NBSP in Unicode mode so we don't need
    /// a separate branch.
    #[test]
    fn prompt_matches_v2_arrow_with_nbsp() {
        let s = signals();
        assert!(any(&s.prompt, "❯\u{a0}"));
        assert!(any(&s.prompt, "❯ "));
        assert!(any(&s.prompt, "❯ hello"));
    }

    /// v1 boxed form still classifies, so old Claude Code installs aren't
    /// regressed by the layered rewrite.
    #[test]
    fn prompt_matches_v1_boxed_form() {
        let s = signals();
        assert!(any(&s.prompt, "│ >                                │"));
        assert!(any(&s.prompt, "│ > hello world                    │"));
    }

    /// Random prose containing `>` is not a prompt line. Keeps the
    /// classifier from flipping on conversation output that happens to
    /// mention a `>` character.
    #[test]
    fn prompt_does_not_match_prose_with_angle_bracket() {
        let s = signals();
        assert!(!any(&s.prompt, "Writing code > fast"));
        assert!(!any(&s.prompt, "Here is the result"));
        // Box borders alone aren't a prompt.
        assert!(!any(&s.prompt, "╭──────────────────────────────────╮"));
        assert!(!any(&s.prompt, "╰──────────────────────────────────╯"));
        // Spinner line that happens to sit inside the v1 box.
        assert!(!any(&s.prompt, "│ ✳ Jitterbugging... (1m 14s)      │"));
    }

    #[test]
    fn project_slug_replaces_slashes() {
        assert_eq!(
            project_slug(Path::new("/Users/kampta/code/slide")),
            "-Users-kampta-code-slide"
        );
    }

    /// When the real `~/.claude/projects` doesn't contain a matching dir,
    /// discovery returns None rather than failing.
    #[test]
    fn discover_session_id_missing_dir_returns_none() {
        let b = ClaudeBackend::new();
        let got = b.discover_session_id(
            Path::new("/no/such/path/that/exists/anywhere"),
            SystemTime::UNIX_EPOCH,
        );
        assert!(got.is_none());
    }

    #[test]
    fn window_for_model_defaults_to_200k_and_handles_1m_suffix() {
        assert_eq!(window_for_model("claude-opus-4-7"), 200_000);
        assert_eq!(window_for_model("claude-sonnet-4-6"), 200_000);
        assert_eq!(window_for_model("claude-opus-4-7-1m"), 1_000_000);
        assert_eq!(window_for_model(""), 200_000);
    }

    #[test]
    fn last_assistant_usage_picks_newest_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        // Two assistant turns plus an unrelated user turn in between. Reader
        // must pick the *last* assistant usage, not the first.
        let older = serde_json::json!({
            "type": "assistant",
            "message": {
                "model": "claude-opus-4-7",
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": 100,
                    "cache_creation_input_tokens": 5,
                    "output_tokens": 20,
                },
            },
        });
        let user_turn = serde_json::json!({ "type": "user", "message": {} });
        let newest = serde_json::json!({
            "type": "assistant",
            "message": {
                "model": "claude-opus-4-7-1m",
                "usage": {
                    "input_tokens": 3,
                    "cache_read_input_tokens": 40_000,
                    "cache_creation_input_tokens": 200,
                    "output_tokens": 77,
                },
            },
        });
        let lines = [older, user_turn, newest]
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, lines).unwrap();
        let u = last_assistant_usage(&path).expect("usage");
        assert_eq!(u.model, "claude-opus-4-7-1m");
        assert_eq!(u.input_tokens, 3);
        assert_eq!(u.cache_read_input_tokens, 40_000);
        assert_eq!(u.cache_creation_input_tokens, 200);
        assert_eq!(u.used_tokens, 40_203);
        assert_eq!(u.window, 1_000_000);
    }

    #[test]
    fn last_assistant_usage_missing_file_returns_none() {
        assert!(last_assistant_usage(Path::new("/no/such/file.jsonl")).is_none());
    }

    /// Stand up a fake `$HOME/.claude/projects/<slug>` with a jsonl newer
    /// than `since`; discovery should surface its stem as the session id.
    /// We redirect HOME via std::env; skip on platforms where dirs::home_dir
    /// ignores HOME (it respects HOME on unix).
    #[test]
    fn discover_session_id_finds_newest_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        // dirs::home_dir reads $HOME on unix; point it at our tempdir.
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let cwd = Path::new("/demo/proj");
        let dir = transcript_dir(cwd).unwrap();
        fs::create_dir_all(&dir).unwrap();
        let older = dir.join("older-uuid.jsonl");
        let newer = dir.join("newer-uuid.jsonl");
        fs::write(&older, b"{}").unwrap();
        // Sleep a touch so mtimes differ even on coarse-grained filesystems.
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&newer, b"{}").unwrap();

        let b = ClaudeBackend::new();
        let got = b.discover_session_id(cwd, SystemTime::UNIX_EPOCH);
        assert_eq!(got.as_deref(), Some("newer-uuid"));

        // `since` after the newer file means nothing qualifies.
        let after = SystemTime::now() + Duration::from_secs(60);
        assert!(b.discover_session_id(cwd, after).is_none());

        // Restore HOME so other tests aren't affected.
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}
