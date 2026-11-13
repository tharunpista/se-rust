//! Input translated into intent.
//!
//! Keys are mapped to an [`Action`] by a pure function, separately from
//! the loop that performs them. That split is what lets the keymap be
//! tested exhaustively without a terminal, and it keeps the decision
//! "what does Esc mean right now" in one readable place instead of
//! spread across a match arm per overlay.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};

use crate::state::{Activity, AppState};

/// What a keypress means, given the current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Nothing,
    Quit,
    /// Send the composer's contents to the core.
    Submit(String),
    /// Interrupt the running task.
    Interrupt,
    /// Answer a pending approval.
    Decide(bool),
    InsertChar(char),
    InsertNewline,
    Backspace,
    ClearComposer,
    ScrollUp(u16),
    ScrollDown(u16),
    ScrollToLatest,
    ScrollToOldest,
    ToggleDiagnostics,
}

/// Maps a keypress to intent. Pure: no I/O, no mutation.
///
/// Precedence is deliberate and narrow:
/// 1. a pending approval owns `y`/`n` and nothing else can be typed into
///    it by accident;
/// 2. interrupt works whenever something is running;
/// 3. otherwise the composer has the keyboard.
pub fn action_for_key(state: &AppState, key: KeyEvent) -> Action {
    // Windows reports both press and release; acting on both would
    // double every keystroke.
    if key.kind != KeyEventKind::Press {
        return Action::Nothing;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // A decision is pending: the only meaningful keys are the answer and
    // an interrupt. Typing is ignored rather than silently buffered,
    // because the user is being asked a question.
    if state.awaiting_approval().is_some() {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Action::Decide(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Action::Decide(false),
            KeyCode::Char('c') if ctrl => Action::Interrupt,
            _ => Action::Nothing,
        };
    }

    match key.code {
        // Ctrl+C interrupts work; it quits only when nothing is running,
        // so a long turn is never lost to a reflex.
        KeyCode::Char('c') if ctrl => match state.activity {
            Activity::Idle => Action::Quit,
            _ => Action::Interrupt,
        },
        KeyCode::Char('q') if ctrl => Action::Quit,
        KeyCode::Char('d') if ctrl => Action::ToggleDiagnostics,
        KeyCode::Char('u') if ctrl => Action::ClearComposer,
        KeyCode::Esc => match state.activity {
            Activity::Idle => Action::Quit,
            _ => Action::Interrupt,
        },

        // Shift+Enter adds a line; plain Enter sends. A multi-line
        // prompt must not be submitted halfway through composing it.
        KeyCode::Enter if shift => Action::InsertNewline,
        KeyCode::Enter => {
            let text = state.composer.trim();
            if text.is_empty() {
                Action::Nothing
            } else {
                Action::Submit(text.to_string())
            }
        }

        KeyCode::Backspace => Action::Backspace,
        KeyCode::PageUp => Action::ScrollUp(10),
        KeyCode::PageDown => Action::ScrollDown(10),
        KeyCode::Home => Action::ScrollToOldest,
        KeyCode::End => Action::ScrollToLatest,
        KeyCode::Char(c) if !ctrl => Action::InsertChar(c),
        _ => Action::Nothing,
    }
}

/// Maps a mouse event to intent. Scrolling only, for now: a click target
/// needs a layout to resolve against, which belongs with the renderer.
pub fn action_for_mouse(_state: &AppState, mouse: MouseEvent) -> Action {
    match mouse.kind {
        MouseEventKind::ScrollUp => Action::ScrollUp(3),
        MouseEventKind::ScrollDown => Action::ScrollDown(3),
        _ => Action::Nothing,
    }
}

/// Applies the purely local part of an action.
///
/// Actions that require the core ([`Action::Submit`],
/// [`Action::Interrupt`], [`Action::Decide`]) are returned to the caller
/// untouched, because this crate must not hold a core handle and reach
/// through it -- that is how the previous implementation ended up
/// executing a model call from a key handler.
pub fn apply_local(state: &mut AppState, action: &Action) {
    match action {
        Action::InsertChar(c) => state.composer.push(*c),
        Action::InsertNewline => state.composer.push('\n'),
        Action::Backspace => {
            state.composer.pop();
        }
        Action::ClearComposer => state.composer.clear(),
        Action::Submit(_) => state.composer.clear(),
        Action::Quit => state.should_quit = true,
        Action::ScrollUp(n) => state.scroll = state.scroll.saturating_add(*n),
        Action::ScrollDown(n) => state.scroll = state.scroll.saturating_sub(*n),
        Action::ScrollToLatest => state.scroll = 0,
        Action::ScrollToOldest => state.scroll = u16::MAX,
        Action::ToggleDiagnostics => state.show_diagnostics = !state.show_diagnostics,
        Action::Nothing | Action::Interrupt | Action::Decide(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_protocol::{TaskId, ToolCallId};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with_mods(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn working() -> AppState {
        let mut s = AppState::new();
        s.activity = Activity::Working {
            task: TaskId::from_raw(1),
        };
        s
    }

    fn awaiting() -> AppState {
        let mut s = AppState::new();
        s.activity = Activity::AwaitingApproval {
            task: TaskId::from_raw(1),
            call: ToolCallId::from_raw(1),
        };
        s
    }

    #[test]
    fn typing_builds_the_composer_and_enter_submits_it() {
        let mut s = AppState::new();
        for c in "hi there".chars() {
            let a = action_for_key(&s, press(KeyCode::Char(c)));
            apply_local(&mut s, &a);
        }
        assert_eq!(s.composer, "hi there");
        assert_eq!(
            action_for_key(&s, press(KeyCode::Enter)),
            Action::Submit("hi there".to_string())
        );
    }

    #[test]
    fn a_key_release_is_ignored_so_windows_does_not_double_every_keystroke() {
        let s = AppState::new();
        let mut release = press(KeyCode::Char('x'));
        release.kind = KeyEventKind::Release;
        assert_eq!(action_for_key(&s, release), Action::Nothing);
    }

    #[test]
    fn shift_enter_adds_a_line_and_does_not_submit() {
        let mut s = AppState::new();
        s.composer = "line one".to_string();
        let action = action_for_key(&s, with_mods(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(action, Action::InsertNewline);
        apply_local(&mut s, &action);
        assert_eq!(s.composer, "line one\n");
    }

    #[test]
    fn an_empty_composer_does_not_submit() {
        let mut s = AppState::new();
        assert_eq!(action_for_key(&s, press(KeyCode::Enter)), Action::Nothing);
        s.composer = "   \n  ".to_string();
        assert_eq!(action_for_key(&s, press(KeyCode::Enter)), Action::Nothing);
    }

    #[test]
    fn submitting_clears_the_composer() {
        let mut s = AppState::new();
        s.composer = "hello".to_string();
        let action = Action::Submit("hello".to_string());
        apply_local(&mut s, &action);
        assert!(s.composer.is_empty());
    }

    #[test]
    fn ctrl_c_interrupts_work_but_quits_when_idle() {
        let idle = AppState::new();
        assert_eq!(
            action_for_key(&idle, with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        // The important half: a reflexive Ctrl+C during a long turn must
        // stop the turn, not kill the session.
        assert_eq!(
            action_for_key(
                &working(),
                with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Action::Interrupt
        );
    }

    #[test]
    fn esc_interrupts_work_and_quits_when_idle() {
        assert_eq!(
            action_for_key(&AppState::new(), press(KeyCode::Esc)),
            Action::Quit
        );
        assert_eq!(
            action_for_key(&working(), press(KeyCode::Esc)),
            Action::Interrupt
        );
    }

    #[test]
    fn a_pending_approval_owns_the_keyboard() {
        let s = awaiting();
        assert_eq!(
            action_for_key(&s, press(KeyCode::Char('y'))),
            Action::Decide(true)
        );
        assert_eq!(
            action_for_key(&s, press(KeyCode::Char('n'))),
            Action::Decide(false)
        );
        assert_eq!(
            action_for_key(&s, press(KeyCode::Esc)),
            Action::Decide(false)
        );
        // Ordinary typing is ignored rather than quietly buffered into a
        // composer the user cannot currently see the point of.
        assert_eq!(
            action_for_key(&s, press(KeyCode::Char('z'))),
            Action::Nothing
        );
        assert_eq!(action_for_key(&s, press(KeyCode::Enter)), Action::Nothing);
    }

    #[test]
    fn an_interrupt_still_works_while_approval_is_pending() {
        assert_eq!(
            action_for_key(
                &awaiting(),
                with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Action::Interrupt
        );
    }

    #[test]
    fn scrolling_moves_back_from_the_newest_and_end_returns_to_it() {
        let mut s = AppState::new();
        let up = action_for_key(&s, press(KeyCode::PageUp));
        apply_local(&mut s, &up);
        assert_eq!(s.scroll, 10);
        let down = action_for_key(&s, press(KeyCode::PageDown));
        apply_local(&mut s, &down);
        assert_eq!(s.scroll, 0);
        // And it never underflows past the newest entry.
        apply_local(&mut s, &Action::ScrollDown(50));
        assert_eq!(s.scroll, 0);
        apply_local(&mut s, &Action::ScrollToOldest);
        assert_eq!(s.scroll, u16::MAX);
    }

    #[test]
    fn the_mouse_wheel_scrolls() {
        let s = AppState::new();
        let wheel = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            action_for_mouse(&s, wheel(MouseEventKind::ScrollUp)),
            Action::ScrollUp(3)
        );
        assert_eq!(
            action_for_mouse(&s, wheel(MouseEventKind::ScrollDown)),
            Action::ScrollDown(3)
        );
    }

    #[test]
    fn core_bound_actions_are_not_applied_locally() {
        // apply_local must leave these for the caller: this crate has no
        // business talking to the core itself.
        let mut s = working();
        let before = format!("{:?}", s.activity);
        apply_local(&mut s, &Action::Interrupt);
        apply_local(&mut s, &Action::Decide(true));
        assert_eq!(format!("{:?}", s.activity), before);
        assert!(!s.should_quit);
    }

    #[test]
    fn diagnostics_toggle_on_and_off() {
        let mut s = AppState::new();
        assert!(!s.show_diagnostics);
        let a = action_for_key(&s, with_mods(KeyCode::Char('d'), KeyModifiers::CONTROL));
        apply_local(&mut s, &a);
        assert!(s.show_diagnostics);
        apply_local(&mut s, &a);
        assert!(!s.show_diagnostics);
    }
}
