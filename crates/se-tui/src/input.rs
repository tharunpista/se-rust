//! Input translated into intent.
//!
//! Keys map to an [`Action`] by a pure function, separately from the
//! loop that performs them. That split keeps "what does Esc mean right
//! now" in one readable place and lets the keymap be tested exhaustively
//! without a terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};

use crate::state::{Activity, AppState};

/// What a keypress means, given the current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Explicit quit (Ctrl+Q). Ctrl+C is separate, via [`Action::CtrlC`].
    Quit,
    /// Ctrl+C, resolved against state and timing in the loop (one press
    /// interrupts or arms, a second within the window exits).
    CtrlC,
    /// Send the composer's contents to the core.
    Submit(String),
    /// Interrupt the running task (Esc).
    Interrupt,
    /// Answer a pending approval.
    Decide(bool),
    // Composer editing.
    InsertChar(char),
    InsertNewline,
    Backspace,
    Delete,
    CursorLeft,
    CursorRight,
    LineStart,
    LineEnd,
    ClearComposer,
    // Transcript navigation.
    ScrollUp(u16),
    ScrollDown(u16),
    ScrollToLatest,
    ScrollToOldest,
    ToggleDiagnostics,
}

/// Maps a keypress to intent. Pure: no I/O, no mutation.
///
/// Precedence, deliberately narrow:
/// 1. a pending approval owns `y`/`n` and nothing else types into it;
/// 2. window-level keys (quit, interrupt, Ctrl+C, diagnostics, scroll);
/// 3. otherwise the composer has the keyboard.
pub fn action_for_key(state: &AppState, key: KeyEvent) -> Action {
    // Windows reports press and release; acting on both doubles every
    // keystroke. Only presses (and repeats) are intent.
    if key.kind == KeyEventKind::Release {
        return Action::Nothing;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // A decision is pending: only the answer and an interrupt mean
    // anything. Typing is ignored, because the user is being asked a
    // question, not composing.
    if state.awaiting_approval().is_some() {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Action::Decide(true),
            KeyCode::Char('n') | KeyCode::Char('N') => Action::Decide(false),
            KeyCode::Esc => Action::Interrupt,
            KeyCode::Char('c') if ctrl => Action::CtrlC,
            _ => Action::Nothing,
        };
    }

    match key.code {
        KeyCode::Char('c') if ctrl => Action::CtrlC,
        KeyCode::Char('q') if ctrl => Action::Quit,
        KeyCode::Char('d') if ctrl => Action::ToggleDiagnostics,
        KeyCode::Char('u') if ctrl => Action::ClearComposer,
        // readline-style line motion, a consistent documented policy in
        // place of a selection model.
        KeyCode::Char('a') if ctrl => Action::LineStart,
        KeyCode::Char('e') if ctrl => Action::LineEnd,

        // Esc interrupts a running task and never quits the application
        // (the directive is explicit). When idle it does nothing.
        KeyCode::Esc => match state.activity {
            Activity::Idle => Action::Nothing,
            _ => Action::Interrupt,
        },

        // Enter sends. A newline is Shift+Enter, and also Alt+Enter,
        // because Windows Terminal does not reliably deliver the SHIFT
        // modifier with Enter unless the user has bound it -- Alt+Enter
        // is the dependable alternative.
        KeyCode::Enter if shift || alt => Action::InsertNewline,
        KeyCode::Enter => {
            let text = state.composer.text().trim();
            if text.is_empty() {
                Action::Nothing
            } else {
                Action::Submit(state.composer.text().to_string())
            }
        }

        // Composer editing.
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left => Action::CursorLeft,
        KeyCode::Right => Action::CursorRight,
        KeyCode::Home => Action::LineStart,
        KeyCode::End => Action::LineEnd,

        // Transcript navigation. Up/Down scroll one line (vertical
        // cursor motion across wrapped composer lines is not
        // implemented; Left/Right/Home/End cover editing).
        KeyCode::Up => Action::ScrollUp(1),
        KeyCode::Down => Action::ScrollDown(1),
        KeyCode::PageUp => Action::ScrollUp(10),
        KeyCode::PageDown => Action::ScrollDown(10),

        KeyCode::Char(c) if !ctrl && !alt => Action::InsertChar(c),
        _ => Action::Nothing,
    }
}

