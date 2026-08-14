use crate::backend::{codex_app_server::Client as CodexClient, BackendKind};
use crate::process::BoundedOutput;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CACHE_TTL: Duration = Duration::from_secs(60);
// Login shells + cold CLI startups (new binary, codesign, dyld) regularly
// exceed a few seconds on macOS. Five seconds was too tight and a single
// timeout used to hard-block create/resume for the full cache TTL.
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const PROBE_OUTPUT_LIMIT: usize = 16 * 1024;
const MAX_CACHE_TARGETS: usize = 64;
const MAX_RATE_LIMITS: usize = 16;
const MAX_RATE_LIMIT_NAME_CHARS: usize = 80;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    Ready,
    Missing,
    Unauthenticated,
    Broken,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeRateLimit {
    pub label: String,
    pub used_percent: u8,
    pub window_minutes: Option<u64>,
    /// Unix timestamp in milliseconds, matching `checked_at`.
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeDiagnostic {
    pub backend: BackendKind,
    pub label: &'static str,
    pub status: RuntimeStatus,
    pub available: bool,
    pub installed: bool,
    pub authenticated: Option<bool>,
    pub version: Option<String>,
    pub message: String,
    pub action: Option<String>,
    pub last_error: Option<String>,
    pub rate_limits: Vec<RuntimeRateLimit>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeCapability {
    pub available: bool,
    pub required: bool,
    pub version: Option<String>,
    pub message: String,
    pub action: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuntimeDiagnosticsSnapshot {
    pub target: String,
    pub checked_at: i64,
    pub backends: Vec<RuntimeDiagnostic>,
    pub tmux: RuntimeCapability,
}

struct CachedSnapshot {
    fetched_at: Instant,
    value: RuntimeDiagnosticsSnapshot,
    rate_limits: Option<CachedRateLimits>,
}

struct CachedRateLimits {
    fetched_at: Instant,
    value: Vec<RuntimeRateLimit>,
}

#[derive(Default)]
pub struct RuntimeDiagnosticsCache {
    snapshots: Mutex<HashMap<String, CachedSnapshot>>,
    query_locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    last_errors: Mutex<HashMap<String, String>>,
}

impl RuntimeDiagnosticsCache {
    /// Full UI diagnostics, including best-effort account limits. Preflight
    /// deliberately uses `get` instead so create/resume never starts Codex's
    /// app-server or waits on a network-backed quota request.
    pub fn diagnostics(
        &self,
        host: Option<&str>,
        refresh: bool,
    ) -> Result<RuntimeDiagnosticsSnapshot> {
        let mut snapshot = self.get(host, refresh)?;
        let key = target_key(host);
        self.enrich_rate_limits_with(&mut snapshot, &key, refresh, || {
            query_codex_rate_limits(host)
        });
        Ok(snapshot)
    }

    pub fn get(&self, host: Option<&str>, refresh: bool) -> Result<RuntimeDiagnosticsSnapshot> {
        if let Some(host) = host {
            crate::ssh::validate_configured_host(host)?;
        }
        let key = target_key(host);
        if !refresh {
            if let Some(value) = self.cached(&key) {
                return Ok(value);
            }
        }
        let query_lock = self.query_lock(&key);
        let _query_guard = query_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !refresh {
            if let Some(value) = self.cached(&key) {
                return Ok(value);
            }
        }

        let mut snapshot = probe_snapshot(host)?;
        let errors = self
            .last_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for diagnostic in &mut snapshot.backends {
            diagnostic.last_error = errors.get(&error_key(&key, diagnostic.backend)).cloned();
        }
        drop(errors);
        self.cache_snapshot(key, snapshot.clone());
        Ok(snapshot)
    }

    fn cache_snapshot(&self, key: String, snapshot: RuntimeDiagnosticsSnapshot) {
        let evicted = {
            let mut snapshots = self
                .snapshots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            snapshots.retain(|_, cached| cached.fetched_at.elapsed() < CACHE_TTL);
            let evicted = if snapshots.len() >= MAX_CACHE_TARGETS && !snapshots.contains_key(&key) {
                snapshots
                    .iter()
                    .max_by_key(|(_, cached)| cached.fetched_at.elapsed())
                    .map(|(target, _)| target.clone())
            } else {
                None
            };
            if let Some(target) = evicted.as_deref() {
                snapshots.remove(target);
            }
            snapshots.insert(
                key,
                CachedSnapshot {
                    fetched_at: Instant::now(),
                    value: snapshot.clone(),
                    rate_limits: None,
                },
            );
            evicted
        };
        if let Some(target) = evicted {
            self.last_errors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|key, _| !key.starts_with(&format!("{target}:")));
        }
    }

    pub fn preflight(&self, backend: BackendKind, host: Option<&str>) -> Result<()> {
        let snapshot = self.get(host, false)?;
        validate_preflight(&snapshot, backend)
    }

    pub fn record_launch_failure(&self, backend: BackendKind, host: Option<&str>) {
        let key = target_key(host);
        let message =
            "The last backend launch failed. Check the daemon log for details.".to_string();
        self.last_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(error_key(&key, backend), message.clone());
        if let Some(snapshot) = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(&key)
        {
            if let Some(diagnostic) = snapshot
                .value
                .backends
                .iter_mut()
                .find(|diagnostic| diagnostic.backend == backend)
            {
                diagnostic.last_error = Some(message);
            }
        }
    }

    pub fn clear_launch_failure(&self, backend: BackendKind, host: Option<&str>) {
        let key = target_key(host);
        self.last_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&error_key(&key, backend));
        if let Some(snapshot) = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(&key)
        {
            if let Some(diagnostic) = snapshot
                .value
                .backends
                .iter_mut()
                .find(|diagnostic| diagnostic.backend == backend)
            {
                diagnostic.last_error = None;
            }
        }
    }

    fn cached(&self, key: &str) -> Option<RuntimeDiagnosticsSnapshot> {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .filter(|cached| cached.fetched_at.elapsed() < CACHE_TTL)
            .map(|cached| cached.value.clone())
    }

    fn query_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .query_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            locks.insert(key.to_string(), Arc::downgrade(&lock));
            lock
        }
    }

    fn cached_rate_limits(&self, key: &str) -> Option<Vec<RuntimeRateLimit>> {
        self.snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .and_then(|cached| cached.rate_limits.as_ref())
            .filter(|cached| cached.fetched_at.elapsed() < CACHE_TTL)
            .map(|cached| cached.value.clone())
    }

    fn cache_rate_limits(&self, key: &str, value: Vec<RuntimeRateLimit>) {
        if let Some(cached) = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(key)
        {
            cached.rate_limits = Some(CachedRateLimits {
                fetched_at: Instant::now(),
                value,
            });
        }
    }

    fn enrich_rate_limits_with(
        &self,
        snapshot: &mut RuntimeDiagnosticsSnapshot,
        key: &str,
        refresh: bool,
        probe: impl FnOnce() -> Result<Vec<RuntimeRateLimit>>,
    ) {
        let should_probe = snapshot.backends.iter().any(|diagnostic| {
            diagnostic.backend == BackendKind::Codex
                && diagnostic.status == RuntimeStatus::Ready
                && diagnostic.installed
                && diagnostic.authenticated != Some(false)
                && (diagnostic.authenticated == Some(true) || diagnostic.version.is_some())
        });
        if !should_probe {
            return;
        }
        if !refresh {
            if let Some(rate_limits) = self.cached_rate_limits(key) {
                set_rate_limits(snapshot, rate_limits);
                return;
            }
        }

        let query_lock = self.query_lock(&format!("{key}:rate-limits"));
        let _query_guard = query_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !refresh {
            if let Some(rate_limits) = self.cached_rate_limits(key) {
                set_rate_limits(snapshot, rate_limits);
                return;
            }
        }

        let rate_limits = probe().unwrap_or_default();
        self.cache_rate_limits(key, rate_limits.clone());
        set_rate_limits(snapshot, rate_limits);
    }
}

