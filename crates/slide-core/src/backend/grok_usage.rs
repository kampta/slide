//! Bounded Grok ACP billing adapter.
//!
//! Grok's interactive product exposes the same weekly pool in its Usage tab.
//! The installed CLI exposes that account snapshot through its ACP billing
//! extension, so Slide asks the CLI rather than reading browser state or
//! scraping a private web page.

use super::{parse_timestamp_ms, ProviderRateLimit};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const QUERY_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_LINE_BYTES: usize = 256 * 1024;
const BILLING_METHOD: &str = "_x.ai/billing";
const GROK_COMMAND: &str = "cd /tmp && exec grok --no-memory agent --no-leader stdio";

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

struct Client {
    child: ChildGuard,
    stdin: ChildStdin,
    responses: Receiver<Value>,
    reader: Option<JoinHandle<()>>,
    deadline: Instant,
    next_id: u64,
}

impl Client {
    fn connect(host: Option<&str>) -> Result<Self> {
        Self::with_command(command(host)?)
    }

    fn with_command(mut command: Command) -> Result<Self> {
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("start Grok ACP")?;
        let mut child = ChildGuard::new(child);
        let stdin = child.child.stdin.take().context("open Grok ACP stdin")?;
        let stdout = child.child.stdout.take().context("open Grok ACP stdout")?;
        let (tx, responses) = std::sync::mpsc::sync_channel(32);
        let reader = std::thread::spawn(move || read_responses(stdout, tx));
        let mut client = Self {
            child,
            stdin,
            responses,
            reader: Some(reader),
            deadline: Instant::now() + QUERY_TIMEOUT,
            next_id: 1,
        };
        let _: Value = client.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": { "name": "slide", "version": env!("CARGO_PKG_VERSION") }
            }),
        )?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    fn request(&mut self, method: &'static str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        let response = self.response_with_id(id)?;
        if response.get("error").is_some_and(|error| !error.is_null()) {
            bail!("Grok ACP request failed");
        }
        response
            .get("result")
            .cloned()
            .context("Grok ACP response omitted result")
    }

    fn notify(&mut self, method: &'static str, params: Value) -> Result<()> {
        self.write(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    fn write(&mut self, value: Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, &value).context("encode Grok ACP request")?;
        self.stdin
            .write_all(b"\n")
            .context("write Grok ACP request")?;
        self.stdin.flush().context("flush Grok ACP request")
    }

    fn response_with_id(&self, wanted_id: u64) -> Result<Value> {
        loop {
            let remaining = self
                .deadline
                .checked_duration_since(Instant::now())
                .context("Grok usage query timed out")?;
            let value = self
                .responses
                .recv_timeout(remaining)
                .context("Grok ACP stopped before replying")?;
            if value.get("id").and_then(Value::as_u64) == Some(wanted_id) {
                return Ok(value);
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.child.shutdown();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn read_responses(stdout: impl std::io::Read, tx: SyncSender<Value>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        loop {
            let buffer = match reader.fill_buf() {
                Ok([]) | Err(_) => return,
                Ok(buffer) => buffer,
            };
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(buffer.len(), |index| index + 1);
            if line
                .len()
                .checked_add(consumed)
                .is_none_or(|length| length > MAX_RESPONSE_LINE_BYTES)
            {
                return;
            }
            line.extend_from_slice(&buffer[..consumed]);
            reader.consume(consumed);
            if newline.is_some() {
                break;
            }
        }
        let end = line
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace())
            .map_or(0, |index| index + 1);
        if let Ok(value) = serde_json::from_slice::<Value>(&line[..end]) {
            if tx.send(value).is_err() {
                return;
            }
        }
    }
}

pub(crate) fn query(host: Option<&str>) -> Result<Vec<ProviderRateLimit>> {
    query_with_client(Client::connect(host)?)
}

fn query_with_client(mut client: Client) -> Result<Vec<ProviderRateLimit>> {
    let response = client.request(BILLING_METHOD, json!({}))?;
    let result = response.get("result").unwrap_or(&response);
    Ok(normalize(result))
}

fn command(host: Option<&str>) -> Result<Command> {
    if let Some(host) = host {
        crate::ssh::validate_host(host).context("invalid ssh host")?;
        let mut command = Command::new("ssh");
        command.args(["-o", "BatchMode=yes"]);
        command.args(crate::ssh::ssh_args());
        command.arg(host).arg(format!(
            "exec \"${{SHELL:-/bin/sh}}\" -lc {}",
            shell_quote(GROK_COMMAND)
        ));
        Ok(command)
    } else {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        let mut command = Command::new(shell);
        command.args(["-lc", GROK_COMMAND]);
        Ok(command)
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn normalize(value: &Value) -> Vec<ProviderRateLimit> {
    let config = value.get("config").unwrap_or(value);
    let Some(percent) = billing_percent(config) else {
        return Vec::new();
    };
    let period = config.get("currentPeriod");
    let reset = period
        .and_then(|period| period.get("end"))
        .and_then(parse_timestamp_ms)
        .or_else(|| config.get("billingPeriodEnd").and_then(parse_timestamp_ms))
        .filter(|timestamp| *timestamp >= 0);
    let window_minutes = period.and_then(|period| {
        let start = period.get("start").and_then(parse_timestamp_ms)?;
        let end = period.get("end").and_then(parse_timestamp_ms)?;
        u64::try_from(end.checked_sub(start)?.max(0) / 60_000).ok()
    });
    let label = period
        .and_then(|period| period.get("type"))
        .and_then(Value::as_str)
        .map(humanize_period)
        .unwrap_or_else(|| "Account usage".to_string());
    vec![ProviderRateLimit {
        label,
        used_percent: percent,
        window_minutes,
        resets_at: reset,
    }]
}

fn billing_percent(config: &Value) -> Option<u8> {
    let percent = config
        .get("creditUsagePercent")
        .and_then(number_value)
        .or_else(|| {
            let used = config.get("used").and_then(number_value)?;
            let limit = config.get("monthlyLimit").and_then(number_value)?;
            (limit > 0.0).then_some(used / limit * 100.0)
        })?;
    if !percent.is_finite() {
        return None;
    }
    let percent = if percent <= 1.0 {
        percent * 100.0
    } else {
        percent
    };
    Some(percent.round().clamp(0.0, 100.0) as u8)
}

fn number_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.get("val").and_then(Value::as_f64))
}

fn humanize_period(value: &str) -> String {
    let mut label = value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut characters = word.chars();
            characters
                .next()
                .map(|first| {
                    first
                        .to_uppercase()
                        .chain(characters.flat_map(char::to_lowercase))
                        .collect::<String>()
                })
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    if label.is_empty() {
        label = "Account".to_string();
    }
    format!("{label} usage")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn local_command_uses_neutral_cwd_and_no_shared_leader() {
        let command = command(None).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args, ["-lc", GROK_COMMAND]);
    }

    #[test]
    fn remote_command_keeps_host_separate() {
        let command = command(Some("spark2")).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(command.get_program(), "ssh");
        assert!(args.iter().any(|arg| arg == "spark2"));
        assert!(args
            .last()
            .is_some_and(|arg| arg.contains("grok --no-memory")));
    }

    #[cfg(unix)]
    #[test]
    fn billing_handshake_normalizes_weekly_usage_without_exposing_account_data() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
IFS= read -r initialized
IFS= read -r billing
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"result":{"config":{"creditUsagePercent":37.5,"currentPeriod":{"type":"weekly","start":"2026-08-07T00:00:00Z","end":"2026-08-14T00:00:00Z"},"accountEmail":"private@example.com"},"subscription_tier":"max"}}}'
"#,
        ]);
        let limits = query_with_client(Client::with_command(command).unwrap()).unwrap();
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].label, "Weekly usage");
        assert_eq!(limits[0].used_percent, 38);
        assert_eq!(limits[0].window_minutes, Some(10_080));
        assert_eq!(
            limits[0].resets_at,
            parse_timestamp_ms(&json!("2026-08-14T00:00:00Z"))
        );
    }

    #[test]
    fn legacy_billing_values_are_supported() {
        let limits = normalize(&json!({
            "config": {
                "used": { "val": 25.0 },
                "monthlyLimit": { "val": 100.0 },
                "billingPeriodEnd": 1_700_000_000
            }
        }));
        assert_eq!(limits[0].used_percent, 25);
        assert_eq!(limits[0].resets_at, Some(1_700_000_000_000));
    }

    #[test]
    fn response_reader_bounds_json_lines_before_parsing() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let oversized = format!(
            "{{\"padding\":\"{}\"}}\n",
            "x".repeat(MAX_RESPONSE_LINE_BYTES)
        );
        read_responses(Cursor::new(oversized), tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn response_reader_accepts_small_json_lines() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        read_responses(Cursor::new(b"{\"id\":1}\n"), tx);
        assert_eq!(
            rx.recv().unwrap().get("id").and_then(Value::as_u64),
            Some(1)
        );
    }
}
