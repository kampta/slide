use crate::classifier::Signals;
use crate::session::ExecutionPolicy;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::SystemTime;

mod agy;
mod claude;
pub(crate) mod claude_usage;
mod codex;
pub(crate) mod codex_app_server;
mod codex_subagents;
mod grok;
pub(crate) mod grok_usage;
mod opencode;

pub use agy::AgyBackend;
pub use claude::ClaudeBackend;
pub use codex::CodexBackend;
pub use grok::GrokBackend;
pub use opencode::OpenCodeBackend;

/// Per-turn context snapshot for a backend, read from the transcript the
/// backend writes to disk. `used_tokens` is what the model ingested on the
/// last assistant turn (input + cache reads + cache creations); dividing by
/// `window` gives the "% context used" chip in the UI.
#[derive(Debug, Clone, Serialize)]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub window: u64,
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
}

/// Provider-reported account usage, reduced to the fields Slide can display
/// without exposing provider-specific account or billing details.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProviderRateLimit {
    pub label: String,
    pub used_percent: u8,
    pub window_minutes: Option<u64>,
    pub resets_at: Option<i64>,
}

/// Accept the timestamp forms used by provider usage responses: Unix seconds,
/// Unix milliseconds, or RFC 3339 strings.
pub(crate) fn parse_timestamp_ms(value: &serde_json::Value) -> Option<i64> {
    if let Some(seconds) = value.as_i64() {
        return if seconds.unsigned_abs() >= 1_000_000_000_000 {
            Some(seconds)
        } else {
            seconds.checked_mul(1_000)
        };
    }
    if let Some(seconds) = value.as_f64() {
        if !seconds.is_finite() {
            return None;
        }
        let multiplier = if seconds.abs() >= 1_000_000_000_000.0 {
            1.0
        } else {
            1_000.0
        };
        let millis = (seconds * multiplier).round();
        if millis < i64::MIN as f64 || millis > i64::MAX as f64 {
            return None;
        }
        return Some(millis as i64);
    }
    value.as_str().and_then(parse_rfc3339_ms)
}

fn parse_rfc3339_ms(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let number = |start: usize, length: usize| -> Option<i64> {
        let end = start.checked_add(length)?;
        let slice = bytes.get(start..end)?;
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse().ok()
    };
    let year = number(0, 4)?;
    let month = number(5, 2)? as u32;
    let day = number(8, 2)? as u32;
    let hour = number(11, 2)?;
    let minute = number(14, 2)?;
    let second = number(17, 2)?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut index = 19;
    let mut millis = 0i64;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let fraction = bytes.get(start..index)?;
        let digits = fraction
            .iter()
            .take(3)
            .fold(0i64, |value, digit| value * 10 + i64::from(digit - b'0'));
        millis = match fraction.len() {
            0 => return None,
            1 => digits * 100,
            2 => digits * 10,
            _ => digits,
        };
    }

    let offset_minutes = match bytes.get(index)? {
        b'Z' | b'z' if index + 1 == bytes.len() => 0,
        b'+' | b'-' if index + 6 == bytes.len() && bytes.get(index + 3) == Some(&b':') => {
            let sign = if bytes[index] == b'+' { 1 } else { -1 };
            let hours = number(index + 1, 2)?;
            let minutes = number(index + 4, 2)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 60 + minutes)
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day)?;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?
        .checked_sub(offset_minutes * 60)?;
    seconds.checked_mul(1_000)?.checked_add(millis)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 400 == 0 || (year % 4 == 0 && year % 100 != 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's proleptic Gregorian calendar conversion, relative to the
// Unix epoch. The provider timestamps are validated before this is called.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let year = year.checked_sub(i64::from(month <= 2))?;
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era.checked_mul(146_097)?
        .checked_add(day_of_era)?
        .checked_sub(719_468)
}

