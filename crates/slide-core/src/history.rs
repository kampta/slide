use crate::backend::BackendKind;
use crate::session::{Location, Session, SessionState};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const MAX_QUERY_CHARS: usize = 200;
const MAX_RESULTS: usize = 100;
const MAX_RESULTS_PER_SESSION: usize = 8;
const READ_CHUNK_BYTES: usize = 64 * 1024;
const SEARCH_OVERLAP_BYTES: usize = 512;
const SNIPPET_CONTEXT_BYTES: usize = 180;
const REMOTE_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const REMOTE_STDERR_LIMIT: usize = 16 * 1024;
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_SESSION_BATCH: usize = 50;

type HistoryMatch = (u64, String);
type MatchesBySession = HashMap<String, Vec<HistoryMatch>>;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HistorySearchResult {
    pub session_id: String,
    pub session_name: String,
    pub backend: BackendKind,
    pub location: Location,
    pub state: SessionState,
    pub position: u64,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HistorySearchResponse {
    pub results: Vec<HistorySearchResult>,
    pub searched_sessions: usize,
    pub unavailable_sessions: usize,
    pub truncated: bool,
}

struct SessionMatches {
    items: Vec<HistoryMatch>,
    truncated: bool,
}

pub fn search(sessions: &[Session], query: &str) -> Result<HistorySearchResponse> {
    validate_query(query)?;
    let query = query.as_bytes();
    let mut results = Vec::new();
    let mut unavailable_sessions = 0usize;
    let mut truncated = false;
    let mut remote_by_host: HashMap<&str, Vec<&Session>> = HashMap::new();

    for session in sessions {
        match session.location {
            Location::Local => {
                let path = session
                    .host_log_path
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        crate::config::logs_dir().join(format!("{}.log", session.id))
                    });
                match search_local_file(&path, query) {
                    Ok(found) => {
                        truncated |= found.truncated;
                        append_results(&mut results, session, found.items);
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(_) => unavailable_sessions += 1,
                }
            }
            Location::Remote => {
                let Some(host) = session.ssh_host.as_deref() else {
                    unavailable_sessions += 1;
                    continue;
                };
                remote_by_host.entry(host).or_default().push(session);
            }
        }
    }

    for (host, host_sessions) in remote_by_host {
        for batch in host_sessions.chunks(REMOTE_SESSION_BATCH) {
            match search_remote_batch(host, batch, query) {
                Ok((found, batch_truncated)) => {
                    truncated |= batch_truncated;
                    for session in batch {
                        if let Some(items) = found.get(&session.id) {
                            append_results(&mut results, session, items.clone());
                        }
                    }
                }
                Err(_) => unavailable_sessions += batch.len(),
            }
        }
    }

    // Session activity is the only reliable cross-log clock. Within one
    // session, byte/line position gives deterministic newest-first ordering.
    let activity: HashMap<&str, i64> = sessions
        .iter()
        .map(|session| (session.id.as_str(), session.last_activity))
        .collect();
    results.sort_by(|left, right| {
        activity
            .get(right.session_id.as_str())
            .cmp(&activity.get(left.session_id.as_str()))
            .then_with(|| right.position.cmp(&left.position))
    });
    if results.len() > MAX_RESULTS {
        results.truncate(MAX_RESULTS);
        truncated = true;
    }

    Ok(HistorySearchResponse {
        results,
        searched_sessions: sessions.len(),
        unavailable_sessions,
        truncated,
    })
}

pub(crate) fn read_tail(session: &Session, limit: usize) -> Result<Vec<u8>> {
    match session.location {
        Location::Local => {
            let path = session
                .host_log_path
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| crate::config::logs_dir().join(format!("{}.log", session.id)));
            let mut file = File::open(path)?;
            let len = file.metadata()?.len();
            file.seek(SeekFrom::Start(len.saturating_sub(limit as u64)))?;
            let mut bytes = Vec::with_capacity(limit.min(len as usize));
            file.take(limit as u64).read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        Location::Remote => {
            let host = session
                .ssh_host
                .as_deref()
                .context("remote session missing SSH host")?;
            crate::ssh::validate_host(host)?;
            let path = session
                .host_log_path
                .clone()
                .unwrap_or_else(|| format!("/tmp/slide-{}.log", session.id));
            let remote = [
                "tail".to_string(),
                "-c".to_string(),
                limit.to_string(),
                path,
            ]
            .iter()
            .map(|part| shell_quote(part))
            .collect::<Vec<_>>()
            .join(" ");
            let mut command = Command::new("ssh");
            command
                .args(["-o", "BatchMode=yes"])
                .args(crate::ssh::ssh_args())
                .arg(host)
                .arg(remote);
            let output =
                crate::process::run_bounded(command, limit, REMOTE_STDERR_LIMIT, REMOTE_TIMEOUT)?;
            if output.timed_out
                || output.stdout_truncated
                || output.stderr_truncated
                || !output.success
            {
                bail!("remote session history is unavailable");
            }
            Ok(output.stdout)
        }
    }
}

