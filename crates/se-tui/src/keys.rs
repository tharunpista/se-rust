//! What this terminal actually delivers.
//!
//! Charter §4 forbids advertising a shortcut that does not arrive:
//! "do not advertise Shift+Enter unless it is actually
//! distinguishable". On Windows that is not a question with a static
//! answer. Whether Shift+Enter, Ctrl+Enter and Alt+Enter reach the
//! application depends on the terminal, on whether it negotiated the
//! kitty keyboard protocol, and on the user's own key bindings.
//!
//! So SE measures it. [`Probe`] records what a real keypress carried,
//! and the help bar advertises only what has been seen to work. Until a
//! chord has been observed, it is reported as unknown rather than
//! assumed in either direction -- an unproven shortcut and a broken one
//! are different states and the user deserves to see which they have.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A newline chord SE might offer, in the order it should be preferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Chord {
    /// Alt+Enter. The dependable one on Windows Terminal.
    AltEnter,
    /// Ctrl+J, which arrives as a control character rather than a
    /// modified Enter, so it survives terminals that drop modifiers.
    CtrlJ,
    /// Shift+Enter. Needs the kitty protocol or a user binding.
    ShiftEnter,
    /// Ctrl+Enter. Frequently indistinguishable from plain Enter.
    CtrlEnter,
}

impl Chord {
    pub fn label(self) -> &'static str {
        match self {
            Chord::AltEnter => "Alt+Enter",
            Chord::CtrlJ => "Ctrl+J",
            Chord::ShiftEnter => "Shift+Enter",
            Chord::CtrlEnter => "Ctrl+Enter",
        }
    }

    /// Which chord, if any, this key event represents.
    pub fn of(key: KeyEvent) -> Option<Chord> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if alt => Some(Chord::AltEnter),
            KeyCode::Enter if ctrl => Some(Chord::CtrlEnter),
            KeyCode::Enter if shift => Some(Chord::ShiftEnter),
            // Ctrl+J is the line-feed control character. Terminals
            // deliver it either as a modified 'j' or as a bare '\n'.
            KeyCode::Char('j') if ctrl => Some(Chord::CtrlJ),
            KeyCode::Char('\n') => Some(Chord::CtrlJ),
            _ => None,
        }
    }
}

/// Whether a chord has been seen to arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Delivery {
    /// Never pressed, so nothing is known. Not the same as broken.
    #[default]
    Unknown,
    /// Observed arriving with its modifier intact.
    Works,
}

/// A record of which chords this session has actually observed.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    alt_enter: Delivery,
    ctrl_j: Delivery,
    shift_enter: Delivery,
    ctrl_enter: Delivery,
}

impl Probe {
    pub fn new() -> Self {
        Probe::default()
    }

    /// Records a keypress. Called for every key, so the probe fills in
    /// as the user works rather than needing a setup wizard.
    pub fn observe(&mut self, key: KeyEvent) {
        if let Some(chord) = Chord::of(key) {
            *self.slot(chord) = Delivery::Works;
        }
    }

    fn slot(&mut self, chord: Chord) -> &mut Delivery {
        match chord {
            Chord::AltEnter => &mut self.alt_enter,
            Chord::CtrlJ => &mut self.ctrl_j,
            Chord::ShiftEnter => &mut self.shift_enter,
            Chord::CtrlEnter => &mut self.ctrl_enter,
        }
    }

    pub fn delivery(&self, chord: Chord) -> Delivery {
        match chord {
            Chord::AltEnter => self.alt_enter,
            Chord::CtrlJ => self.ctrl_j,
            Chord::ShiftEnter => self.shift_enter,
            Chord::CtrlEnter => self.ctrl_enter,
        }
    }

