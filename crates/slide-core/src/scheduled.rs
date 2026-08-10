use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub const MIN_INTERVAL_SECONDS: i64 = 60;
pub const MAX_INTERVAL_SECONDS: i64 = 365 * 24 * 60 * 60;
pub const MAX_JOBS_PER_SESSION: i64 = 50;
const MAX_TITLE_CHARS: usize = 100;
const MAX_PROMPT_CHARS: usize = 4_000;
const MAX_FUTURE_MS: i64 = 5 * 365 * 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    Once,
    Interval,
}

impl ScheduleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Interval => "interval",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "once" => Some(Self::Once),
            "interval" => Some(Self::Interval),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScheduledJob {
    pub id: String,
    pub session_id: String,
    pub title: String,
    pub prompt: String,
    pub schedule_kind: ScheduleKind,
    pub interval_seconds: Option<i64>,
    /// Canonical scheduled occurrence. Busy-session retries live separately
    /// so an interval never drifts from its original cadence.
    pub next_run_at: i64,
    pub retry_at: Option<i64>,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    pub last_error: Option<String>,
    pub run_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateScheduledJobRequest {
    pub title: String,
    pub prompt: String,
    pub schedule_kind: ScheduleKind,
    #[serde(default)]
    pub interval_seconds: Option<i64>,
    pub next_run_at: i64,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateScheduledJobRequest {
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

pub fn build_job(
    session_id: &str,
    request: CreateScheduledJobRequest,
    now: i64,
) -> Result<ScheduledJob> {
    let title = compact_field(&request.title, "title", MAX_TITLE_CHARS)?;
    let prompt = compact_field(&request.prompt, "prompt", MAX_PROMPT_CHARS)?;
    let interval_seconds = match request.schedule_kind {
        ScheduleKind::Once => {
            if request.interval_seconds.is_some() {
                bail!("one-time jobs must not include an interval");
            }
            None
        }
        ScheduleKind::Interval => {
            let interval = request
                .interval_seconds
                .ok_or_else(|| anyhow::anyhow!("interval jobs require interval_seconds"))?;
            if !(MIN_INTERVAL_SECONDS..=MAX_INTERVAL_SECONDS).contains(&interval) {
                bail!(
                    "interval_seconds must be between {MIN_INTERVAL_SECONDS} and {MAX_INTERVAL_SECONDS}"
                );
            }
            Some(interval)
        }
    };
    if request.next_run_at > now.saturating_add(MAX_FUTURE_MS) {
        bail!("next_run_at must be within five years");
    }
    // A form submission can cross its selected second while in flight. Treat
    // a past timestamp as immediately due instead of manufacturing a racey
    // validation failure.
    let next_run_at = request.next_run_at.max(now);
    Ok(ScheduledJob {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session_id.to_string(),
        title,
        prompt,
        schedule_kind: request.schedule_kind,
        interval_seconds,
        next_run_at,
        retry_at: None,
        enabled: request.enabled,
        last_run_at: None,
        last_error: None,
        run_count: 0,
        created_at: now,
        updated_at: now,
    })
}

pub fn next_interval_occurrence(job: &ScheduledJob, now: i64) -> Option<i64> {
    let interval_ms = job.interval_seconds?.checked_mul(1_000)?;
    if interval_ms <= 0 {
        return None;
    }
    if job.next_run_at > now {
        return Some(job.next_run_at);
    }
    let elapsed = now.saturating_sub(job.next_run_at);
    let slots = elapsed / interval_ms + 1;
    job.next_run_at.checked_add(slots.checked_mul(interval_ms)?)
}

pub fn dispatch_prompt(job: &ScheduledJob) -> String {
    format!("Slide scheduled job '{}'. Task: {}", job.title, job.prompt)
}

fn compact_field(value: &str, label: &str, max_chars: usize) -> Result<String> {
    if value
        .chars()
        .any(|character| character.is_control() && !character.is_whitespace())
    {
        bail!("{label} must not contain control characters");
    }
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        bail!("{label} is required");
    }
    if value.chars().count() > max_chars {
        bail!("{label} must be at most {max_chars} characters");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval_job(next_run_at: i64) -> ScheduledJob {
        build_job(
            "session",
            CreateScheduledJobRequest {
                title: "Check status".to_string(),
                prompt: "Inspect the latest run".to_string(),
                schedule_kind: ScheduleKind::Interval,
                interval_seconds: Some(60),
                next_run_at,
                enabled: true,
            },
            0,
        )
        .unwrap()
    }

    #[test]
    fn interval_skips_missed_slots_without_drifting() {
        let job = interval_job(100_000);
        assert_eq!(next_interval_occurrence(&job, 275_000), Some(280_000));
        assert_eq!(next_interval_occurrence(&job, 280_000), Some(340_000));
    }

    #[test]
    fn validation_is_bounded_and_schedule_specific() {
        let invalid = CreateScheduledJobRequest {
            title: "job".to_string(),
            prompt: "prompt".to_string(),
            schedule_kind: ScheduleKind::Once,
            interval_seconds: Some(60),
            next_run_at: 1,
            enabled: true,
        };
        assert!(build_job("session", invalid, 0).is_err());
        assert!(compact_field("bad\u{1b}", "prompt", 100).is_err());
        assert_eq!(compact_field(" a\n b ", "prompt", 100).unwrap(), "a b");
    }

    #[test]
    fn dispatch_is_one_terminal_line() {
        let job = interval_job(100_000);
        let prompt = dispatch_prompt(&job);
        assert!(prompt.contains("Check status"));
        assert!(!prompt.contains('\n'));
    }
}