fn append_results(
    output: &mut Vec<HistorySearchResult>,
    session: &Session,
    matches: Vec<HistoryMatch>,
) {
    output.extend(
        matches
            .into_iter()
            .map(|(position, snippet)| HistorySearchResult {
                session_id: session.id.clone(),
                session_name: session.name.clone(),
                backend: session.backend,
                location: session.location,
                state: session.state,
                position,
                snippet,
            }),
    );
}

fn validate_query(query: &str) -> Result<()> {
    let length = query.chars().count();
    if length < 2 {
        bail!("search query must contain at least 2 characters");
    }
    if length > MAX_QUERY_CHARS {
        bail!("search query must be at most {MAX_QUERY_CHARS} characters");
    }
    if query.chars().any(char::is_control) {
        bail!("search query must be a single line without control characters");
    }
    Ok(())
}

/// Stream a local log with fixed memory. The overlap catches a query split
/// across read boundaries and supplies context for snippets; only the newest
/// few distinct matches are retained even when a TUI redraws the same frame.
fn search_local_file(path: &Path, query: &[u8]) -> std::io::Result<SessionMatches> {
    let mut file = File::open(path)?;
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    let mut carry = Vec::new();
    let mut total_read = 0u64;
    let mut items = VecDeque::with_capacity(MAX_RESULTS_PER_SESSION + 1);
    let mut truncated = false;

    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let previous_total = total_read;
        total_read = total_read.saturating_add(count as u64);
        let carry_len = carry.len();
        let base = previous_total.saturating_sub(carry_len as u64);
        let mut window = carry;
        window.extend_from_slice(&chunk[..count]);
        let report_from = previous_total.saturating_sub(query.len().saturating_sub(1) as u64);

        for index in find_matches(&window, query) {
            let position = base.saturating_add(index as u64);
            if position < report_from {
                continue;
            }
            let snippet = local_snippet(&window, index, query.len(), base > 0);
            if snippet.is_empty()
                || items
                    .back()
                    .is_some_and(|(_, previous)| previous == &snippet)
            {
                continue;
            }
            if items.len() == MAX_RESULTS_PER_SESSION {
                items.pop_front();
                truncated = true;
            }
            items.push_back((position, snippet));
        }

        let keep = SEARCH_OVERLAP_BYTES.max(query.len() + SNIPPET_CONTEXT_BYTES);
        carry = window[window.len().saturating_sub(keep)..].to_vec();
    }

    Ok(SessionMatches {
        items: items.into_iter().rev().collect(),
        truncated,
    })
}

fn local_snippet(window: &[u8], index: usize, query_len: usize, has_prefix: bool) -> String {
    let match_end = (index + query_len).min(window.len());
    let record_start = window[..index]
        .iter()
        .rposition(|byte| matches!(byte, b'\n' | b'\r'))
        .map(|position| position + 1)
        .unwrap_or(0);
    let record_end = window[match_end..]
        .iter()
        .position(|byte| matches!(byte, b'\n' | b'\r'))
        .map(|position| match_end + position)
        .unwrap_or(window.len());
    let snippet_start = record_start.max(index.saturating_sub(SNIPPET_CONTEXT_BYTES));
    let snippet_end = record_end.min(match_end.saturating_add(SNIPPET_CONTEXT_BYTES));
    compact_snippet(
        &window[snippet_start..snippet_end],
        snippet_start > record_start || (record_start == 0 && has_prefix),
        snippet_end < record_end,
    )
}

