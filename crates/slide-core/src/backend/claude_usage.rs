//! Best-effort Claude Code subscription usage adapter.
//!
//! Claude Code exposes these same windows through `/usage` and its documented
//! status-line fields. The CLI does not provide a standalone JSON usage
//! command, so this adapter asks the CLI's own OAuth-backed usage endpoint for
//! the small window payload and never forwards credentials or raw responses.

use super::{parse_timestamp_ms, ProviderRateLimit};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const USAGE_TIMEOUT: Duration = Duration::from_secs(15);
const USAGE_OUTPUT_LIMIT: usize = 64 * 1024;
const USAGE_SCRIPT: &str = r#"
set -eu
token="${CLAUDE_CODE_OAUTH_TOKEN:-}"
if [ -z "$token" ] && command -v security >/dev/null 2>&1; then
  credentials="$(security find-generic-password -s 'Claude Code-credentials' -w 2>/dev/null || true)"
  if [ -n "$credentials" ] && command -v jq >/dev/null 2>&1; then
    token="$(printf '%s' "$credentials" | jq -r '.claudeAiOauth.accessToken // empty' 2>/dev/null || true)"
  fi
fi
if [ -z "$token" ] && [ -r "$HOME/.claude/.credentials.json" ]; then
  if command -v jq >/dev/null 2>&1; then
    token="$(jq -r '.claudeAiOauth.accessToken // empty' "$HOME/.claude/.credentials.json" 2>/dev/null || true)"
  elif command -v python3 >/dev/null 2>&1; then
    token="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("claudeAiOauth",{}).get("accessToken", ""))' "$HOME/.claude/.credentials.json" 2>/dev/null || true)"
  fi
fi
[ -n "$token" ] || exit 3
exec curl --silent --show-error --max-time 12 \
  -H "Authorization: Bearer $token" \
  -H 'Accept: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  https://api.anthropic.com/api/oauth/usage
"#;

const WINDOWS: [(&str, &str, Option<u64>); 5] = [
    ("five_hour", "5-hour", Some(300)),
    ("seven_day", "7-day", Some(10_080)),
    ("seven_day_opus", "7-day Opus", Some(10_080)),
    ("seven_day_sonnet", "7-day Sonnet", Some(10_080)),
    ("seven_day_overage_included", "7-day overage", Some(10_080)),
];

pub(crate) fn query(host: Option<&str>) -> Result<Vec<ProviderRateLimit>> {
    query_with_command(command(host)?)
}

fn query_with_command(command: Command) -> Result<Vec<ProviderRateLimit>> {
    let output =
        crate::process::run_bounded(command, USAGE_OUTPUT_LIMIT, 16 * 1024, USAGE_TIMEOUT)?;
    if !output.success || output.timed_out || output.stdout_truncated {
        anyhow::bail!("Claude usage query failed");
    }
    let value: Value = serde_json::from_slice(&output.stdout).context("decode Claude usage")?;
    Ok(normalize(&value))
}

fn command(host: Option<&str>) -> Result<Command> {
    if let Some(host) = host {
        crate::ssh::validate_host(host).context("invalid ssh host")?;
        let mut command = Command::new("ssh");
        command.args(["-o", "BatchMode=yes"]);
        command.args(crate::ssh::ssh_args());
        command.arg(host).arg(format!(
            "exec \"${{SHELL:-/bin/sh}}\" -lc {}",
            shell_quote(USAGE_SCRIPT)
        ));
        Ok(command)
    } else {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        let mut command = Command::new(shell);
        command.args(["-lc", USAGE_SCRIPT]);
        Ok(command)
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn normalize(value: &Value) -> Vec<ProviderRateLimit> {
    let Some(object) = value
        .get("rate_limits")
        .and_then(Value::as_object)
        .or_else(|| value.as_object())
    else {
        return Vec::new();
    };
    WINDOWS
        .into_iter()
        .filter_map(|(key, label, window_minutes)| {
            let window = object.get(key)?.as_object()?;
            let utilization = window
                .get("utilization")
                .and_then(Value::as_f64)
                .or_else(|| window.get("used_percentage").and_then(Value::as_f64))?;
            if !utilization.is_finite() {
                return None;
            }
            let percent = if utilization <= 1.0 {
                utilization * 100.0
            } else {
                utilization
            };
            let resets_at = window
                .get("resets_at")
                .and_then(parse_timestamp_ms)
                .filter(|timestamp| *timestamp >= 0);
            Some(ProviderRateLimit {
                label: label.to_string(),
                used_percent: percent.round().clamp(0.0, 100.0) as u8,
                window_minutes,
                resets_at,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_command_uses_a_login_shell_and_keeps_token_out_of_argv() {
        let command = command(None).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args[0], "-lc");
        assert!(args[1].contains("CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(args[1].contains("Bearer $token"));
    }

    #[test]
    fn remote_command_keeps_host_separate() {
        let command = command(Some("spark1")).unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(command.get_program(), "ssh");
        assert!(args.iter().any(|arg| arg == "spark1"));
        assert!(args
            .last()
            .is_some_and(|arg| arg.contains("api/oauth/usage")));
    }

    #[cfg(unix)]
    #[test]
    fn parses_documented_windows_and_discards_unknown_fields() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"printf '%s' '{"five_hour":{"utilization":0.375,"resets_at":1700000000},"seven_day":{"utilization":87.4,"resets_at":"2026-08-14T12:30:00Z"},"account_email":"private@example.com","unknown":{"utilization":0.5}}'"#,
        ]);
        let limits = query_with_command(command).unwrap();
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0].label, "5-hour");
        assert_eq!(limits[0].used_percent, 38);
        assert_eq!(limits[0].resets_at, Some(1_700_000_000_000));
        assert_eq!(limits[1].label, "7-day");
        assert_eq!(limits[1].used_percent, 87);
        assert!(limits.iter().all(|limit| !limit.label.contains("email")));
    }

    #[test]
    fn invalid_payload_is_empty() {
        assert!(normalize(&serde_json::json!({"five_hour": null})).is_empty());
        assert!(normalize(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn accepts_statusline_rate_limit_shape() {
        let limits = normalize(&serde_json::json!({
            "rate_limits": {
                "five_hour": {
                    "used_percentage": 42.5,
                    "resets_at": 1_700_000_000_i64
                }
            }
        }));
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].used_percent, 43);
        assert_eq!(limits[0].resets_at, Some(1_700_000_000_000));
    }
}