#[derive(Clone, Copy)]
enum AuthProbe {
    ClaudeJson,
    ExitCode,
}

#[derive(Clone, Copy)]
struct RuntimeSpec {
    backend: BackendKind,
    label: &'static str,
    command: &'static str,
    auth: Option<AuthProbe>,
    install_action: &'static str,
    auth_action: Option<&'static str>,
}

fn runtime_spec(backend: BackendKind) -> RuntimeSpec {
    match backend {
        BackendKind::Claude => RuntimeSpec {
            backend,
            label: "Claude Code",
            command: "claude",
            auth: Some(AuthProbe::ClaudeJson),
            install_action: "Install Claude Code and ensure `claude` is on Slide's launch PATH.",
            auth_action: Some("Run `claude auth login` to authenticate."),
        },
        BackendKind::Codex => RuntimeSpec {
            backend,
            label: "Codex",
            command: "codex",
            auth: Some(AuthProbe::ExitCode),
            install_action: "Install Codex and ensure `codex` is on Slide's launch PATH.",
            auth_action: Some("Run `codex login` to authenticate."),
        },
        BackendKind::Grok => RuntimeSpec {
            backend,
            label: "Grok",
            command: "grok",
            auth: None,
            install_action: "Install Grok and ensure `grok` is on Slide's launch PATH.",
            auth_action: None,
        },
        BackendKind::Antigravity => RuntimeSpec {
            backend,
            label: "Antigravity",
            command: "agy",
            auth: None,
            install_action: "Install Antigravity and ensure `agy` is on Slide's launch PATH.",
            auth_action: None,
        },
        BackendKind::OpenCode => RuntimeSpec {
            backend,
            label: "OpenCode",
            command: "opencode",
            auth: None,
            install_action: "Install OpenCode and ensure `opencode` is on Slide's launch PATH.",
            auth_action: None,
        },
    }
}

