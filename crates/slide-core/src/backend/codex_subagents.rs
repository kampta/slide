//! Structured Codex child-agent metadata adapter.
//!
//! This intentionally uses the CLI's app-server protocol instead of parsing
//! rollout files. The response is immediately narrowed to the small metadata
//! contract Slide exposes, so prompts, turns, paths, and tool output never
//! reach the HTTP layer.

use super::{SubagentSnapshot, SubagentState};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SUBAGENTS: usize = 50;
const MAX_LABEL_CHARS: usize = 160;
const MAX_STATUS_PARENTS: usize = 20;
const RECENT_TURNS_PER_PARENT: usize = 8;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadListResult {
    data: Vec<CodexThread>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexThread {
    id: String,
    parent_thread_id: Option<String>,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    status: CodexThreadStatus,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Deserialize)]
struct CodexThreadStatus {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "activeFlags")]
    active_flags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TurnsListResult {
    data: Vec<CodexTurn>,
}

#[derive(Debug, Deserialize)]
struct CodexTurn {
    items: Vec<CodexTurnItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexTurnItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    agents_states: HashMap<String, CodexAgentState>,
    #[serde(default)]
    receiver_thread_ids: Vec<String>,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Deserialize)]
struct CodexAgentState {
    status: String,
}

fn sanitized_label(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(MAX_LABEL_CHARS).collect())
    }
}

fn map_agent_state(status: &str) -> SubagentState {
    match status {
        "pendingInit" => SubagentState::Starting,
        "running" => SubagentState::Running,
        "completed" | "shutdown" => SubagentState::Completed,
        "interrupted" | "errored" | "notFound" => SubagentState::Failed,
        _ => SubagentState::Starting,
    }
}

fn map_thread(thread: CodexThread, lifecycle: Option<SubagentState>) -> Option<SubagentSnapshot> {
    let parent_id = thread.parent_thread_id?;
    let state = lifecycle.unwrap_or(match thread.status.kind.as_str() {
        "active" if thread.status.active_flags.is_empty() => SubagentState::Running,
        "active" => SubagentState::Waiting,
        "idle" | "notLoaded" => SubagentState::Completed,
        "systemError" => SubagentState::Failed,
        // New Codex versions may add a transient initialization state. Keep
        // the row visible without guessing that work has already started.
        _ => SubagentState::Starting,
    });
    Some(SubagentSnapshot {
        id: thread.id,
        parent_id,
        name: sanitized_label(thread.agent_nickname),
        role: sanitized_label(thread.agent_role),
        state,
        created_at: thread.created_at,
        updated_at: thread.updated_at,
    })
}

/// Fold collaboration items newest-first. Parent transcripts contain the
/// authoritative lifecycle of their children even when a separate app-server
/// process reports those child threads as merely `notLoaded`.
fn merge_lifecycle(result: TurnsListResult, states: &mut HashMap<String, SubagentState>) {
    for turn in result.data {
        for item in turn.items.into_iter().rev() {
            if item.kind != "collabAgentToolCall" {
                continue;
            }
            for (id, state) in item.agents_states {
                states
                    .entry(id)
                    .or_insert_with(|| map_agent_state(&state.status));
            }
            let fallback = match item.status.as_str() {
                "inProgress" => Some(SubagentState::Starting),
                "failed" => Some(SubagentState::Failed),
                _ => None,
            };
            if let Some(fallback) = fallback {
                for id in item.receiver_thread_ids {
                    states.entry(id).or_insert(fallback);
                }
            }
        }
    }
}

/// Start Codex's structured app-server locally or through the same SSH
/// transport used by a remote Slide session. The remote command is constant;
/// the validated host is passed as a distinct argv element.
fn app_server_command(ssh_host: Option<&str>) -> Result<Command> {
    if let Some(host) = ssh_host {
        crate::ssh::validate_host(host).context("invalid ssh host")?;
        let mut command = Command::new("ssh");
        command.args(["-o", "BatchMode=yes"]);
        command.args(crate::ssh::ssh_args());
        command.arg(host).arg("codex app-server --stdio");
        Ok(command)
    } else {
        let mut command = Command::new("codex");
        command.args(["app-server", "--stdio"]);
        Ok(command)
    }
}