fn find_matches(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    // Case-folded Boyer-Moore-Horspool keeps repetitive multi-gigabyte logs
    // from degrading to O(log_bytes * query_bytes) comparisons.
    let mut skip = [needle.len(); 256];
    for (index, byte) in needle[..needle.len() - 1].iter().enumerate() {
        skip[fold_ascii(*byte) as usize] = needle.len() - index - 1;
    }
    let mut found = Vec::new();
    let mut index = 0usize;
    while index + needle.len() <= haystack.len() {
        let candidate = &haystack[index..index + needle.len()];
        if bytes_equal_folded(candidate, needle) {
            found.push(index);
            index += needle.len();
        } else {
            index += skip[fold_ascii(candidate[needle.len() - 1]) as usize].max(1);
        }
    }
    found
}

fn bytes_equal_folded(left: &[u8], right: &[u8]) -> bool {
    left.iter()
        .zip(right)
        .all(|(left, right)| fold_ascii(*left) == fold_ascii(*right))
}

fn fold_ascii(byte: u8) -> u8 {
    byte.to_ascii_lowercase()
}

fn compact_snippet(bytes: &[u8], prefix: bool, suffix: bool) -> String {
    let raw = String::from_utf8_lossy(bytes);
    let compact = crate::terminal_text::compact(&raw);
    match (prefix, suffix, compact.is_empty()) {
        (_, _, true) => compact,
        (true, true, false) => format!("…{compact}…"),
        (true, false, false) => format!("…{compact}"),
        (false, true, false) => format!("{compact}…"),
        (false, false, false) => compact,
    }
}

fn search_remote_batch(
    host: &str,
    sessions: &[&Session],
    query: &[u8],
) -> Result<(MatchesBySession, bool)> {
    crate::ssh::validate_host(host)?;
    let query = std::str::from_utf8(query)?;
    const SCRIPT: &str = r#"query=$1
shift
while [ "$#" -ge 2 ]; do
  id=$1
  path=$2
  shift 2
  printf '\036%s\n' "$id"
  if [ -r "$path" ]; then
    grep -ainF -- "$query" "$path" 2>/dev/null | tail -n 9 | cut -c 1-1000
  fi
done
"#;
    let mut parts = vec![
        "sh".to_string(),
        "-c".to_string(),
        SCRIPT.to_string(),
        "sh".into(),
        query.into(),
    ];
    for session in sessions {
        parts.push(session.id.clone());
        parts.push(
            session
                .host_log_path
                .clone()
                .unwrap_or_else(|| format!("/tmp/slide-{}.log", session.id)),
        );
    }
    let remote = parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = Command::new("ssh");
    command
        .args(["-o", "BatchMode=yes"])
        .args(crate::ssh::ssh_args())
        .arg(host)
        .arg(remote);
    let output = crate::process::run_bounded(
        command,
        REMOTE_OUTPUT_LIMIT,
        REMOTE_STDERR_LIMIT,
        REMOTE_TIMEOUT,
    )?;
    if output.timed_out || output.stdout_truncated || output.stderr_truncated || !output.success {
        bail!("remote history search unavailable");
    }
    Ok(parse_remote_output(&output.stdout, sessions))
}

