//! Session-state classification from a rendered pane.
//!
//! No single signal is reliable on its own — byte-idle timing flaps with
//! spinner redraws, prompt glyphs stay on screen while generating, and
//! status hints change with upstream releases. Each backend exposes a
//! [`Signals`] bundle and [`classify`] combines them in the priority
//! order documented on the struct fields. Adding a backend = populate
//! one `Signals` literal.

use crate::session::SessionState;
use regex::Regex;

/// Only the bottom of the viewport describes the backend's current state.
/// Older prompts and status text remain in scrollback and must not win over a
/// newer modal or composer.
const SIGNAL_TAIL_LINES: usize = 12;

/// Per-backend patterns consumed by [`classify`]. Priority is the order
/// these fields are checked in `classify`: `needs_input` → `working` →
/// `idle_hints` → settle gate → `prompt`.
pub struct Signals {
    /// Matches → `Waiting`. Approval, authentication, and choice modals
    /// outrank working text because a modal may be drawn over a still-visible
    /// "esc to interrupt" status line.
    pub needs_input: Vec<Regex>,
    /// Matches → `Active`. App says it's generating (e.g. "esc to interrupt").
    pub working: Vec<Regex>,
    /// Matches → `Waiting`. App says it's at the prompt (e.g. "? for shortcuts").
    pub idle_hints: Vec<Regex>,
    /// Matches → `Waiting`, but only after `settle_ms` of byte silence —
    /// prompt glyphs stay drawn during work, so we don't trust them until
    /// the pane stops moving.
    pub prompt: Vec<Regex>,
    /// Byte-silence gate before `prompt` is consulted. 1500 ms comfortably
    /// clears the supported TUIs' periodic spinner redraws.
    pub settle_ms: u64,
}

/// Per-tick classifier input. `pane` is whatever approximates the user's
/// view (tmux `capture-pane -p` or an ANSI-stripped ring tail).
pub struct Snapshot<'a> {
    pub pane: &'a str,
    pub idle_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassificationReason {
    NeedsInput,
    Working,
    IdleHint,
    RecentOutput,
    Prompt,
    Ambiguous,
    CaptureFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    pub state: SessionState,
    pub reason: ClassificationReason,
}

/// Common modal text used by several terminal agents. Backend signal bundles
/// include these in addition to their own composer and working patterns.
pub fn common_needs_input_signals() -> Vec<Regex> {
    [
        r"(?mi)^\s*(?:Would you like to (?:run|allow|approve|continue|proceed)\b.*\?|Do you want to (?:allow|approve|continue|proceed)\b.*\?|(?:Permission|Approval|Authentication) required\b.*)\s*$",
        r"(?mi)^\s*(?:Press enter to confirm(?: or esc to cancel)?|Enter to select(?: .*Esc to cancel)?|Use (?:the )?arrow keys to (?:select|navigate).*)\s*$",
        r"(?mi)^\s*(?:Waiting for (?:approval|authentication)|Sign[- ]in required|Log[- ]in required)\b.*$",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("valid common needs-input regex"))
    .collect()
}

fn tail_lines(text: &str, count: usize) -> &str {
    let mut seen = 0;
    for (index, _) in text.match_indices('\n').rev() {
        seen += 1;
        if seen == count {
            return &text[index + 1..];
        }
    }
    text
}