struct ChildGuard {
    child: Child,
    shut_down: bool,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child,
            shut_down: false,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn shutdown(&mut self) {
        if !self.shut_down {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.shut_down = true;
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn response_with_id(
    rx: &std::sync::mpsc::Receiver<serde_json::Value>,
    wanted_id: u64,
    deadline: Instant,
) -> Result<serde_json::Value> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("Codex app-server query timed out")?;
        let value = rx
            .recv_timeout(remaining)
            .context("Codex app-server stopped before replying")?;
        if value.get("id").and_then(|id| id.as_u64()) != Some(wanted_id) {
            continue;
        }
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            bail!("Codex app-server request failed: {error}");
        }
        return Ok(value);
    }
}

fn write_json_line(writer: &mut impl std::io::Write, value: serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, &value).context("encode Codex app-server request")?;
    writer
        .write_all(b"\n")
        .context("write Codex app-server request")?;
    writer.flush().context("flush Codex app-server request")
}

/// Query only descendant metadata. Deserializing into `CodexThread` drops
/// preview text, transcript paths, turns, and provider-specific extras before
/// the result reaches any Slide API response.
pub(super) fn query(session_id: &str, ssh_host: Option<&str>) -> Result<Vec<SubagentSnapshot>> {
    query_with_command(app_server_command(ssh_host)?, session_id)
}