fn probe_snapshot(host: Option<&str>) -> Result<RuntimeDiagnosticsSnapshot> {
    if let Some(host) = host {
        let reachable = run_target_command(Some(host), &["true"], true)
            .map(|output| {
                output.success
                    && !output.timed_out
                    && !output.stdout_truncated
                    && !output.stderr_truncated
            })
            .unwrap_or(false);
        if !reachable {
            return Ok(unreachable_snapshot(host));
        }
    }
    let tmux = probe_tmux(host);
    let login_shell = host.is_some() || tmux.available;
    // Each CLI has an independent five-second bound. Run those probes in
    // parallel so a missing or wedged provider cannot multiply the latency
    // of create-time preflight by the number of supported backends.
    let backends = std::thread::scope(|scope| {
        BackendKind::ALL
            .map(|backend| {
                (
                    backend,
                    scope.spawn(move || probe_backend(backend, host, login_shell)),
                )
            })
            .into_iter()
            .map(|(backend, handle)| {
                handle.join().unwrap_or_else(|_| {
                    broken(
                        runtime_spec(backend),
                        "diagnostic worker stopped unexpectedly.",
                    )
                })
            })
            .collect()
    });
    Ok(RuntimeDiagnosticsSnapshot {
        target: host.unwrap_or("local").to_string(),
        checked_at: now_ms(),
        backends,
        tmux,
    })
}

fn unreachable_snapshot(host: &str) -> RuntimeDiagnosticsSnapshot {
    RuntimeDiagnosticsSnapshot {
        target: host.to_string(),
        checked_at: now_ms(),
        backends: BackendKind::ALL
            .into_iter()
            .map(|backend| RuntimeDiagnostic {
                backend,
                label: runtime_spec(backend).label,
                status: RuntimeStatus::Broken,
                available: false,
                installed: false,
                authenticated: None,
                version: None,
                message: "The target login shell could not be reached.".to_string(),
                action: Some("Verify SSH connectivity and the selected host alias.".to_string()),
                last_error: None,
                rate_limits: Vec::new(),
            })
            .collect(),
        tmux: RuntimeCapability {
            available: false,
            required: true,
            version: None,
            message: "tmux could not be checked because the host is unreachable.".to_string(),
            action: Some("Verify SSH connectivity and try again.".to_string()),
        },
    }
}

fn probe_backend(backend: BackendKind, host: Option<&str>, login_shell: bool) -> RuntimeDiagnostic {
    let spec = runtime_spec(backend);
    probe_backend_with(spec, |args| run_target_command(host, args, login_shell))
}

