//! SE's terminal interface.
//!
//! This crate draws and reads input. It does not execute work, and it
//! structurally cannot: its only route to the core is
//! [`se_core::CoreHandle`], whose submit returns a task id immediately
//! and whose `drain` never blocks. There is no provider, no process
//! spawn and no filesystem walk reachable from here, which is how
//! invariant I1 is enforced rather than merely intended.
//!
//! The loop is deliberately boring:
//!
//! ```text
//! loop {
//!     drain core events  -> fold into AppState
//!     poll input (short timeout) -> Action -> local effect or CoreOp
//!     draw if anything changed
//! }
//! ```
//!
//! Every step is bounded. Nothing in it can wait on a model.

pub mod composer;
pub mod input;
pub mod render;
pub mod state;

pub use composer::Composer;
pub use input::{action_for_key, action_for_mouse, apply_local, Action};
pub use state::{Activity, AppState, CtrlC, Entry, ToolState};

use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use ratatui::backend::Backend;
use ratatui::Terminal;
use se_core::CoreHandle;
use se_protocol::ApprovalDecision;

/// How long the loop waits for input before looking at the core again.
///
/// This is the tick that keeps streaming smooth: at 16ms the interface
/// wakes ~60 times a second, so a delta that arrives between keystrokes
/// is still drawn within one frame (the Phase 1 gate), while an idle
/// session costs almost nothing.
const TICK: Duration = Duration::from_millis(16);

/// Runs the interface until the user quits.
///
/// `banner`, if given, is shown above the transcript for the whole
/// session -- the caller's way to say "this is not a real model" when it
/// had to fall back to a local runner, so the honesty requirement is
/// enforced where the user is actually looking rather than only in a
/// line printed after the alternate screen has already closed.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    core: &CoreHandle,
    banner: Option<String>,
) -> io::Result<()> {
    let mut state = AppState::new();
    state.banner = banner;
    let mut dirty = true;

    while !state.should_quit {
        // 1. Core events first, so input is handled against the freshest
        //    state (pressing `y` must act on the approval that is
        //    actually pending).
        for event in core.drain() {
            state.apply(&event);
            dirty = true;
        }

        // 2. Input, with a short bounded wait.
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) => {
                    let action = action_for_key(&state, key);
                    dirty |= dispatch(&mut state, core, action);
                }
                Event::Mouse(mouse) => {
                    let action = action_for_mouse(&state, mouse);
                    dirty |= dispatch(&mut state, core, action);
                }
                Event::Paste(text) => {
                    // Bracketed paste must be enabled by the binary for
                    // this arm to ever fire; see `se`'s `enter()`.
                    // Pasted text, including embedded newlines, is
                    // inserted verbatim rather than key-by-key, so a
                    // multi-line paste cannot submit on its first
                    // newline the way un-bracketed paste would.
                    state.composer.insert_str(&text);
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }

        if dirty {
            terminal.draw(|f| render::draw(f, &mut state))?;
            dirty = false;
        }
    }
    Ok(())
}

/// Performs an action: local effects here, core-bound ones as operations.
/// Returns whether the view needs redrawing.
fn dispatch(state: &mut AppState, core: &CoreHandle, action: Action) -> bool {
    match &action {
        Action::Nothing => false,
        Action::Submit(text) => {
            apply_local(state, &action);
            match core.submit(text, Vec::new()) {
                Ok(_task) => {}
                Err(e) => {
                    // Refusals are shown, never swallowed: the user's
                    // text has already left the composer, so silence
                    // here would look like the message vanished.
                    state.push(Entry::Notice(e.to_string()));
                    state.composer.set(text.clone());
                }
            }
            true
        }
        Action::Interrupt => {
            if let Some(task) = state.running_task() {
                core.interrupt(task);
                state.status = "interrupting…".to_string();
            }
            true
        }
        Action::CtrlC => {
            // The explicit two-press machine: `AppState::ctrl_c` decides
            // based on current activity and timing since the last press.
            match state.ctrl_c(Instant::now()) {
                state::CtrlC::Interrupt(task) => core.interrupt(task),
                state::CtrlC::Quit => state.should_quit = true,
                state::CtrlC::Armed => {}
            }
            true
        }
        Action::Decide(allow) => {
            if let Some(call) = state.awaiting_approval() {
                core.approve(ApprovalDecision {
                    call,
                    allow: *allow,
                });
                // The transcript is updated by the resulting events, not
                // guessed at here.
            }
            true
        }
        _ => {
            apply_local(state, &action);
            true
        }
    }
}

/// Drives one iteration's worth of core draining and folding, without
/// touching the terminal or input.
///
/// Exists so the loop's state machine can be tested without a tty; the
/// interactive [`run`] uses the same folding path.
pub fn pump(state: &mut AppState, core: &CoreHandle) -> usize {
    let events = core.drain();
    let n = events.len();
    for event in events {
        state.apply(&event);
    }
    n
}

