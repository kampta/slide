use super::{Backend, BackendKind};
use crate::classifier::{common_needs_input_signals, Signals};
use regex::Regex;
use rusqlite::Connection;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

fn argv_with_permissions() -> Vec<String> {
    vec!["agy".into(), "--dangerously-skip-permissions".into()]
}

fn antigravity_home() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".gemini").join("antigravity-cli"))
}

fn parse_summary_time(value: &str) -> Option<SystemTime> {
    let millis = value
        .parse::<i64>()
        .ok()
        .and_then(|value| super::parse_timestamp_ms(&Value::from(value)))
        .or_else(|| {
            let normalized = if value.contains('T') {
                value.to_string()
            } else {
                format!("{}Z", value.replace(' ', "T"))
            };
            super::parse_timestamp_ms(&Value::String(normalized))
        })?;
    if millis < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_millis(millis as u64))
}

fn workspace_matches(raw: &str, cwd: &Path) -> bool {
    let expected = cwd.to_string_lossy();
    fn contains_path(value: &Value, expected: &str) -> bool {
        match value {
            Value::String(value) => {
                value == expected
                    || value
                        .strip_prefix("file://")
                        .is_some_and(|path| path == expected)
            }
            Value::Array(values) => values.iter().any(|value| contains_path(value, expected)),
            Value::Object(values) => values.values().any(|value| contains_path(value, expected)),
            _ => false,
        }
    }
    serde_json::from_str::<Value>(raw)
        .ok()
        .is_some_and(|value| contains_path(&value, expected.as_ref()))
}

fn discover_in(db_path: &Path, cwd: &Path, since: SystemTime) -> Option<String> {
    let connection = Connection::open(db_path).ok()?;
    let mut statement = connection
        .prepare(
            "SELECT conversation_id, last_modified_time, workspace_uris
             FROM conversation_summaries",
        )
        .ok()?;
    let mut rows = statement.query([]).ok()?;
    let mut newest: Option<(SystemTime, String)> = None;
    while let Some(row) = rows.next().ok()? {
        let id = match row.get::<_, String>(0) {
            Ok(id) if !id.is_empty() => id,
            _ => continue,
        };
        let modified = match row
            .get::<_, String>(1)
            .ok()
            .and_then(|value| parse_summary_time(&value))
        {
            Some(modified) => modified,
            None => continue,
        };
        let workspaces = match row.get::<_, String>(2) {
            Ok(workspaces) => workspaces,
            Err(_) => continue,
        };
        if modified <= since || !workspace_matches(&workspaces, cwd) {
            continue;
        }
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

fn project_id_for(db_path: &Path, session_id: &str) -> String {
    let Some(connection) = Connection::open(db_path).ok() else {
        return "default-cli-project".to_string();
    };
    connection
        .query_row(
            "SELECT project_id FROM conversation_summaries WHERE conversation_id = ?1",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|project_id| !project_id.is_empty())
        .unwrap_or_else(|| "default-cli-project".to_string())
}

/// Antigravity descends from the Gemini CLI TUI and keeps its `>` composer.
/// Newer releases also render a text placeholder at the prompt. Busy hints
/// have changed from Esc to Ctrl+C, so accept both documented key labels.
fn signals() -> &'static Signals {
    SIGNALS.get_or_init(|| Signals {
        needs_input: common_needs_input_signals(),
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

    fn fork_argv(
        &self,
        _cwd: &Path,
        session_id: &str,
        _prompt: Option<&str>,
    ) -> Option<Vec<String>> {
        // Antigravity's fork operation is an interactive `/fork` command;
        // fork_input sends it after this process resumes the source.
        let mut argv = argv_with_permissions();
        argv.extend(["--conversation".into(), session_id.into()]);
        Some(argv)
    }

    fn fork_input(&self, _cwd: &Path, session_id: &str) -> Option<Vec<u8>> {
        let db_path = antigravity_home()?.join("conversation_summaries.db");
        let project_id = project_id_for(&db_path, session_id);
        Some(format!("/fork {project_id}\r").into_bytes())
    }

    fn discover_session_id(&self, cwd: &Path, since: SystemTime) -> Option<String> {
        discover_in(
            &antigravity_home()?.join("conversation_summaries.db"),
            cwd,
            since,
        )
    }

    fn supports_session_discovery(&self) -> bool {
        true
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
    fn fork_argv_continues_the_durable_conversation() {
        let argv = AgyBackend::new()
            .fork_argv(
                Path::new("/tmp"),
                "conversation-123",
                Some("ignored by agy"),
            )
            .unwrap();
        assert_eq!(
            argv,
            vec![
                "agy",
                "--dangerously-skip-permissions",
                "--conversation",
                "conversation-123"
            ]
        );
    }

    #[test]
    fn fork_input_uses_the_source_project() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("summaries.db");
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE conversation_summaries (
                   conversation_id TEXT,
                   project_id TEXT
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2)",
                rusqlite::params!["conversation-123", "project-456"],
            )
            .unwrap();
        drop(connection);

        assert_eq!(project_id_for(&db, "conversation-123"), "project-456");
    }

    #[test]
    fn discovers_newest_summary_for_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("summaries.db");
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE conversation_summaries (
                   conversation_id TEXT,
                   last_modified_time TEXT,
                   workspace_uris TEXT
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3)",
                rusqlite::params!["old", "2025-01-01 00:00:00", "[\"file:///work/project\"]"],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3)",
                rusqlite::params!["new", "2026-01-01 00:00:00", "[\"file:///work/project\"]"],
            )
            .unwrap();
        drop(connection);

        assert_eq!(
            discover_in(&db, Path::new("/work/project"), UNIX_EPOCH).as_deref(),
            Some("new")
        );
    }

    #[test]
    fn launch_argv_skips_all_permission_prompts() {
        let argv = AgyBackend::new().argv(Path::new("/tmp"));
        assert_eq!(argv, vec!["agy", "--dangerously-skip-permissions"]);
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