fn probe_backend_with(
    spec: RuntimeSpec,
    mut run: impl FnMut(&[&str]) -> Result<BoundedOutput>,
) -> RuntimeDiagnostic {
    let version = match run(&[spec.command, "--version"]) {
        Ok(output) => output,
        Err(_) => return broken(spec, "The version probe could not be started."),
    };
    if version.code == Some(127) {
        return missing(spec);
    }
    // Timeouts and transient version-check failures must not mark the
    // runtime unavailable: preflight gates create/resume on `available`,
    // and a flaky probe would otherwise strand the user for CACHE_TTL.
    if version.timed_out {
        return ready(
            spec,
            None,
            None,
            "Runtime version check timed out; launch may still work.",
        );
    }
    if version.stdout_truncated || version.stderr_truncated || !version.success {
        return ready(
            spec,
            None,
            None,
            "Runtime version check failed; launch may still work.",
        );
    }
    let version = sanitized_version(&version);
    let Some(auth_probe) = spec.auth else {
        return ready(spec, version, None, "Runtime is installed.");
    };
    let auth_args: &[&str] = match auth_probe {
        AuthProbe::ClaudeJson => &[spec.command, "auth", "status", "--json"],
        AuthProbe::ExitCode => &[spec.command, "login", "status"],
    };
    let auth = match run(auth_args) {
        Ok(output) => output,
        Err(_) => {
            return ready(
                spec,
                version,
                None,
                "Runtime is installed; authentication could not be verified.",
            )
        }
    };
    if auth.timed_out || auth.stdout_truncated || auth.stderr_truncated {
        return ready(
            spec,
            version,
            None,
            "Runtime is installed; authentication could not be verified.",
        );
    }
    if !auth.success && auth_probe_unsupported(&auth) {
        return ready(
            spec,
            version,
            None,
            "Runtime is installed; this version does not expose authentication status.",
        );
    }
    let authenticated = match auth_probe {
        AuthProbe::ClaudeJson if auth.success => parse_claude_logged_in(&auth.stdout),
        AuthProbe::ClaudeJson | AuthProbe::ExitCode => Some(auth.success),
    };
    match authenticated {
        Some(true) => ready(
            spec,
            version,
            Some(true),
            "Runtime is installed and authenticated.",
        ),
        Some(false) => unauthenticated(spec, version),
        None => ready(
            spec,
            version,
            None,
            "Runtime is installed; authentication could not be verified.",
        ),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexRateLimitsResult {
    rate_limits: Option<CodexRateLimit>,
    #[serde(default)]
    rate_limits_by_limit_id: BTreeMap<String, CodexRateLimit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexRateLimit {
    limit_id: Option<String>,
    limit_name: Option<String>,
    primary: Option<CodexRateLimitWindow>,
    secondary: Option<CodexRateLimitWindow>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexRateLimitWindow {
    used_percent: Option<i32>,
    window_duration_mins: Option<i64>,
    resets_at: Option<i64>,
}

fn query_codex_rate_limits(host: Option<&str>) -> Result<Vec<RuntimeRateLimit>> {
    let client = CodexClient::connect(host, false)?;
    query_codex_rate_limits_with_client(client)
}

#[cfg(test)]
fn query_codex_rate_limits_with_command(command: Command) -> Result<Vec<RuntimeRateLimit>> {
    query_codex_rate_limits_with_client(CodexClient::with_command(command, false)?)
}

fn query_codex_rate_limits_with_client(mut client: CodexClient) -> Result<Vec<RuntimeRateLimit>> {
    let result = client.request("account/rateLimits/read", serde_json::json!({}))?;
    Ok(normalize_codex_rate_limits(result))
}

fn normalize_codex_rate_limits(result: CodexRateLimitsResult) -> Vec<RuntimeRateLimit> {
    let mut rows = Vec::new();
    if result.rate_limits_by_limit_id.is_empty() {
        if let Some(limit) = result.rate_limits {
            append_rate_limit_rows(&mut rows, "codex", limit);
        }
    } else {
        for (map_id, limit) in result.rate_limits_by_limit_id {
            append_rate_limit_rows(&mut rows, &map_id, limit);
            if rows.len() >= MAX_RATE_LIMITS {
                break;
            }
        }
    }
    rows.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then(left.window_minutes.cmp(&right.window_minutes))
            .then(left.resets_at.cmp(&right.resets_at))
            .then(left.used_percent.cmp(&right.used_percent))
    });
    rows.dedup();
    rows.truncate(MAX_RATE_LIMITS);
    rows
}

fn append_rate_limit_rows(rows: &mut Vec<RuntimeRateLimit>, map_id: &str, limit: CodexRateLimit) {
    let label = sanitize_rate_limit_name(limit.limit_name)
        .unwrap_or_else(|| humanize_rate_limit_id(limit.limit_id.as_deref().unwrap_or(map_id)));
    for window in [limit.primary, limit.secondary].into_iter().flatten() {
        if rows.len() >= MAX_RATE_LIMITS {
            return;
        }
        let Some(used_percent) = window.used_percent else {
            continue;
        };
        rows.push(RuntimeRateLimit {
            label: label.clone(),
            used_percent: used_percent.clamp(0, 100) as u8,
            window_minutes: window
                .window_duration_mins
                .and_then(|minutes| u64::try_from(minutes).ok()),
            resets_at: window
                .resets_at
                .filter(|timestamp| *timestamp >= 0)
                .and_then(|timestamp| timestamp.checked_mul(1000)),
        });
    }
}

fn sanitize_rate_limit_name(name: Option<String>) -> Option<String> {
    let name = name?;
    let value = name
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_RATE_LIMIT_NAME_CHARS)
        .collect::<String>();
    (!value.is_empty()).then_some(value)
}

fn humanize_rate_limit_id(id: &str) -> String {
    let mut label = id
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
    label.truncate(MAX_RATE_LIMIT_NAME_CHARS);
    if label.is_empty() {
        "Codex".to_string()
    } else {
        label
    }
}

fn set_rate_limits(snapshot: &mut RuntimeDiagnosticsSnapshot, rate_limits: Vec<RuntimeRateLimit>) {
    if let Some(codex) = snapshot
        .backends
        .iter_mut()
        .find(|diagnostic| diagnostic.backend == BackendKind::Codex)
    {
        codex.rate_limits = rate_limits;
    }
}

fn probe_tmux(host: Option<&str>) -> RuntimeCapability {
    let required = host.is_some();
    let output = match host {
        Some(_) => run_target_command(host, &["tmux", "-V"], true),
        None => {
            let mut command = Command::new("tmux");
            command.arg("-V");
            crate::process::run_bounded(
                command,
                PROBE_OUTPUT_LIMIT,
                PROBE_OUTPUT_LIMIT,
                PROBE_TIMEOUT,
            )
        }
    };
    match output {
        Ok(output)
            if output.success
                && !output.timed_out
                && !output.stdout_truncated
                && !output.stderr_truncated =>
        {
            RuntimeCapability {
                available: true,
                required,
                version: sanitized_version(&output),
                message: if required {
                    "tmux is ready for persistent remote sessions."
                } else {
                    "tmux is ready; local sessions can survive daemon restarts."
                }
                .to_string(),
                action: None,
            }
        }
        _ => RuntimeCapability {
            available: false,
            required,
            version: None,
            message: if required {
                "tmux is required for remote Slide sessions but is unavailable."
            } else {
                "tmux is unavailable; local sessions will use a direct PTY."
            }
            .to_string(),
            action: Some(
                if required {
                    "Install tmux on the remote host."
                } else {
                    "Install tmux to keep local sessions alive across daemon restarts."
                }
                .to_string(),
            ),
        },
    }
}

fn run_target_command(
    host: Option<&str>,
    argv: &[&str],
    login_shell: bool,
) -> Result<BoundedOutput> {
    let command = target_command(host, argv, login_shell)?;
    crate::process::run_bounded(
        command,
        PROBE_OUTPUT_LIMIT,
        PROBE_OUTPUT_LIMIT,
        PROBE_TIMEOUT,
    )
}

fn target_command(host: Option<&str>, argv: &[&str], login_shell: bool) -> Result<Command> {
    let command_line = argv
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    match host {
        None => {
            let shell = if login_shell {
                std::env::var_os("SHELL")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/bin/sh"))
            } else {
                PathBuf::from("/bin/sh")
            };
            let mut command = Command::new(shell);
            command.arg(if login_shell { "-lc" } else { "-c" });
            command.arg(command_line);
            Ok(command)
        }
        Some(host) => {
            crate::ssh::validate_host(host)?;
            let remote = format!(
                "exec \"${{SHELL:-/bin/sh}}\" -lc {}",
                shell_quote(&command_line)
            );
            let mut command = Command::new("ssh");
            command
                .args(["-o", "BatchMode=yes"])
                .args(crate::ssh::ssh_args())
                .arg(host)
                .arg(remote);
            Ok(command)
        }
    }
}

fn ready(
    spec: RuntimeSpec,
    version: Option<String>,
    authenticated: Option<bool>,
    message: &str,
) -> RuntimeDiagnostic {
    RuntimeDiagnostic {
        backend: spec.backend,
        label: spec.label,
        status: RuntimeStatus::Ready,
        available: true,
        installed: true,
        authenticated,
        version,
        message: message.to_string(),
        action: None,
        last_error: None,
        rate_limits: Vec::new(),
    }
}

fn missing(spec: RuntimeSpec) -> RuntimeDiagnostic {
    RuntimeDiagnostic {
        backend: spec.backend,
        label: spec.label,
        status: RuntimeStatus::Missing,
        available: false,
        installed: false,
        authenticated: None,
        version: None,
        message: format!(
            "{} is not available in Slide's launch environment.",
            spec.label
        ),
        action: Some(spec.install_action.to_string()),
        last_error: None,
        rate_limits: Vec::new(),
    }
}

fn unauthenticated(spec: RuntimeSpec, version: Option<String>) -> RuntimeDiagnostic {
    RuntimeDiagnostic {
        backend: spec.backend,
        label: spec.label,
        status: RuntimeStatus::Unauthenticated,
        available: false,
        installed: true,
        authenticated: Some(false),
        version,
        message: format!("{} is installed but not authenticated.", spec.label),
        action: spec.auth_action.map(str::to_string),
        last_error: None,
        rate_limits: Vec::new(),
    }
}

fn broken(spec: RuntimeSpec, message: &str) -> RuntimeDiagnostic {
    RuntimeDiagnostic {
        backend: spec.backend,
        label: spec.label,
        status: RuntimeStatus::Broken,
        available: false,
        installed: true,
        authenticated: None,
        version: None,
        message: format!("{} {message}", spec.label),
        action: Some(format!(
            "Run `{} --version` in the target login shell and fix the reported error.",
            spec.command
        )),
        last_error: None,
        rate_limits: Vec::new(),
    }
}

fn validate_preflight(snapshot: &RuntimeDiagnosticsSnapshot, backend: BackendKind) -> Result<()> {
    let diagnostic = snapshot
        .backends
        .iter()
        .find(|diagnostic| diagnostic.backend == backend)
        .context("runtime diagnostic missing backend")?;
    // Only hard-block on definitive, user-actionable states. `Broken` is a
    // probe uncertainty (timeout, weird exit) — let spawn surface the real
    // error rather than caching a false negative across create/resume.
    match diagnostic.status {
        RuntimeStatus::Missing | RuntimeStatus::Unauthenticated => {
            bail!(
                "{} {}",
                diagnostic.message,
                diagnostic.action.as_deref().unwrap_or("")
            );
        }
        RuntimeStatus::Ready | RuntimeStatus::Broken => {}
    }
    if snapshot.tmux.required && !snapshot.tmux.available {
        bail!(
            "{} {}",
            snapshot.tmux.message,
            snapshot.tmux.action.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn sanitized_version(output: &BoundedOutput) -> Option<String> {
    let bytes = if output.stdout.iter().any(|byte| !byte.is_ascii_whitespace()) {
        &output.stdout
    } else {
        &output.stderr
    };
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            line.chars()
                .filter(|character| !character.is_control())
                .take(120)
                .collect()
        })
        .filter(|line: &String| !line.is_empty())
}

fn parse_claude_logged_in(bytes: &[u8]) -> Option<bool> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|value| {
            value
                .get("loggedIn")
                .and_then(|logged_in| logged_in.as_bool())
        })
}