/// Maps a mouse event to intent. Scrolling only: a click target needs a
/// layout to resolve against, which this crate does not hold.
pub fn action_for_mouse(_state: &AppState, mouse: MouseEvent) -> Action {
    match mouse.kind {
        MouseEventKind::ScrollUp => Action::ScrollUp(3),
        MouseEventKind::ScrollDown => Action::ScrollDown(3),
        _ => Action::Nothing,
    }
}

/// Applies the purely local part of an action.
///
/// Actions needing the core ([`Action::Submit`], [`Action::Interrupt`],
/// [`Action::Decide`], [`Action::CtrlC`]) are returned untouched: this
/// crate must not hold a core handle and reach through it, which is how
/// the previous implementation ran a model call from a key handler.
pub fn apply_local(state: &mut AppState, action: &Action) {
    match action {
        Action::InsertChar(c) => state.composer.insert_char(*c),
        Action::InsertNewline => state.composer.insert_newline(),
        Action::Backspace => state.composer.backspace(),
        Action::Delete => state.composer.delete(),
        Action::CursorLeft => state.composer.left(),
        Action::CursorRight => state.composer.right(),
        Action::LineStart => state.composer.home(),
        Action::LineEnd => state.composer.end(),
        Action::ClearComposer => state.composer.clear(),
        Action::Submit(_) => state.composer.clear(),
        Action::Quit => state.should_quit = true,
        Action::ScrollUp(n) => {
            state.scroll = state.scroll.saturating_add(*n);
            state.clamp_scroll();
        }
        Action::ScrollDown(n) => state.scroll = state.scroll.saturating_sub(*n),
        Action::ScrollToLatest => state.scroll = 0,
        Action::ScrollToOldest => {
            state.scroll = u16::MAX;
            state.clamp_scroll();
        }
        Action::ToggleDiagnostics => state.show_diagnostics = !state.show_diagnostics,
        // Core-bound or state/timing-resolved elsewhere.
        Action::Nothing | Action::Interrupt | Action::Decide(_) | Action::CtrlC => {}
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

    fn typed(s: &str) -> AppState {
        let mut state = AppState::new();
        for c in s.chars() {
            apply_local(&mut state, &Action::InsertChar(c));
        }
        state
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
    fn typing_builds_the_composer_and_enter_submits_it_once() {
        let mut s = AppState::new();
        for c in "hi there".chars() {
            let a = action_for_key(&s, press(KeyCode::Char(c)));
            apply_local(&mut s, &a);
        }
        assert_eq!(s.composer.text(), "hi there");
        let submit = action_for_key(&s, press(KeyCode::Enter));
        assert_eq!(submit, Action::Submit("hi there".to_string()));
    }

    #[test]
    fn a_key_release_is_ignored_so_windows_does_not_double_a_keystroke() {
        let s = AppState::new();
        let mut release = press(KeyCode::Char('x'));
        release.kind = KeyEventKind::Release;
        assert_eq!(action_for_key(&s, release), Action::Nothing);
    }

    #[test]
    fn a_key_repeat_still_types() {
        // Held keys arrive as Repeat on Windows; those are real input.
        let s = AppState::new();
        let mut repeat = press(KeyCode::Char('x'));
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(action_for_key(&s, repeat), Action::InsertChar('x'));
    }

    #[test]
    fn shift_enter_and_alt_enter_both_insert_a_newline() {
        let s = typed("line one");
        assert_eq!(
            action_for_key(&s, with_mods(KeyCode::Enter, KeyModifiers::SHIFT)),
            Action::InsertNewline
        );
        // The Windows-reliable alternative.
        assert_eq!(
            action_for_key(&s, with_mods(KeyCode::Enter, KeyModifiers::ALT)),
            Action::InsertNewline
        );
    }

    #[test]
    fn an_empty_composer_does_not_submit() {
        let mut s = AppState::new();
        assert_eq!(action_for_key(&s, press(KeyCode::Enter)), Action::Nothing);
        for c in "   \n  ".chars() {
            apply_local(&mut s, &Action::InsertChar(c));
        }
        assert_eq!(action_for_key(&s, press(KeyCode::Enter)), Action::Nothing);
    }

    #[test]
    fn cursor_keys_move_within_the_composer() {
        let mut s = typed("abc");
        let a = action_for_key(&s, press(KeyCode::Left));
        apply_local(&mut s, &a);
        apply_local(&mut s, &Action::InsertChar('X'));
        assert_eq!(s.composer.text(), "abXc");

        let a = action_for_key(&s, press(KeyCode::Home));
        apply_local(&mut s, &a);
        apply_local(&mut s, &Action::InsertChar('Y'));
        assert_eq!(s.composer.text(), "YabXc");

        let a = action_for_key(&s, press(KeyCode::End));
        apply_local(&mut s, &a);
        apply_local(&mut s, &Action::InsertChar('Z'));
        assert_eq!(s.composer.text(), "YabXcZ");
    }

    #[test]
    fn delete_removes_at_the_cursor_and_backspace_before_it() {
        let mut s = typed("abc");
        apply_local(&mut s, &Action::LineStart);
        let a = action_for_key(&s, press(KeyCode::Delete));
        apply_local(&mut s, &a);
        assert_eq!(s.composer.text(), "bc");
        apply_local(&mut s, &Action::LineEnd);
        let a = action_for_key(&s, press(KeyCode::Backspace));
        apply_local(&mut s, &a);
        assert_eq!(s.composer.text(), "b");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_are_line_start_and_end() {
        let s = typed("hello");
        assert_eq!(
            action_for_key(&s, with_mods(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Action::LineStart
        );
        assert_eq!(
            action_for_key(&s, with_mods(KeyCode::Char('e'), KeyModifiers::CONTROL)),
            Action::LineEnd
        );
    }

    #[test]
    fn ctrl_c_is_always_the_ctrl_c_action_not_an_immediate_quit() {
        // The two-press decision is made in the loop with state+timing;
        // the keymap only reports that Ctrl+C happened.
        for s in [AppState::new(), working(), awaiting()] {
            assert_eq!(
                action_for_key(&s, with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                Action::CtrlC
            );
        }
    }

    #[test]
    fn esc_interrupts_a_running_task_and_never_quits() {
        // Idle: nothing (explicitly not a quit).
        assert_eq!(
            action_for_key(&AppState::new(), press(KeyCode::Esc)),
            Action::Nothing
        );
        // Working: interrupt.
        assert_eq!(
            action_for_key(&working(), press(KeyCode::Esc)),
            Action::Interrupt
        );
        // Awaiting approval: interrupt the whole task.
        assert_eq!(
            action_for_key(&awaiting(), press(KeyCode::Esc)),
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
        // Ordinary typing is ignored, not buffered into a hidden composer.
        assert_eq!(
            action_for_key(&s, press(KeyCode::Char('z'))),
            Action::Nothing
        );
        assert_eq!(action_for_key(&s, press(KeyCode::Enter)), Action::Nothing);
    }

    #[test]
    fn scrolling_moves_back_from_the_newest_and_clamps() {
        let mut s = AppState::new();
        s.last_total_rows = 100;
        s.last_viewport_rows = 10;
        let a = action_for_key(&s, press(KeyCode::PageUp));
        apply_local(&mut s, &a);
        assert_eq!(s.scroll, 10);
        let a = action_for_key(&s, press(KeyCode::PageDown));
        apply_local(&mut s, &a);
        assert_eq!(s.scroll, 0);
        // Never underflows past the newest.
        apply_local(&mut s, &Action::ScrollDown(50));
        assert_eq!(s.scroll, 0);
        // Never overshoots the oldest: max is total - viewport = 90.
        apply_local(&mut s, &Action::ScrollToOldest);
        assert_eq!(s.scroll, 90);
    }

    #[test]
    fn up_and_down_scroll_the_transcript_by_a_line() {
        let mut s = AppState::new();
        s.last_total_rows = 100;
        s.last_viewport_rows = 10;
        let a = action_for_key(&s, press(KeyCode::Up));
        apply_local(&mut s, &a);
        assert_eq!(s.scroll, 1);
        let a = action_for_key(&s, press(KeyCode::Down));
        apply_local(&mut s, &a);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn core_bound_actions_are_not_applied_locally() {
        let mut s = working();
        let before = format!("{:?}", s.activity);
        apply_local(&mut s, &Action::Interrupt);
        apply_local(&mut s, &Action::Decide(true));
        apply_local(&mut s, &Action::CtrlC);
        assert_eq!(format!("{:?}", s.activity), before);
        assert!(!s.should_quit);
    }
}