/// A privacy-bounded view of a backend child agent. Provider prompts, tool
/// arguments, command output, and transcript paths deliberately never cross
/// this boundary; the dock only needs identity, hierarchy, and lifecycle.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SubagentSnapshot {
    pub id: String,
    pub parent_id: String,
    pub name: Option<String>,
    pub role: Option<String>,
    pub state: SubagentState,
    /// Unix timestamp in seconds, matching the provider's thread metadata.
    pub created_at: i64,
    /// Unix timestamp in seconds, matching the provider's thread metadata.
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Starting,
    Running,
    Waiting,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SubagentList {
    pub supported: bool,
    pub agents: Vec<SubagentSnapshot>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Claude,
    Codex,
    Grok,
    #[serde(rename = "agy", alias = "antigravity")]
    Antigravity,
    OpenCode,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct BackendInfo {
    pub id: BackendKind,
    pub label: &'static str,
    pub context_usage: bool,
    pub subagents: bool,
    pub fork: bool,
    pub execution_policies: &'static [ExecutionPolicy],
}

const UNRESTRICTED_ONLY: &[ExecutionPolicy] = &[ExecutionPolicy::Unrestricted];
const CODEX_POLICIES: &[ExecutionPolicy] = &[
    ExecutionPolicy::Unrestricted,
    ExecutionPolicy::SandboxedAuto,
];

impl BackendKind {
    pub const ALL: [Self; 5] = [
        Self::Claude,
        Self::Codex,
        Self::Grok,
        Self::Antigravity,
        Self::OpenCode,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Claude => "claude",
            BackendKind::Codex => "codex",
            BackendKind::Grok => "grok",
            BackendKind::Antigravity => "agy",
            BackendKind::OpenCode => "opencode",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(BackendKind::Claude),
            "codex" => Some(BackendKind::Codex),
            "grok" => Some(BackendKind::Grok),
            "agy" | "antigravity" => Some(BackendKind::Antigravity),
            "opencode" => Some(BackendKind::OpenCode),
            _ => None,
        }
    }

    pub fn info(self) -> BackendInfo {
        match self {
            Self::Claude => BackendInfo {
                id: self,
                label: "Claude",
                context_usage: true,
                subagents: false,
                fork: true,
                execution_policies: UNRESTRICTED_ONLY,
            },
            Self::Codex => BackendInfo {
                id: self,
                label: "Codex",
                context_usage: false,
                subagents: true,
                fork: true,
                execution_policies: CODEX_POLICIES,
            },
            Self::Grok => BackendInfo {
                id: self,
                label: "Grok",
                context_usage: false,
                subagents: false,
                fork: false,
                execution_policies: UNRESTRICTED_ONLY,
            },
            Self::Antigravity => BackendInfo {
                id: self,
                label: "Antigravity",
                context_usage: false,
                subagents: false,
                fork: false,
                execution_policies: UNRESTRICTED_ONLY,
            },
            Self::OpenCode => BackendInfo {
                id: self,
                label: "OpenCode",
                context_usage: false,
                subagents: false,
                fork: false,
                execution_policies: UNRESTRICTED_ONLY,
            },
        }
    }
}

pub fn available() -> Vec<BackendInfo> {
    BackendKind::ALL
        .into_iter()
        .map(BackendKind::info)
        .collect()
}

pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn argv(&self, cwd: &Path) -> Vec<String>;

    /// Apply a session's persisted execution policy to a complete launch
    /// command. Backends opt in explicitly so Slide never labels an
    /// unrestricted process as sandboxed.
    fn apply_execution_policy(
        &self,
        policy: ExecutionPolicy,
        argv: Vec<String>,
    ) -> Result<Vec<String>> {
        if matches!(policy, ExecutionPolicy::Unrestricted) {
            Ok(argv)
        } else {
            bail!(
                "{} does not support the {} execution policy",
                self.kind().as_str(),
                policy.as_str()
            )
        }
    }

    /// Environment overrides applied only to the backend process. This is
    /// preferable to editing a user's global CLI configuration when a backend
    /// exposes per-process configuration through environment variables.
    fn env(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// The patterns this backend exposes for session-state classification.
    /// See [`crate::classifier`] for how they combine into session state.
    /// One `Signals` per backend, built lazily into a `OnceLock`, so calls
    /// are cheap and the regex compile cost is paid once.
    fn signals(&self) -> &'static Signals;

    /// argv that re-enters a previously-started backend conversation. Used
    /// when the supervisor is gone but the backend has its own durable
    /// transcript on disk (e.g. `claude --resume <id>`). `None` means the
    /// backend has no resume story and a fresh session must be started.
    fn resume_argv(&self, _cwd: &Path, _session_id: &str) -> Option<Vec<String>> {
        None
    }

    /// Resume the newest conversation scoped to `cwd` when Slide has no
    /// provider-native id. Used only for an existing session being relaunched,
    /// never for a newly-created session.
    fn resume_latest_argv(&self, _cwd: &Path) -> Option<Vec<String>> {
        None
    }

    /// Start a provider-native branch of an existing conversation. The
    /// returned process must create a new provider session id rather than
    /// attaching both Slide sessions to the same transcript.
    fn fork_argv(
        &self,
        _cwd: &Path,
        _session_id: &str,
        _prompt: Option<&str>,
    ) -> Option<Vec<String>> {
        None
    }

    /// Scan the backend's transcript directory on the host where it runs
    /// for the newest session file whose mtime is after `since`. Returns
    /// the file's session id (its stem), or `None` if no matching file
    /// exists. Only meaningful on the host that owns the transcripts —
    /// callers running remotely must run this over SSH.
    fn discover_session_id(&self, _cwd: &Path, _since: SystemTime) -> Option<String> {
        None
    }

    /// Whether `discover_session_id` has a real implementation. Callers use
    /// this to avoid polling the default no-op for backends whose transcript
    /// location is unknown.
    fn supports_session_discovery(&self) -> bool {
        false
    }

    /// Read the latest turn's context usage from the backend's transcript.
    /// Returns `None` when the backend has no transcript, the session id
    /// hasn't been discovered yet, or no assistant turn has been recorded.
    fn read_context_usage(&self, _cwd: &Path, _session_id: &str) -> Option<ContextUsage> {
        None
    }

    /// Return a bounded, sanitized snapshot of descendants spawned by this
    /// backend session. `ssh_host` is present when the provider and its
    /// metadata live on a remote Slide host. Backends without a structured
    /// child-agent API return `Ok(None)` so the frontend can hide the dock.
    fn read_subagents(
        &self,
        _cwd: &Path,
        _session_id: &str,
        _ssh_host: Option<&str>,
    ) -> Result<Option<Vec<SubagentSnapshot>>> {
        Ok(None)
    }
}