    /// The newline chord to advertise in the help bar.
    ///
    /// Prefers one actually observed to work. With nothing observed
    /// yet it names Alt+Enter, which is the one documented to be
    /// reliable on Windows Terminal -- and the help text says
    /// "or Ctrl+J" so the user has a fallback that needs no modifier
    /// negotiation at all.
    pub fn newline_hint(&self) -> &'static str {
        for chord in [
            Chord::AltEnter,
            Chord::CtrlJ,
            Chord::ShiftEnter,
            Chord::CtrlEnter,
        ] {
            if self.delivery(chord) == Delivery::Works {
                return chord.label();
            }
        }
        "Alt+Enter"
    }

    /// The `/keys` report: every chord and what is actually known about
    /// it. Unknown is reported as unknown, never as working.
    pub fn report(&self) -> String {
        let mut lines = vec![
            "keyboard delivery in this terminal".to_string(),
            String::new(),
            "  Newline chords — press one to test it; this fills in as you use SE:".to_string(),
        ];
        for chord in [
            Chord::AltEnter,
            Chord::CtrlJ,
            Chord::ShiftEnter,
            Chord::CtrlEnter,
        ] {
            let state = match self.delivery(chord) {
                Delivery::Works => "arrives",
                Delivery::Unknown => "not pressed yet — unknown, not broken",
            };
            lines.push(format!("    {:<12} {}", chord.label(), state));
        }
        lines.push(String::new());
        lines.push(format!(
            "  Currently advertised for newline: {}",
            self.newline_hint()
        ));
        lines.push(String::new());
        lines.push("  Always available, no modifier negotiation needed:".to_string());
        lines
            .push("    Enter        send        Esc          interrupt / close picker".to_string());
        lines
            .push("    /            commands    Ctrl+C       interrupt, twice to exit".to_string());
        lines.push("    Ctrl+U       clear       Ctrl+D       diagnostics".to_string());
        lines.push("    Ctrl+A/E     line start/end           PgUp/PgDn  scroll".to_string());
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn alt_enter_is_recognised_as_a_newline_chord() {
        assert_eq!(
            Chord::of(key(KeyCode::Enter, KeyModifiers::ALT)),
            Some(Chord::AltEnter)
        );
    }

    #[test]
    fn a_plain_enter_is_not_a_newline_chord() {
        // Plain Enter sends. If it registered as a newline chord the
        // probe would report every submission as proof that a modified
        // Enter works.
        assert_eq!(Chord::of(key(KeyCode::Enter, KeyModifiers::NONE)), None);
    }

    #[test]
    fn ctrl_j_is_recognised_in_both_the_forms_terminals_send_it() {
        assert_eq!(
            Chord::of(key(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Some(Chord::CtrlJ)
        );
        // Some terminals deliver the line feed directly.
        assert_eq!(
            Chord::of(key(KeyCode::Char('\n'), KeyModifiers::NONE)),
            Some(Chord::CtrlJ)
        );
    }

    #[test]
    fn nothing_is_claimed_to_work_before_it_has_been_pressed() {
        // The distinction the charter requires: unproven is not the
        // same as broken, and neither is the same as working.
        let probe = Probe::new();
        for chord in [
            Chord::AltEnter,
            Chord::CtrlJ,
            Chord::ShiftEnter,
            Chord::CtrlEnter,
        ] {
            assert_eq!(probe.delivery(chord), Delivery::Unknown);
        }
        assert!(probe.report().contains("not pressed yet"));
    }

    #[test]
    fn observing_a_chord_records_it_as_working() {
        let mut probe = Probe::new();
        probe.observe(key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(probe.delivery(Chord::ShiftEnter), Delivery::Works);
        // And only that one.
        assert_eq!(probe.delivery(Chord::AltEnter), Delivery::Unknown);
    }

    #[test]
    fn the_advertised_newline_prefers_a_chord_actually_observed() {
        let mut probe = Probe::new();
        // With nothing seen, the documented-reliable default.
        assert_eq!(probe.newline_hint(), "Alt+Enter");
        // Shift+Enter observed but Alt+Enter not: Alt+Enter is still
        // only an assumption, so advertise what was proven.
        probe.observe(key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(probe.newline_hint(), "Shift+Enter");
        // Once Alt+Enter is proven too, it wins on preference order.
        probe.observe(key(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(probe.newline_hint(), "Alt+Enter");
    }

    #[test]
    fn the_report_never_claims_an_unpressed_chord_arrives() {
        let probe = Probe::new();
        let report = probe.report();
        assert!(
            !report.contains("arrives"),
            "an untested terminal must not be described as working: {report}"
        );
    }

    #[test]
    fn the_report_lists_every_chord_and_the_unconditional_keys() {
        let mut probe = Probe::new();
        probe.observe(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        let report = probe.report();
        for chord in [
            Chord::AltEnter,
            Chord::CtrlJ,
            Chord::ShiftEnter,
            Chord::CtrlEnter,
        ] {
            assert!(report.contains(chord.label()), "{} missing", chord.label());
        }
        assert!(report.contains("arrives"), "Ctrl+J was observed");
        // The keys that need no negotiation are always worth stating.
        assert!(report.contains("Ctrl+C"));
        assert!(report.contains("Esc"));
    }
}