fn parse_remote_output(bytes: &[u8], sessions: &[&Session]) -> (MatchesBySession, bool) {
    let known: HashMap<&str, &Session> = sessions
        .iter()
        .map(|session| (session.id.as_str(), *session))
        .collect();
    let mut current: Option<&str> = None;
    let mut found: MatchesBySession = HashMap::new();
    let mut raw_counts: HashMap<String, usize> = HashMap::new();
    let mut truncated = false;
    for line in bytes.split(|byte| *byte == b'\n') {
        if let Some(header) = line.strip_prefix(&[0x1e]) {
            let id = std::str::from_utf8(header).ok();
            current = id.filter(|id| known.contains_key(*id));
            continue;
        }
        let Some(id) = current else { continue };
        let Some(separator) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let Ok(position) = std::str::from_utf8(&line[..separator])
            .unwrap_or_default()
            .parse::<u64>()
        else {
            continue;
        };
        let snippet = compact_snippet(&line[separator + 1..], true, true);
        if snippet.is_empty() {
            continue;
        }
        let raw_count = raw_counts.entry(id.to_string()).or_default();
        *raw_count += 1;
        truncated |= *raw_count > MAX_RESULTS_PER_SESSION;
        let items = found.entry(id.to_string()).or_default();
        if items
            .last()
            .is_some_and(|(_, previous)| previous == &snippet)
        {
            continue;
        }
        if items.len() == MAX_RESULTS_PER_SESSION {
            items.remove(0);
            truncated = true;
        }
        items.push((position, snippet));
    }
    for items in found.values_mut() {
        items.reverse();
    }
    (found, truncated)
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            name: format!("session-{id}"),
            backend: BackendKind::Codex,
            location: Location::Remote,
            ssh_host: Some("host".to_string()),
            base_dir: "/tmp".to_string(),
            project_path: "/tmp/project".to_string(),
            worktree: false,
            state: SessionState::Stopped,
            created_at: 1,
            last_activity: 2,
            supervisor: crate::session::SupervisorKind::Tmux,
            host_log_path: Some(format!("/tmp/{id}.log")),
            log_offset: 0,
            backend_session_id: None,
            parent_session_id: None,
        }
    }

    #[test]
    fn local_search_is_case_insensitive_streaming_and_ansi_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.log");
        let mut file = File::create(&path).unwrap();
        file.write_all(&vec![b'x'; READ_CHUNK_BYTES - 3]).unwrap();
        file.write_all(b"NEE").unwrap();
        file.write_all(b"dle \x1b[31mresult\x1b[0m\n").unwrap();
        let matches = search_local_file(&path, b"needle").unwrap();
        assert_eq!(matches.items.len(), 1);
        assert!(matches.items[0].1.contains("NEEdle result"));
        assert!(!matches.items[0].1.contains("\x1b"));
    }

    #[test]
    fn local_tail_reads_only_the_requested_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.log");
        std::fs::write(&path, b"0123456789").unwrap();
        let mut source = session("tail");
        source.location = Location::Local;
        source.ssh_host = None;
        source.host_log_path = Some(path.to_string_lossy().into_owned());

        assert_eq!(read_tail(&source, 4).unwrap(), b"6789");
        assert_eq!(read_tail(&source, 20).unwrap(), b"0123456789");
    }

    #[test]
    fn local_search_keeps_only_newest_distinct_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.log");
        let mut file = File::create(&path).unwrap();
        for index in 0..MAX_RESULTS_PER_SESSION + 3 {
            writeln!(file, "needle result {index}").unwrap();
        }
        let matches = search_local_file(&path, b"needle").unwrap();
        assert_eq!(matches.items.len(), MAX_RESULTS_PER_SESSION);
        assert!(matches.truncated);
        assert!(matches.items[0].1.contains("result 10"));
    }

    #[test]
    fn folded_matcher_skips_repetitive_non_matches_without_missing_the_tail() {
        let mut log = vec![b'a'; 2 * READ_CHUNK_BYTES];
        log.extend_from_slice(b"Needle");
        assert_eq!(find_matches(&log, b"needle"), [2 * READ_CHUNK_BYTES]);
    }

    #[test]
    fn whole_history_orders_sessions_by_activity() {
        let dir = tempfile::tempdir().unwrap();
        let older_path = dir.path().join("older.log");
        let newer_path = dir.path().join("newer.log");
        std::fs::write(&older_path, b"needle in older\n").unwrap();
        std::fs::write(&newer_path, b"needle in newer\n").unwrap();
        let mut older = session("older");
        older.location = Location::Local;
        older.ssh_host = None;
        older.host_log_path = Some(older_path.to_string_lossy().into_owned());
        older.last_activity = 10;
        let mut newer = session("newer");
        newer.location = Location::Local;
        newer.ssh_host = None;
        newer.host_log_path = Some(newer_path.to_string_lossy().into_owned());
        newer.last_activity = 20;

        let response = search(&[older, newer], "needle").unwrap();
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.results[0].session_id, "newer");
        assert_eq!(response.searched_sessions, 2);
        assert_eq!(response.unavailable_sessions, 0);
    }

    #[test]
    fn remote_output_is_framed_per_session_and_bounded() {
        let one = session("one");
        let two = session("two");
        let sessions = [&one, &two];
        let output = b"\x1eone\n2:first needle\n9:last needle\n\x1etwo\n4:other needle\n";
        let (found, truncated) = parse_remote_output(output, &sessions);
        assert!(!truncated);
        assert_eq!(found["one"][0].0, 9);
        assert_eq!(found["two"][0].0, 4);
    }

    #[test]
    fn query_validation_rejects_tiny_multiline_and_oversized_queries() {
        assert!(validate_query("x").is_err());
        assert!(validate_query("two\nlines").is_err());
        assert!(validate_query(&"x".repeat(MAX_QUERY_CHARS + 1)).is_err());
        assert!(validate_query("good query").is_ok());
    }
}
