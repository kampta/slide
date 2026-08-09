//! Active/Waiting classification from a rendered pane.
//!
//! No single signal is reliable on its own — byte-idle timing flaps with
//! spinner redraws, prompt glyphs stay on screen while generating, and
//! status hints change with upstream releases. Each backend exposes a
//! [`Signals`] bundle and [`classify`] combines them in the priority
//! order documented on the struct fields. Adding a backend = populate
//! one `Signals` literal.

use crate::session::SessionState;
use regex::Regex;

/// Per-backend patterns consumed by [`classify`]. Priority is the order
/// these fields are checked in `classify`: `working` → `idle_hints` →
/// settle gate → `prompt`.
pub struct Signals {
    /// Matches → `Active`. App says it's generating (e.g. "esc to interrupt").
    pub working: Vec<Regex>,
    /// Matches → `Waiting`. App says it's at the prompt (e.g. "? for shortcuts").
    pub idle_hints: Vec<Regex>,
    /// Matches → `Waiting`, but only after `settle_ms` of byte silence —
    /// prompt glyphs stay drawn during work, so we don't trust them until
    /// the pane stops moving.
    pub prompt: Vec<Regex>,
    /// Byte-silence gate before `prompt` is consulted. 1500 ms comfortably
    /// clears Claude/Codex per-second spinner redraws.
    pub settle_ms: u64,
}

/// Per-tick classifier input. `pane` is whatever approximates the user's
/// view (tmux `capture-pane -p` or an ANSI-stripped ring tail).
pub struct Snapshot<'a> {
    pub pane: &'a str,
    pub idle_ms: i64,
}

/// Classify a tick. Precedence: `working` > `idle_hints` > settle gate >
/// `prompt` > fall through to `Active`. Pure function — called ~2× per
/// session per second.
pub fn classify(snap: &Snapshot, signals: &Signals) -> SessionState {
    let any = |regs: &[Regex]| regs.iter().any(|r| r.is_match(snap.pane));
    if any(&signals.working) {
        return SessionState::Active;
    }
    if any(&signals.idle_hints) {
        return SessionState::Waiting;
    }
    if snap.idle_ms < signals.settle_ms as i64 {
        return SessionState::Active;
    }
    if any(&signals.prompt) {
        return SessionState::Waiting;
    }
    SessionState::Active
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(pattern: &str) -> Regex {
        Regex::new(pattern).unwrap()
    }

    fn signals_for_test() -> Signals {
        Signals {
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
        assert_eq!(classify(&snap, &s), SessionState::Active);
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
        assert_eq!(classify(&snap, &s), SessionState::Waiting);
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
        assert_eq!(classify(&snap, &s), SessionState::Active);
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
        assert_eq!(classify(&snap, &s), SessionState::Waiting);
    }

    /// Nothing matches, bytes have stopped. We don't know — fall through
    /// to Active so the session stays clickable.
    #[test]
    fn unknown_pane_after_settle_falls_through_to_active() {
        let s = signals_for_test();
        let pane = "just some weird output no patterns match";
        let snap = Snapshot {
            pane,
            idle_ms: 3_000,
        };
        assert_eq!(classify(&snap, &s), SessionState::Active);
    }

    /// Empty signal lists degrade gracefully: byte-idle is the only lever
    /// left, so the classifier behaves like the pre-layered code did.
    #[test]
    fn empty_signals_fall_back_to_byte_idle_only() {
        let s = Signals {
            working: vec![],
            idle_hints: vec![],
            prompt: vec![],
            settle_ms: 1500,
        };
        let pane = "whatever";
        assert_eq!(
            classify(&Snapshot { pane, idle_ms: 100 }, &s),
            SessionState::Active,
        );
        assert_eq!(
            classify(
                &Snapshot {
                    pane,
                    idle_ms: 10_000,
                },
                &s,
            ),
            SessionState::Active,
        );
    }
}