fn auth_probe_unsupported(output: &BoundedOutput) -> bool {
    let mut text = String::from_utf8_lossy(&output.stdout).to_lowercase();
    text.push_str(&String::from_utf8_lossy(&output.stderr).to_lowercase());
    [
        "unknown command",
        "unrecognized command",
        "unrecognized subcommand",
        "unexpected argument",
        "no such command",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn target_key(host: Option<&str>) -> String {
    host.map(|host| format!("ssh:{host}"))
        .unwrap_or_else(|| "local".to_string())
}

fn error_key(target: &str, backend: BackendKind) -> String {
    format!("{target}:{}", backend.as_str())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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

    fn output(success: bool, code: i32, stdout: &str) -> BoundedOutput {
        output_with_stderr(success, code, stdout, "")
    }

    fn output_with_stderr(success: bool, code: i32, stdout: &str, stderr: &str) -> BoundedOutput {
        BoundedOutput {
            success,
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        }
    }

    fn empty_snapshot(target: String) -> RuntimeDiagnosticsSnapshot {
        RuntimeDiagnosticsSnapshot {
            target,
            checked_at: 0,
            backends: Vec::new(),
            tmux: RuntimeCapability {
                available: true,
                required: false,
                version: None,
                message: String::new(),
                action: None,
            },
        }
    }

    fn ready_codex_snapshot() -> RuntimeDiagnosticsSnapshot {
        RuntimeDiagnosticsSnapshot {
            target: "local".to_string(),
            checked_at: 0,
            backends: vec![ready(
                runtime_spec(BackendKind::Codex),
                Some("codex-cli 1.0".to_string()),
                Some(true),
                "ready",
            )],
            tmux: RuntimeCapability {
                available: true,
                required: false,
                version: None,
                message: String::new(),
                action: None,
            },
        }
    }

    #[test]
    fn diagnostics_cache_has_a_hard_target_limit() {
        let cache = RuntimeDiagnosticsCache::default();
        for index in 0..=MAX_CACHE_TARGETS {
            let target = format!("ssh:host-{index}");
            cache.cache_snapshot(target.clone(), empty_snapshot(target));
        }
        assert_eq!(
            cache
                .snapshots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            MAX_CACHE_TARGETS
        );
    }

    #[test]
    fn preflight_never_enriches_account_rate_limits() {
        let cache = RuntimeDiagnosticsCache::default();
        cache.cache_snapshot("local".to_string(), ready_codex_snapshot());

        cache.preflight(BackendKind::Codex, None).unwrap();

        assert!(cache.cached_rate_limits("local").is_none());
    }

    #[test]
    fn failed_rate_limit_probe_is_non_blocking_and_cached() {
        let cache = RuntimeDiagnosticsCache::default();
        cache.cache_snapshot("local".to_string(), ready_codex_snapshot());
        let mut snapshot = cache.get(None, false).unwrap();
        let mut probes = 0;
        cache.enrich_rate_limits_with(&mut snapshot, "local", false, || {
            probes += 1;
            bail!("private provider error")
        });
        assert_eq!(probes, 1);
        assert!(snapshot.backends[0].rate_limits.is_empty());

        let mut cached = cache.get(None, false).unwrap();
        cache.enrich_rate_limits_with(&mut cached, "local", false, || {
            panic!("cached failure must not start another app-server")
        });
        assert!(cached.backends[0].rate_limits.is_empty());
    }

    #[test]
    fn uncertain_codex_probe_does_not_start_app_server() {
        let mut snapshot = ready_codex_snapshot();
        snapshot.backends[0].version = None;
        snapshot.backends[0].authenticated = None;
        RuntimeDiagnosticsCache::default().enrich_rate_limits_with(
            &mut snapshot,
            "local",
            false,
            || panic!("uncertain installation must not start app-server"),
        );
        assert!(snapshot.backends[0].rate_limits.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn codex_rate_limit_handshake_whitelists_and_normalizes_fields() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"
IFS= read -r initialize
case "$initialize" in *experimentalApi*) exit 5 ;; esac
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized
IFS= read -r limits
case "$limits" in
  *'account/rateLimits/read'*)
    printf '%s\n' '{"id":2,"result":{"rateLimits":null,"rateLimitsByLimitId":{"weekly":{"limitName":" Weekly\u0001 ","primary":{"usedPercent":125,"windowDurationMins":10080,"resetsAt":1700000000},"secondary":{"usedPercent":50,"windowDurationMins":300,"resetsAt":-1},"privateToken":"hidden"},"codex_spark":{"limitName":null,"primary":{"usedPercent":-4,"windowDurationMins":null,"resetsAt":null}}},"accountEmail":"private@example.com"}}'
    ;;
  *) exit 6 ;;
esac
"#,
        ]);

        let limits = query_codex_rate_limits_with_command(command).unwrap();
        assert_eq!(limits.len(), 3);
        assert_eq!(
            limits[0],
            RuntimeRateLimit {
                label: "Codex Spark".to_string(),
                used_percent: 0,
                window_minutes: None,
                resets_at: None,
            }
        );
        assert_eq!(limits[1].label, "Weekly");
        assert_eq!(limits[1].used_percent, 50);
        assert_eq!(limits[1].window_minutes, Some(300));
        assert_eq!(limits[1].resets_at, None);
        assert_eq!(limits[2].used_percent, 100);
        assert_eq!(limits[2].window_minutes, Some(10_080));
        assert_eq!(limits[2].resets_at, Some(1_700_000_000_000));

        let serialized = serde_json::to_string(&limits).unwrap();
        assert!(!serialized.contains("private@example.com"));
        assert!(!serialized.contains("hidden"));
        assert!(!serialized.contains("codex_spark"));
    }

    #[test]
    fn rate_limit_map_is_preferred_and_legacy_shape_is_a_fallback() {
        let mapped = serde_json::from_value(serde_json::json!({
            "rateLimits": {
                "limitName": "Duplicate legacy row",
                "primary": { "usedPercent": 99 },
                "secondary": null
            },
            "rateLimitsByLimitId": {
                "codex_hourly": {
                    "limitName": null,
                    "primary": { "usedPercent": 10 },
                    "secondary": null
                }
            }
        }))
        .unwrap();
        let limits = normalize_codex_rate_limits(mapped);
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].label, "Codex Hourly");

        let legacy = serde_json::from_value(serde_json::json!({
            "rateLimits": {
                "limitId": "codex_weekly",
                "limitName": null,
                "primary": {
                    "usedPercent": 12,
                    "windowDurationMins": 10080,
                    "resetsAt": 1700000000
                },
                "secondary": null
            },
            "rateLimitsByLimitId": {}
        }))
        .unwrap();
        let limits = normalize_codex_rate_limits(legacy);
        assert_eq!(limits[0].label, "Codex Weekly");
    }

    #[test]
    fn missing_runtime_is_explicit_and_actionable() {
        let diagnostic = probe_backend_with(runtime_spec(BackendKind::Codex), |_| {
            Ok(output(false, 127, ""))
        });
        assert_eq!(diagnostic.status, RuntimeStatus::Missing);
        assert!(!diagnostic.available);
        assert!(diagnostic.action.unwrap().contains("Install Codex"));
    }

    #[test]
    fn claude_auth_probe_does_not_expose_identity() {
        let mut calls = 0;
        let diagnostic = probe_backend_with(runtime_spec(BackendKind::Claude), |_| {
            calls += 1;
            Ok(if calls == 1 {
                output(true, 0, "2.3.4 (Claude Code)\n")
            } else {
                output(
                    true,
                    0,
                    "profile banner\n{\"loggedIn\":true,\"email\":\"private@example.com\",\"organizationName\":\"Secret\"}\n",
                )
            })
        });
        assert_eq!(diagnostic.status, RuntimeStatus::Ready);
        assert_eq!(diagnostic.version.as_deref(), Some("2.3.4 (Claude Code)"));
        let serialized = serde_json::to_string(&diagnostic).unwrap();
        assert!(!serialized.contains("private@example.com"));
        assert!(!serialized.contains("Secret"));
    }

    #[test]
    fn codex_auth_failure_is_actionable() {
        let mut calls = 0;
        let diagnostic = probe_backend_with(runtime_spec(BackendKind::Codex), |_| {
            calls += 1;
            Ok(if calls == 1 {
                output(true, 0, "codex-cli 1.2.3\n")
            } else {
                output(false, 1, "")
            })
        });
        assert_eq!(diagnostic.status, RuntimeStatus::Unauthenticated);
        assert!(diagnostic.action.unwrap().contains("codex login"));
    }

    #[test]
    fn unsupported_auth_probe_does_not_block_an_installed_runtime() {
        let mut calls = 0;
        let diagnostic = probe_backend_with(runtime_spec(BackendKind::Codex), |_| {
            calls += 1;
            Ok(if calls == 1 {
                output(true, 0, "codex-cli 0.8.0\n")
            } else {
                output_with_stderr(false, 2, "", "error: unrecognized subcommand 'status'\n")
            })
        });
        assert_eq!(diagnostic.status, RuntimeStatus::Ready);
        assert!(diagnostic.available);
        assert_eq!(diagnostic.authenticated, None);
        assert_eq!(diagnostic.action, None);
    }

    #[test]
    fn auth_parse_failure_keeps_an_installed_runtime_available() {
        let mut calls = 0;
        let diagnostic = probe_backend_with(runtime_spec(BackendKind::Claude), |_| {
            calls += 1;
            Ok(if calls == 1 {
                output(true, 0, "claude 2.0\n")
            } else {
                output(true, 0, "unexpected")
            })
        });
        assert_eq!(diagnostic.status, RuntimeStatus::Ready);
        assert!(diagnostic.available);
        assert_eq!(diagnostic.authenticated, None);
    }

    #[test]
    fn remote_preflight_requires_tmux() {
        let diagnostic = ready(
            runtime_spec(BackendKind::Grok),
            Some("1.0".to_string()),
            None,
            "ready",
        );
        let snapshot = RuntimeDiagnosticsSnapshot {
            target: "host".to_string(),
            checked_at: 0,
            backends: vec![diagnostic],
            tmux: RuntimeCapability {
                available: false,
                required: true,
                version: None,
                message: "tmux missing".to_string(),
                action: Some("install tmux".to_string()),
            },
        };
        let error = validate_preflight(&snapshot, BackendKind::Grok).unwrap_err();
        assert!(error.to_string().contains("install tmux"));
    }

    #[test]
    fn version_probe_timeout_keeps_runtime_available() {
        let diagnostic = probe_backend_with(runtime_spec(BackendKind::Claude), |_| {
            Ok(BoundedOutput {
                success: false,
                code: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                timed_out: true,
            })
        });
        assert_eq!(diagnostic.status, RuntimeStatus::Ready);
        assert!(diagnostic.available);
        assert!(diagnostic.message.contains("timed out"));
    }

    #[test]
    fn preflight_allows_broken_probe_but_blocks_missing() {
        let broken = broken(runtime_spec(BackendKind::Claude), "weird failure.");
        let missing = missing(runtime_spec(BackendKind::Codex));
        let snapshot = RuntimeDiagnosticsSnapshot {
            target: "local".to_string(),
            checked_at: 0,
            backends: vec![broken, missing],
            tmux: RuntimeCapability {
                available: true,
                required: false,
                version: Some("tmux 3.4".to_string()),
                message: "ready".to_string(),
                action: None,
            },
        };
        validate_preflight(&snapshot, BackendKind::Claude).expect("broken must not block");
        let error = validate_preflight(&snapshot, BackendKind::Codex).unwrap_err();
        assert!(error.to_string().contains("not available"));
    }

    #[test]
    fn preflight_blocks_unauthenticated() {
        let diagnostic = unauthenticated(runtime_spec(BackendKind::Codex), Some("1.0".into()));
        let snapshot = RuntimeDiagnosticsSnapshot {
            target: "local".to_string(),
            checked_at: 0,
            backends: vec![diagnostic],
            tmux: RuntimeCapability {
                available: true,
                required: false,
                version: None,
                message: "ready".to_string(),
                action: None,
            },
        };
        let error = validate_preflight(&snapshot, BackendKind::Codex).unwrap_err();
        assert!(error.to_string().contains("not authenticated"));
    }

    #[test]
    fn launch_errors_are_generic_and_clear_after_success() {
        let cache = RuntimeDiagnosticsCache::default();
        cache.record_launch_failure(BackendKind::Codex, None);
        let errors = cache.last_errors.lock().unwrap();
        let value = errors.get("local:codex").unwrap();
        assert!(!value.contains('/'));
        drop(errors);
        cache.clear_launch_failure(BackendKind::Codex, None);
        assert!(cache.last_errors.lock().unwrap().is_empty());
    }

    #[test]
    fn shell_quote_handles_remote_arguments() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn local_direct_probe_uses_the_daemon_environment() {
        let command = target_command(None, &["codex", "--version"], false).unwrap();
        assert_eq!(command.get_program(), "/bin/sh");
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["-c", "'codex' '--version'"]
        );
    }
}