/// Pumps until the core reports a terminal event or `limit` elapses.
/// For tests and headless runs.
pub fn pump_until_idle(state: &mut AppState, core: &CoreHandle, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        pump(state, core);
        if !state.is_busy() && !core.is_busy() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use se_core::{ApprovalRunner, EchoRunner};

    /// The invariant the previous implementation broke, asserted
    /// directly: while a turn is running, the interface's own work must
    /// stay fast. Each pump-and-draw cycle is timed against a turn that
    /// deliberately takes far longer than a frame.
    #[test]
    fn the_interface_stays_responsive_while_a_slow_turn_runs() {
        let core = se_core::start(EchoRunner::slow(Duration::from_millis(40), 25));
        let mut state = AppState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        core.submit("a turn that takes about a second", vec![])
            .unwrap();

        let mut worst = Duration::ZERO;
        for _ in 0..40 {
            let started = Instant::now();
            pump(&mut state, &core);
            terminal.draw(|f| render::draw(f, &mut state)).unwrap();
            // Typing must also still be processed promptly.
            let action = action_for_key(&state, key('x'));
            apply_local(&mut state, &action);
            worst = worst.max(started.elapsed());
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(
            worst < Duration::from_millis(50),
            "a cycle took {worst:?}; the interface must never wait on the turn"
        );
        // And the turn really was still in flight for that whole time.
        assert!(
            state.is_busy() || !state.assistant_text().is_empty(),
            "the turn should have been producing output"
        );
        core.shutdown();
    }

    /// Streamed text must appear as it arrives, not at the end.
    #[test]
    fn deltas_are_rendered_while_the_turn_is_still_running() {
        let core = se_core::start(EchoRunner::slow(Duration::from_millis(25), 20));
        let mut state = AppState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        core.submit("one two three four five", vec![]).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_partial = false;
        while Instant::now() < deadline {
            pump(&mut state, &core);
            terminal.draw(|f| render::draw(f, &mut state)).unwrap();
            if state.is_busy() && !state.assistant_text().is_empty() {
                saw_partial = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            saw_partial,
            "text must be visible before the task finishes, not only after"
        );
        core.shutdown();
    }

    #[test]
    fn an_interrupt_typed_during_a_turn_actually_stops_it() {
        let core = se_core::start(EchoRunner::slow(Duration::from_millis(20), 200));
        let mut state = AppState::new();
        core.submit("something very long", vec![]).unwrap();

        // Let it start, then press Ctrl+C the way a user would.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !state.is_busy() {
            pump(&mut state, &core);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(state.is_busy(), "the turn should be running");

        let action = action_for_key(&state, ctrl('c'));
        assert_eq!(action, Action::CtrlC);
        dispatch(&mut state, &core, action);

        assert!(
            pump_until_idle(&mut state, &core, Duration::from_secs(5)),
            "the interrupted turn must reach a terminal state"
        );
        assert!(
            state
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::Notice(n) if n.contains("interrupted"))),
            "the transcript must record the interruption: {:?}",
            state.transcript
        );
        core.shutdown();
    }

    #[test]
    fn a_whole_turn_flows_from_keystrokes_to_rendered_answer() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        for c in "hello core".chars() {
            let action = action_for_key(&state, key(c));
            apply_local(&mut state, &action);
        }
        let submit = action_for_key(&state, key_code(crossterm::event::KeyCode::Enter));
        assert_eq!(submit, Action::Submit("hello core".to_string()));
        dispatch(&mut state, &core, submit);
        assert!(state.composer.is_empty(), "submitting clears the composer");

        assert!(pump_until_idle(&mut state, &core, Duration::from_secs(5)));
        terminal.draw(|f| render::draw(f, &mut state)).unwrap();

        assert_eq!(state.assistant_text(), "hello core");
        assert!(state
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::User(t) if t == "hello core")));
        core.shutdown();
    }

    #[test]
    fn approval_is_answered_from_the_keyboard_without_a_nested_event_loop() {
        // The previous implementation read the terminal directly from
        // inside the agent loop to get this keypress. Here it is an
        // ordinary key, handled by the ordinary loop.
        let core = se_core::start(ApprovalRunner { tool: "run_shell" });
        let mut state = AppState::new();
        core.submit("cargo test", vec![]).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut answered = false;
        while Instant::now() < deadline {
            pump(&mut state, &core);
            if !answered && state.awaiting_approval().is_some() {
                let action = action_for_key(&state, key('y'));
                assert_eq!(action, Action::Decide(true));
                dispatch(&mut state, &core, action);
                answered = true;
            }
            if answered && !state.is_busy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(answered, "approval must have been requested");
        assert!(
            state.transcript.iter().any(|e| matches!(
                e,
                Entry::Tool {
                    state: ToolState::Ok,
                    ..
                }
            )),
            "the approved tool must have run: {:?}",
            state.transcript
        );
        core.shutdown();
    }

    #[test]
    fn a_refused_submission_keeps_the_users_text_instead_of_losing_it() {
        let core = se_core::start(EchoRunner::slow(Duration::from_millis(30), 20));
        let mut state = AppState::new();
        core.submit("first", vec![]).unwrap();

        state.composer.set("second".to_string());
        let action = Action::Submit("second".to_string());
        dispatch(&mut state, &core, action);

        assert_eq!(
            state.composer.text(),
            "second",
            "a refused message must be recoverable, not silently dropped"
        );
        assert!(state
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::Notice(n) if n.contains("still running"))));
        core.shutdown();
    }

    #[test]
    fn quitting_is_the_only_thing_that_ends_the_loop() {
        let mut state = AppState::new();
        assert!(!state.should_quit);
        apply_local(&mut state, &Action::Quit);
        assert!(state.should_quit);
    }

    fn key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        )
    }

    fn key_code(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::CONTROL,
        )
    }
}