pub fn for_kind(kind: BackendKind) -> Box<dyn Backend> {
    match kind {
        BackendKind::Claude => Box::new(ClaudeBackend::new()),
        BackendKind::Codex => Box::new(CodexBackend::new()),
        BackendKind::Grok => Box::new(GrokBackend::new()),
        BackendKind::Antigravity => Box::new(AgyBackend::new()),
        BackendKind::OpenCode => Box::new(OpenCodeBackend::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn backend_kind_roundtrip() {
        let cases = [
            (BackendKind::Claude, "claude"),
            (BackendKind::Codex, "codex"),
            (BackendKind::Grok, "grok"),
            (BackendKind::Antigravity, "agy"),
            (BackendKind::OpenCode, "opencode"),
        ];
        for (kind, s) in cases {
            assert_eq!(kind.as_str(), s);
            assert_eq!(BackendKind::from_str(s), Some(kind));
        }
    }

    #[test]
    fn backend_kind_unknown_returns_none() {
        assert_eq!(BackendKind::from_str("gpt"), None);
        assert_eq!(BackendKind::from_str(""), None);
        assert_eq!(
            BackendKind::from_str("antigravity"),
            Some(BackendKind::Antigravity)
        );
    }

    #[test]
    fn provider_timestamps_normalize_seconds_milliseconds_and_rfc3339() {
        assert_eq!(
            parse_timestamp_ms(&serde_json::json!(1_700_000_000)),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_timestamp_ms(&serde_json::json!(1_700_000_000_123_i64)),
            Some(1_700_000_000_123)
        );
        assert_eq!(
            parse_timestamp_ms(&serde_json::json!("2026-08-14T12:30:00-07:00")),
            Some(1_786_735_800_000)
        );
    }

    #[test]
    fn provider_timestamps_reject_malformed_values() {
        assert_eq!(parse_timestamp_ms(&serde_json::json!(null)), None);
        assert_eq!(
            parse_timestamp_ms(&serde_json::json!("2026-99-14T12:30:00Z")),
            None
        );
        assert_eq!(
            parse_timestamp_ms(&serde_json::json!("2026-08-14T12:30:00+99:00")),
            None
        );
    }

    #[test]
    fn antigravity_serializes_as_its_cli_id_and_accepts_product_name() {
        assert_eq!(
            serde_json::to_string(&BackendKind::Antigravity).unwrap(),
            "\"agy\""
        );
        assert_eq!(
            serde_json::from_str::<BackendKind>("\"antigravity\"").unwrap(),
            BackendKind::Antigravity
        );
    }

    #[test]
    fn every_backend_argv_starts_with_its_command() {
        let cases = [
            (BackendKind::Claude, "claude"),
            (BackendKind::Codex, "codex"),
            (BackendKind::Grok, "grok"),
            (BackendKind::Antigravity, "agy"),
            (BackendKind::OpenCode, "opencode"),
        ];
        for (kind, command) in cases {
            let backend = for_kind(kind);
            let argv = backend.argv(Path::new("/some/path"));
            assert_eq!(argv.first().map(String::as_str), Some(command));
            assert_eq!(backend.kind(), kind);
        }
    }

    #[test]
    fn every_backend_launches_with_unrestricted_permissions() {
        for kind in BackendKind::ALL {
            let backend = for_kind(kind);
            let argv = backend.argv(Path::new("/some/path"));
            let unrestricted = match kind {
                BackendKind::Claude | BackendKind::Antigravity => argv
                    .iter()
                    .any(|arg| arg == "--dangerously-skip-permissions"),
                BackendKind::Codex => argv
                    .iter()
                    .any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"),
                BackendKind::Grok => argv.iter().any(|arg| arg == "--always-approve"),
                BackendKind::OpenCode => backend
                    .env()
                    .iter()
                    .any(|(key, value)| key == "OPENCODE_PERMISSION" && value == r#""allow""#),
            };
            assert!(unrestricted, "{kind:?} does not launch unrestricted");
        }
    }

    /// Smoke test: every backend ships a non-empty `Signals` bundle with a
    /// sensible settle window. Per-pattern assertions live in each backend's
    /// own module.
    #[test]
    fn every_backend_exposes_signals() {
        for kind in BackendKind::ALL {
            let b = for_kind(kind);
            let s = b.signals();
            assert!(
                !s.prompt.is_empty() || !s.idle_hints.is_empty(),
                "{kind:?} has no way to signal Waiting",
            );
            assert!(
                !s.needs_input.is_empty(),
                "{kind:?} has no way to recognize approval prompts",
            );
            assert!(s.settle_ms > 0, "{kind:?} settle_ms must be > 0");
        }
    }

    #[test]
    fn every_backend_has_runtime_metadata() {
        let available = available();
        assert_eq!(available.len(), BackendKind::ALL.len());
        assert!(available.iter().all(|backend| !backend.label.is_empty()));
        assert_eq!(BackendKind::Codex.info().execution_policies, CODEX_POLICIES);
        assert!(BackendKind::ALL
            .into_iter()
            .filter(|kind| *kind != BackendKind::Codex)
            .all(|kind| kind.info().execution_policies == UNRESTRICTED_ONLY));
        let fork_backends = available
            .iter()
            .filter(|backend| backend.fork)
            .map(|backend| backend.id)
            .collect::<Vec<_>>();
        assert_eq!(fork_backends, [BackendKind::Claude, BackendKind::Codex]);
    }

    #[test]
    fn discovery_capability_matches_implemented_backends() {
        let discoverable = BackendKind::ALL
            .into_iter()
            .filter(|kind| for_kind(*kind).supports_session_discovery())
            .collect::<Vec<_>>();
        assert_eq!(
            discoverable,
            [BackendKind::Claude, BackendKind::Codex, BackendKind::Grok]
        );
    }
}