/// Classify a tick. Precedence: `needs_input` > `working` > `idle_hints` >
/// settle gate > `prompt` > `Unknown`. Only the viewport tail is inspected so
/// stale status lines in scrollback do not override the current UI.
pub fn classify(snap: &Snapshot, signals: &Signals) -> Classification {
    let pane = tail_lines(snap.pane, SIGNAL_TAIL_LINES);
    let any = |regs: &[Regex]| regs.iter().any(|r| r.is_match(pane));
    if any(&signals.needs_input) {
        return Classification {
            state: SessionState::Waiting,
            reason: ClassificationReason::NeedsInput,
        };
    }
    if any(&signals.working) {
        return Classification {
            state: SessionState::Active,
            reason: ClassificationReason::Working,
        };
    }
    if any(&signals.idle_hints) {
        return Classification {
            state: SessionState::Waiting,
            reason: ClassificationReason::IdleHint,
        };
    }
    if snap.idle_ms < signals.settle_ms as i64 {
        return Classification {
            state: SessionState::Active,
            reason: ClassificationReason::RecentOutput,
        };
    }
    if any(&signals.prompt) {
        return Classification {
            state: SessionState::Waiting,
            reason: ClassificationReason::Prompt,
        };
    }
    Classification {
        state: SessionState::Unknown,
        reason: ClassificationReason::Ambiguous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(pattern: &str) -> Regex {
        Regex::new(pattern).unwrap()
    }

    fn signals_for_test() -> Signals {
        Signals {
            needs_input: common_needs_input_signals(),
            working: vec![r(r"(?mi)^\s*esc to interrupt\b")],
            idle_hints: vec![r(r"(?mi)^\s*\? for shortcuts\b")],
            prompt: vec![r(r"(?m)^❯\s*$")],
            settle_ms: 1500,
        }
    }

    /// Working regex is the top of the ladder: bytes-recent, prompt visible,
    /// idle-hint-maybe-there — none of that matters if the app itself says
    /// it's busy.
    #[test]
    fn working_signal_beats_everything() {
        let s = signals_for_test();
        let pane = "\
            ❯ \n\
              esc to interrupt                        Now using extra usage\n\
              ? for shortcuts\n\
        ";
        let snap = Snapshot {
            pane,
            idle_ms: 5_000, // well past the settle gate
        };
        assert_eq!(classify(&snap, &s).state, SessionState::Active);
    }

    /// With no working match, an idle-hint beats the byte-idle gate — the
    /// app is reporting "I'm at the prompt" even if something is still
    /// repainting.
    #[test]
    fn idle_hint_wins_over_settle_gate() {
        let s = signals_for_test();
        let pane = "❯ \n  ? for shortcuts";
        let snap = Snapshot {
            pane,
            idle_ms: 100, // bytes still recent
        };
        assert_eq!(classify(&snap, &s).state, SessionState::Waiting);
    }

    /// No hints either way + bytes are recent → stay Active. This is the
    /// spinner-repaint case: the prompt glyph is on screen but we can't
    /// trust it yet.
    #[test]
    fn recent_bytes_keep_active_even_if_prompt_visible() {
        let s = signals_for_test();
        let pane = "❯ ";
        let snap = Snapshot {
            pane,
            idle_ms: 500, // inside settle window
        };
        assert_eq!(classify(&snap, &s).state, SessionState::Active);
    }

    /// Bytes have stopped, no working/idle hints, prompt glyph visible →
    /// Waiting. The main bug fix: a session idle at the prompt finally
    /// classifies correctly.
    #[test]
    fn prompt_glyph_after_settle_flips_to_waiting() {
        let s = signals_for_test();
        let pane = "some output\n❯ ";
        let snap = Snapshot {
            pane,
            idle_ms: 3_000,
        };
        assert_eq!(classify(&snap, &s).state, SessionState::Waiting);
    }

    /// Nothing matches and bytes have stopped. Report uncertainty instead of
    /// claiming the backend is still working.
    #[test]
    fn unknown_pane_after_settle_is_unknown() {
        let s = signals_for_test();
        let pane = "just some weird output no patterns match";
        let snap = Snapshot {
            pane,
            idle_ms: 3_000,
        };
        assert_eq!(classify(&snap, &s).state, SessionState::Unknown);
    }

    #[test]
    fn approval_prompt_beats_stale_working_hint() {
        let s = signals_for_test();
        let pane = "\
• Ran gh pr view --json state\n\
  └ Pull request is ready\n\
\n\
  esc to interrupt\n\
\n\
  Would you like to run the following command?\n\
\n\
  1. Yes, proceed\n\
  2. Yes, and don't ask again for commands that start with `gh pr view`\n\
  3. No, and tell Codex what to do differently\n\
\n\
  Press enter to confirm or esc to cancel\n";
        let got = classify(
            &Snapshot {
                pane,
                idle_ms: 5_000,
            },
            &s,
        );
        assert_eq!(got.state, SessionState::Waiting);
        assert_eq!(got.reason, ClassificationReason::NeedsInput);
    }

    #[test]
    fn stale_signals_above_viewport_tail_are_ignored() {
        let s = signals_for_test();
        let pane = format!("esc to interrupt\n{}", "plain output\n".repeat(12));
        let got = classify(
            &Snapshot {
                pane: &pane,
                idle_ms: 5_000,
            },
            &s,
        );
        assert_eq!(got.state, SessionState::Unknown);
    }

    /// Empty signal lists still use byte recency, then honestly report that
    /// the settled pane is ambiguous.
    #[test]
    fn empty_signals_become_unknown_after_settle() {
        let s = Signals {
            needs_input: vec![],
            working: vec![],
            idle_hints: vec![],
            prompt: vec![],
            settle_ms: 1500,
        };
        let pane = "whatever";
        assert_eq!(
            classify(&Snapshot { pane, idle_ms: 100 }, &s).state,
            SessionState::Active,
        );
        assert_eq!(
            classify(
                &Snapshot {
                    pane,
                    idle_ms: 10_000,
                },
                &s,
            )
            .state,
            SessionState::Unknown,
        );
    }
}