fn query_with_command(mut command: Command, session_id: &str) -> Result<Vec<SubagentSnapshot>> {
    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // App-server diagnostics can include local paths. They are neither
        // useful to the dock nor safe to echo through an HTTP error.
        .stderr(Stdio::null())
        .spawn()
        .context("start Codex app-server")?;
    let mut child = ChildGuard::new(child);
    let mut stdin = child
        .child_mut()
        .stdin
        .take()
        .context("open Codex app-server stdin")?;
    let stdout = child
        .child_mut()
        .stdout
        .take()
        .context("open Codex app-server stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout)
            .lines()
            .map_while(std::io::Result::ok)
        {
            if let Ok(value) = serde_json::from_str(&line) {
                if tx.send(value).is_err() {
                    break;
                }
            }
        }
    });

    let result = (|| {
        let deadline = Instant::now() + APP_SERVER_TIMEOUT;
        write_json_line(
            &mut stdin,
            serde_json::json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": { "name": "slide", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "experimentalApi": true }
                }
            }),
        )?;
        response_with_id(&rx, 1, deadline)?;

        write_json_line(
            &mut stdin,
            serde_json::json!({ "method": "initialized", "params": {} }),
        )?;
        write_json_line(
            &mut stdin,
            serde_json::json!({
                "id": 2,
                "method": "thread/list",
                "params": {
                    "ancestorThreadId": session_id,
                    "limit": MAX_SUBAGENTS,
                    "sortKey": "updated_at",
                    "sortDirection": "desc",
                    "sourceKinds": [
                        "subAgent",
                        "subAgentReview",
                        "subAgentCompact",
                        "subAgentThreadSpawn",
                        "subAgentOther"
                    ]
                }
            }),
        )?;
        let response = response_with_id(&rx, 2, deadline)?;
        let result: ThreadListResult = serde_json::from_value(
            response
                .get("result")
                .cloned()
                .context("Codex app-server response omitted result")?,
        )
        .context("decode Codex thread list")?;

        // Query lifecycle only for threads that are parents. A flat fan-out
        // costs one extra request (the root); nested trees add one request per
        // internal node, capped to keep pathological trees bounded.
        let mut seen_parents = HashSet::new();
        let mut parents = Vec::new();
        seen_parents.insert(session_id.to_string());
        parents.push(session_id.to_string());
        for thread in &result.data {
            if let Some(parent) = thread.parent_thread_id.as_ref() {
                if seen_parents.insert(parent.clone()) {
                    parents.push(parent.clone());
                }
            }
        }
        parents.truncate(MAX_STATUS_PARENTS);

        let mut lifecycle = HashMap::new();
        for (index, parent_id) in parents.into_iter().enumerate() {
            let request_id = 3 + index as u64;
            write_json_line(
                &mut stdin,
                serde_json::json!({
                    "id": request_id,
                    "method": "thread/turns/list",
                    "params": {
                        "threadId": parent_id,
                        "limit": RECENT_TURNS_PER_PARENT,
                        "sortDirection": "desc",
                        "itemsView": "full"
                    }
                }),
            )?;
            let response = response_with_id(&rx, request_id, deadline)?;
            let turns: TurnsListResult = serde_json::from_value(
                response
                    .get("result")
                    .cloned()
                    .context("Codex turns response omitted result")?,
            )
            .context("decode Codex collaboration lifecycle")?;
            merge_lifecycle(turns, &mut lifecycle);
        }

        Ok(result
            .data
            .into_iter()
            .filter_map(|thread| {
                let state = lifecycle.get(&thread.id).copied();
                map_thread(thread, state)
            })
            .take(MAX_SUBAGENTS)
            .collect())
    })();

    drop(stdin);
    child.shutdown();
    let _ = reader.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_codex_thread_status_without_exposing_content() {
        let thread: CodexThread = serde_json::from_value(serde_json::json!({
            "id": "child-1",
            "parentThreadId": "parent-1",
            "agentNickname": "worker",
            "agentRole": "tests",
            "status": { "type": "active", "activeFlags": [] },
            "createdAt": 10,
            "updatedAt": 20,
            "preview": "must never leave the adapter",
            "path": "/private/transcript.jsonl",
            "turns": [{ "secret": true }]
        }))
        .unwrap();
        let snapshot = map_thread(thread, None).unwrap();
        assert_eq!(snapshot.id, "child-1");
        assert_eq!(snapshot.parent_id, "parent-1");
        assert_eq!(snapshot.name.as_deref(), Some("worker"));
        assert_eq!(snapshot.role.as_deref(), Some("tests"));
        assert_eq!(snapshot.state, SubagentState::Running);
        let public = serde_json::to_value(snapshot).unwrap();
        assert!(public.get("preview").is_none());
        assert!(public.get("path").is_none());
        assert!(public.get("turns").is_none());
    }

    #[test]
    fn maps_waiting_and_terminal_statuses() {
        fn state(kind: &str, flags: &[&str]) -> SubagentState {
            map_thread(
                CodexThread {
                    id: "child".into(),
                    parent_thread_id: Some("parent".into()),
                    agent_nickname: None,
                    agent_role: None,
                    status: CodexThreadStatus {
                        kind: kind.into(),
                        active_flags: flags.iter().map(|s| (*s).into()).collect(),
                    },
                    created_at: 0,
                    updated_at: 0,
                },
                None,
            )
            .unwrap()
            .state
        }
        assert_eq!(
            state("active", &["waitingOnUserInput"]),
            SubagentState::Waiting
        );
        assert_eq!(state("idle", &[]), SubagentState::Completed);
        assert_eq!(state("notLoaded", &[]), SubagentState::Completed);
        assert_eq!(state("systemError", &[]), SubagentState::Failed);
        assert_eq!(state("newProviderState", &[]), SubagentState::Starting);
    }

    #[test]
    fn drops_non_descendant_records_defensively() {
        let snapshot = map_thread(
            CodexThread {
                id: "root".into(),
                parent_thread_id: None,
                agent_nickname: None,
                agent_role: None,
                status: CodexThreadStatus {
                    kind: "active".into(),
                    active_flags: vec![],
                },
                created_at: 0,
                updated_at: 0,
            },
            None,
        );
        assert!(snapshot.is_none());
    }

    #[test]
    fn trims_and_bounds_provider_labels() {
        assert_eq!(
            sanitized_label(Some("  worker  ".into())).as_deref(),
            Some("worker")
        );
        assert!(sanitized_label(Some("  ".into())).is_none());
        let long = "a".repeat(MAX_LABEL_CHARS + 20);
        assert_eq!(
            sanitized_label(Some(long)).unwrap().chars().count(),
            MAX_LABEL_CHARS
        );
    }

    #[cfg(unix)]
    #[test]
    fn app_server_handshake_returns_a_sanitized_snapshot() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"
IFS= read -r initialize
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized
IFS= read -r list
case "$list" in
  *'"ancestorThreadId":"root-thread"'*)
    printf '%s\n' '{"id":2,"result":{"data":[{"id":"child","parentThreadId":"root-thread","agentNickname":"helper","agentRole":"tests","status":{"type":"notLoaded"},"createdAt":10,"updatedAt":20,"preview":"private"}]}}'
    ;;
  *)
    printf '%s\n' '{"id":2,"error":{"code":-1,"message":"missing ancestor"}}'
    ;;
esac
IFS= read -r turns
printf '%s\n' '{"id":3,"result":{"data":[{"items":[{"type":"collabAgentToolCall","agentsStates":{"child":{"status":"running","message":"private result"}},"receiverThreadIds":["child"],"status":"completed"}]}]}}'
"#,
        ]);

        let snapshots = query_with_command(command, "root-thread").unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].id, "child");
        assert_eq!(snapshots[0].parent_id, "root-thread");
        assert_eq!(snapshots[0].state, SubagentState::Running);
    }
}
