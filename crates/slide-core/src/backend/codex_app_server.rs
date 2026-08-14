//! Small, bounded client for Codex's JSONL app-server protocol.

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const APP_SERVER_COMMAND: &str = "codex app-server -c 'mcp_servers={}' --stdio";

/// Start Codex locally or through the same SSH transport as a remote session.
/// MCP startup is disabled because these metadata queries do not need tools.
fn command(ssh_host: Option<&str>) -> Result<Command> {
    if let Some(host) = ssh_host {
        crate::ssh::validate_host(host).context("invalid ssh host")?;
        let mut command = Command::new("ssh");
        command.args(["-o", "BatchMode=yes"]);
        command.args(crate::ssh::ssh_args());
        command.arg(host).arg(format!(
            "exec \"${{SHELL:-/bin/sh}}\" -lc {}",
            shell_quote(APP_SERVER_COMMAND)
        ));
        Ok(command)
    } else {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        let mut command = Command::new(shell);
        command.args(["-lc", APP_SERVER_COMMAND]);
        Ok(command)
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

pub(crate) struct Client {
    child: ChildGuard,
    stdin: Option<ChildStdin>,
    responses: Receiver<Value>,
    reader: Option<JoinHandle<()>>,
    deadline: Instant,
    next_id: u64,
}

impl Client {
    pub(crate) fn connect(
        ssh_host: Option<&str>,
        experimental_api: bool,
        max_response_line_bytes: usize,
    ) -> Result<Self> {
        Self::with_command(
            command(ssh_host)?,
            experimental_api,
            max_response_line_bytes,
        )
    }

    pub(crate) fn with_command(
        mut command: Command,
        experimental_api: bool,
        max_response_line_bytes: usize,
    ) -> Result<Self> {
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Diagnostics may contain local paths or account details. They
            // are neither needed nor safe to echo through Slide's API.
            .stderr(Stdio::null())
            .spawn()
            .context("start Codex app-server")?;
        let mut child = ChildGuard::new(child);
        let stdin = child
            .child_mut()
            .stdin
            .take()
            .context("open Codex app-server stdin")?;
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .context("open Codex app-server stdout")?;
        let (tx, responses) = std::sync::mpsc::channel();
        let reader =
            std::thread::spawn(move || read_responses(stdout, tx, max_response_line_bytes));

        let mut client = Self {
            child,
            stdin: Some(stdin),
            responses,
            reader: Some(reader),
            deadline: Instant::now() + APP_SERVER_TIMEOUT,
            next_id: 1,
        };
        let _: Value = client.request(
            "initialize",
            serde_json::json!({
                "clientInfo": { "name": "slide", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": if experimental_api {
                    serde_json::json!({ "experimentalApi": true })
                } else {
                    serde_json::json!({})
                }
            }),
        )?;
        client.notify("initialized", serde_json::json!({}))?;
        Ok(client)
    }

    pub(crate) fn request<T: DeserializeOwned>(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> Result<T> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(serde_json::json!({
            "id": id,
            "method": method,
            "params": params
        }))?;
        let response = self.response_with_id(id)?;
        serde_json::from_value(
            response
                .get("result")
                .cloned()
                .context("Codex app-server response omitted result")?,
        )
        .with_context(|| format!("decode Codex {method} response"))
    }

    fn notify(&mut self, method: &'static str, params: Value) -> Result<()> {
        self.write(serde_json::json!({ "method": method, "params": params }))
    }

    fn write(&mut self, value: Value) -> Result<()> {
        let writer = self
            .stdin
            .as_mut()
            .context("Codex app-server stdin is closed")?;
        serde_json::to_writer(&mut *writer, &value).context("encode Codex app-server request")?;
        std::io::Write::write_all(writer, b"\n").context("write Codex app-server request")?;
        std::io::Write::flush(writer).context("flush Codex app-server request")
    }

    fn response_with_id(&self, wanted_id: u64) -> Result<Value> {
        loop {
            let remaining = self
                .deadline
                .checked_duration_since(Instant::now())
                .context("Codex app-server query timed out")?;
            let value = self
                .responses
                .recv_timeout(remaining)
                .context("Codex app-server stopped before replying")?;
            if value.get("id").and_then(Value::as_u64) != Some(wanted_id) {
                continue;
            }
            if value.get("error").is_some_and(|error| !error.is_null()) {
                bail!("Codex app-server request failed");
            }
            return Ok(value);
        }
    }
}

fn read_responses(
    stdout: impl std::io::Read,
    tx: std::sync::mpsc::Sender<Value>,
    max_line_bytes: usize,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        let buffer = match reader.fill_buf() {
            Ok([]) | Err(_) => return,
            Ok(buffer) => buffer,
        };
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        if line.len().saturating_add(consumed) > max_line_bytes {
            return;
        }
        line.extend_from_slice(&buffer[..consumed]);
        reader.consume(consumed);
        if newline.is_none() {
            continue;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if let Ok(value) = serde_json::from_slice(&line) {
            if tx.send(value).is_err() {
                return;
            }
        }
        line.clear();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stdin.take();
        self.child.shutdown();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_command_uses_a_login_shell_and_disables_mcp_servers() {
        let command = command(None).unwrap();
        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["-lc", APP_SERVER_COMMAND]
        );
    }

    #[test]
    fn remote_command_keeps_host_and_program_separate() {
        let command = command(Some("spark1")).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(command.get_program(), "ssh");
        assert!(args.iter().any(|arg| arg == "spark1"));
        assert_eq!(
            args.last().unwrap(),
            &format!(
                "exec \"${{SHELL:-/bin/sh}}\" -lc {}",
                shell_quote(APP_SERVER_COMMAND)
            )
        );
    }

    #[test]
    fn response_reader_enforces_its_caller_bound() {
        let mut input = serde_json::to_vec(&serde_json::json!({
            "id": 1,
            "padding": "x".repeat(4096)
        }))
        .unwrap();
        input.push(b'\n');

        let (tx, rx) = std::sync::mpsc::channel();
        read_responses(input.as_slice(), tx, input.len());
        assert_eq!(rx.recv().unwrap()["id"], 1);

        let (tx, rx) = std::sync::mpsc::channel();
        read_responses(input.as_slice(), tx, input.len() - 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn transcript_bound_accepts_lines_that_quota_bound_rejects() {
        let mut input = serde_json::to_vec(&serde_json::json!({
            "id": 1,
            "padding": "x".repeat(2_500_000)
        }))
        .unwrap();
        input.push(b'\n');

        let (tx, rx) = std::sync::mpsc::channel();
        read_responses(input.as_slice(), tx, 16 * 1024 * 1024);
        assert_eq!(rx.recv().unwrap()["id"], 1);

        let (tx, rx) = std::sync::mpsc::channel();
        read_responses(input.as_slice(), tx, 256 * 1024);
        assert!(rx.try_recv().is_err());
    }
}
