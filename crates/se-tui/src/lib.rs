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
pub mod keys;
pub mod picker;
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
    specialists: Vec<se_command::CommandSpec>,
) -> io::Result<()> {
    let mut state = AppState::new();
    discover_and_register_commands(&mut state);
    // Pre-built by the caller from the real catalogue -- this crate
    // never calls the catalogue loader itself (construction stays in
    // `se-core`/the binary, per the architecture doc's rule that the
    // TUI never reaches in on its own). Plain `CommandSpec`s go through
    // exactly the same collision/precedence handling as any other
    // registered command.
    for spec in specialists {
        let _ = state.commands.register(spec);
    }
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
                    // Recorded before the action is resolved, so the
                    // probe sees what the terminal delivered even for a
                    // chord SE does not currently bind.
                    state.keys.observe(key);
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
            // Charter §3.1: a registry-known slash path is never sent
            // to the model, whether it was accepted from the picker or
            // typed out in full and submitted directly -- the picker
            // only ever closes on whitespace, so a command with
            // arguments reaches here, not `Action::PickerAccept`.
            if let Some((path, raw_args)) = leading_command(text, &state.commands) {
                run_command(state, core, &path, &raw_args);
                return true;
            }
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
        Action::PickerAccept => {
            // Charter §3.1: a command is never submitted to the model
            // as a prompt. The composer is cleared here and no
            // `core.submit` happens on this path at all, which is the
            // structural guarantee -- not a string check before
            // sending.
            if let Some(path) = state.highlighted_command_path() {
                state.picker.close();
                state.composer.clear();
                run_command(state, core, &path, "");
            } else {
                // Nothing matched; leave the draft alone so the user
                // can correct it rather than losing what they typed.
                state.picker.close();
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

/// Loads custom commands from the project and global command
/// directories and registers them over the builtins already in
/// `state.commands`.
///
/// `.se/commands` in the current directory is `Provenance::Project`;
/// `~/.se/commands` (`%USERPROFILE%` on Windows, `$HOME` elsewhere) is
/// `Provenance::Global`. Neither directory existing is the ordinary
/// case, not a failure (`se_command::discovery::scan` already treats a
/// missing directory as empty). A file that failed to parse is
/// reported as a notice naming the real path, per charter §5.1's "an
/// invalid [definition] is reported, not silently skipped" -- applied
/// here to commands as it is to agents.
pub fn discover_and_register_commands(state: &mut AppState) {
    let mut load = |root: std::path::PathBuf, provenance: se_command::Provenance| {
        let (loaded, errors) = se_command::discovery::scan(&root, provenance);
        for se_command::discovery::Loaded { spec, def } in loaded {
            let id = spec.id.clone();
            // A collision is itself worth surfacing: the registry
            // already decides the winner by precedence, but silence
            // here would hide that two sources defined the same path.
            match state.commands.register(spec) {
                Ok(se_command::RegisterOutcome::Shadowed { winner_id }) => {
                    state.push(Entry::Notice(format!(
                        "a loaded command was shadowed by {winner_id} (higher precedence already owns that path)"
                    )));
                }
                Ok(_) => {
                    state.templates.insert(id, def);
                }
                Err(se_command::RegisterError::SecurityCriticalShadowed { path, builtin_id }) => {
                    state.push(Entry::Notice(format!(
                        "/{path} cannot be redefined -- it belongs to the built-in {builtin_id}"
                    )));
                }
            }
        }
        for e in errors {
            state.push(Entry::Notice(format!("command file skipped: {e}")));
        }
    };

    if let Ok(cwd) = std::env::current_dir() {
        load(
            cwd.join(".se").join("commands"),
            se_command::Provenance::Project,
        );
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok();
    if let Some(home) = home {
        load(
            std::path::PathBuf::from(home).join(".se").join("commands"),
            se_command::Provenance::Global,
        );
    }
}

/// If `text` names a registered command, returns its path and the
/// remaining text as raw arguments. Returns `None` for ordinary prose,
/// including prose that happens to contain a slash -- only a leading
/// `/<known-path>` counts.
fn leading_command(text: &str, commands: &se_command::Registry) -> Option<(String, String)> {
    let after_slash = text.trim_start().strip_prefix('/')?;
    let (word, rest) = match after_slash.find(char::is_whitespace) {
        Some(i) => (&after_slash[..i], after_slash[i..].trim_start()),
        None => (after_slash, ""),
    };
    commands.resolve(word).ok()?;
    Some((word.to_string(), rest.to_string()))
}

/// Executes a resolved slash command.
///
/// Phase 1 scope: every command resolves and reports honestly. A
/// command whose subsystem is not built yet says so with the reason
/// from its builtin definition, which is charter §1's requirement that
/// nothing appear to work when it does not. A loaded prompt-template
/// command renders its body with `raw_args` and submits the *rendered*
/// text as the turn -- the command path and arguments themselves never
/// reach the model.
pub fn run_command(state: &mut AppState, core: &CoreHandle, path: &str, raw_args: &str) {
    let Ok(spec) = state.commands.resolve(path) else {
        state.push(Entry::Notice(format!("unknown command: /{path}")));
        return;
    };
    let id = spec.id.clone();
    if spec.handler == se_command::HandlerKind::PromptTemplate {
        run_prompt_template(state, core, &id, path, raw_args);
        return;
    }
    if spec.handler == se_command::HandlerKind::SpecialistInvoke {
        // `spec.path` is the bare catalogue agent id; `spec.id` is
        // namespaced (`specialist:<id>`) specifically so a specialist
        // can never collide with a short builtin id under the
        // registry's same-id-is-a-reload rule.
        run_specialist_invoke(state, core, &spec.path.clone(), path, raw_args);
        return;
    }
    match se_command::builtins::availability(&id) {
        Some(se_command::builtins::Availability::Ready) => match id.as_str() {
            "exit" => state.should_quit = true,
            "keys" => {
                let report = state.keys.report();
                state.push(Entry::Notice(report));
            }
            "help" | "commands" => {
                let mut lines = vec!["commands:".to_string()];
                for category in se_command::builtins::CATEGORIES {
                    let in_category: Vec<&se_command::CommandSpec> = state
                        .commands
                        .query("")
                        .into_iter()
                        .filter(|c| c.category == *category)
                        .collect();
                    if in_category.is_empty() {
                        continue;
                    }
                    lines.push(format!("  {category}"));
                    for c in in_category {
                        lines.push(format!("    /{:<12} {}", c.path, c.description));
                    }
                }
                state.push(Entry::Notice(lines.join("\n")));
            }
            // Ready, but its panel is Phase 1's honest-empty state: the
            // registry and providers exist, the browser UI does not.
            other => state.push(Entry::Notice(format!(
                "/{other} is available but its panel is not built yet — \
                 use the equivalent `se {other}` subcommand for now"
            ))),
        },
        Some(se_command::builtins::Availability::NotImplemented(reason))
        | Some(se_command::builtins::Availability::Unconfigured(reason)) => {
            state.push(Entry::Notice(format!(
                "/{path} is not available yet: {reason}"
            )));
        }
        None => state.push(Entry::Notice(format!(
            "/{path} has no availability record — this is a bug in the builtin table"
        ))),
    }
}

/// Renders a loaded command's template against `raw_args` and submits
/// the result as a normal turn. The template body is cached at
/// discovery time in `state.templates`; its absence here means the
/// registry and the cache disagree, which is a bug in the wiring
/// between `discover_and_register_commands` and this function, not a
/// user-facing condition -- reported rather than panicking, since a
/// bug in the TUI is still not a reason to crash it.
fn run_prompt_template(
    state: &mut AppState,
    core: &CoreHandle,
    id: &str,
    path: &str,
    raw_args: &str,
) {
    let Some(def) = state.templates.get(id).cloned() else {
        state.push(Entry::Notice(format!(
            "/{path} is registered as a template command but its body was not cached — this is a bug"
        )));
        return;
    };
    let rendered = se_command::markdown::render(&def, raw_args);
    if let Err(e) = core.submit(&rendered, Vec::new()) {
        state.push(Entry::Notice(e.to_string()));
    }
}

/// Dispatches `raw_args` as a turn routed through the specialist named
/// `specialist_id` (the catalogue's own bare agent id, not the
/// namespaced `CommandSpec::id`).
///
/// A specialist invoked with no trailing text is refused here rather
/// than submitted as an empty turn -- this is also what makes
/// selecting one from the picker (which always calls this with an
/// empty `raw_args`) a safe no-op rather than a silent, meaningless
/// submission.
fn run_specialist_invoke(
    state: &mut AppState,
    core: &CoreHandle,
    specialist_id: &str,
    path: &str,
    raw_args: &str,
) {
    if raw_args.trim().is_empty() {
        state.push(Entry::Notice(format!(
            "/{path} needs a request after it, e.g. /{path} <what you want>"
        )));
        return;
    }
    if let Err(e) = core.submit_specialist(specialist_id, raw_args, Vec::new()) {
        state.push(Entry::Notice(e.to_string()));
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

    #[test]
    fn accepting_a_command_never_submits_it_to_the_model() {
        // The charter's hardest slash-command requirement: `/models`
        // must not be sent as a prompt. Proven against a real core --
        // if the command reached `submit`, the echo runner would put
        // the text in the transcript as a reply and the core would
        // report itself busy.
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        for c in "/models".chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }
        assert!(state.picker.is_open());
        dispatch(&mut state, &core, Action::PickerAccept);

        assert!(!core.is_busy(), "no turn may have been submitted");
        pump(&mut state, &core);
        // Nothing the user "said" and nothing the model "replied".
        let has_user_turn = state.transcript.iter().any(|e| matches!(e, Entry::User(_)));
        assert!(!has_user_turn, "the command must not appear as a user turn");
        // And the composer was cleared rather than left dirty.
        assert!(state.composer.is_empty());
        core.shutdown();
    }

    #[test]
    fn an_ordinary_prompt_containing_a_slash_still_reaches_the_model() {
        // The guard above must not have made plain prompts unsendable.
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        let text = "please read src/lib.rs";
        for c in text.chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }
        assert!(!state.picker.is_open());
        dispatch(&mut state, &core, Action::Submit(text.to_string()));
        pump_until_idle(&mut state, &core, Duration::from_secs(2));
        assert!(
            state
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::User(t) if t.contains("src/lib.rs"))),
            "a prompt with a path in it is not a command"
        );
        core.shutdown();
    }

    /// A command directory under the OS temp dir, removed on drop.
    struct TempCommandDir(std::path::PathBuf);

    impl TempCommandDir {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let path = std::env::temp_dir()
                .join(format!("se-tui-command-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempCommandDir(path)
        }

        fn write(&self, name: &str, contents: &str) -> &Self {
            std::fs::write(self.0.join(name), contents).unwrap();
            self
        }
    }

    impl Drop for TempCommandDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_loaded_template_command_renders_its_body_and_submits_that_not_the_invocation() {
        let dir = TempCommandDir::new();
        dir.write("hello.md", "Translate $0 to $1.");
        let (loaded, errors) = se_command::discovery::scan(&dir.0, se_command::Provenance::Project);
        assert!(errors.is_empty(), "{errors:?}");

        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        let id = loaded[0].spec.id.clone();
        state.commands.register(loaded[0].spec.clone()).unwrap();
        state.templates.insert(id, loaded[0].def.clone());

        run_command(&mut state, &core, "hello", "world French");
        pump_until_idle(&mut state, &core, Duration::from_secs(2));

        let sent = state
            .transcript
            .iter()
            .find_map(|e| match e {
                Entry::User(t) => Some(t.clone()),
                _ => None,
            })
            .expect("the rendered template must be submitted as a turn");
        assert_eq!(sent, "Translate world to French.");
        // The literal invocation text never appears anywhere.
        assert!(!state
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::User(t) if t.contains("$0"))));
        core.shutdown();
    }

    #[test]
    fn typing_a_known_command_with_arguments_and_pressing_enter_runs_it_not_the_model() {
        // The picker closes the moment whitespace is typed, so a
        // command with arguments reaches Submit, not PickerAccept --
        // this is the path that matters for every real invocation.
        let dir = TempCommandDir::new();
        dir.write("echo-arg.md", "You said: $0");
        let (loaded, _) = se_command::discovery::scan(&dir.0, se_command::Provenance::Project);

        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        let id = loaded[0].spec.id.clone();
        state.commands.register(loaded[0].spec.clone()).unwrap();
        state.templates.insert(id, loaded[0].def.clone());

        let text = "/echo-arg hooray";
        for c in text.chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }
        // The space after the command word already closed the picker.
        assert!(!state.picker.is_open());
        dispatch(&mut state, &core, Action::Submit(text.to_string()));
        pump_until_idle(&mut state, &core, Duration::from_secs(2));

        let sent = state
            .transcript
            .iter()
            .find_map(|e| match e {
                Entry::User(t) => Some(t.clone()),
                _ => None,
            })
            .expect("the rendered command must reach the model");
        assert_eq!(sent, "You said: hooray");
        core.shutdown();
    }

    #[test]
    fn an_unknown_leading_slash_word_is_not_mistaken_for_a_command() {
        let reg = se_command::builtins::registry_with_builtins();
        assert_eq!(leading_command("/not-a-real-command hi", &reg), None);
        assert_eq!(leading_command("no leading slash here", &reg), None);
    }

    #[test]
    fn a_known_command_with_no_arguments_splits_cleanly() {
        let reg = se_command::builtins::registry_with_builtins();
        assert_eq!(
            leading_command("/doctor", &reg),
            Some(("doctor".to_string(), String::new()))
        );
        assert_eq!(
            leading_command("/doctor   ", &reg),
            Some(("doctor".to_string(), String::new()))
        );
    }

    #[test]
    fn an_unavailable_command_explains_why_instead_of_failing_silently() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        // `/agents` is honest-unavailable until the Phase 3 loader.
        for c in "/agents".chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }
        dispatch(&mut state, &core, Action::PickerAccept);
        let notice = state
            .transcript
            .iter()
            .find_map(|e| match e {
                Entry::Notice(t) => Some(t.clone()),
                _ => None,
            })
            .expect("an unavailable command must say something");
        assert!(notice.contains("not available yet"), "got {notice:?}");
        assert!(
            notice.contains("Phase 3"),
            "the reason must be specific: {notice:?}"
        );
        core.shutdown();
    }

    #[test]
    fn help_lists_every_registered_command_grouped_by_category() {
        // /help is a projection of the registry, so this also pins that
        // no command can exist in the registry and be missing from help.
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        run_command(&mut state, &core, "help", "");
        core.shutdown();
        let notice = state
            .transcript
            .iter()
            .find_map(|e| match e {
                Entry::Notice(t) => Some(t.clone()),
                _ => None,
            })
            .expect("help must print something");
        for spec in state.commands.query("") {
            assert!(
                notice.contains(&format!("/{}", spec.path)),
                "/{} is registered but missing from /help",
                spec.path
            );
        }
    }

    #[test]
    fn the_keys_command_reports_what_this_terminal_delivers() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        run_command(&mut state, &core, "keys", "");
        core.shutdown();
        let notice = state
            .transcript
            .iter()
            .find_map(|e| match e {
                Entry::Notice(t) => Some(t.clone()),
                _ => None,
            })
            .expect("/keys must print a report");
        assert!(notice.contains("Alt+Enter"));
        assert!(notice.contains("Shift+Enter"));
        // Charter §4: an untested chord is reported as unknown rather
        // than advertised as working.
        assert!(notice.contains("not pressed yet"));
    }

    #[test]
    fn a_newline_chord_observed_once_is_what_the_help_bar_then_advertises() {
        let mut state = AppState::new();
        assert_eq!(state.keys.newline_hint(), "Alt+Enter");
        state.keys.observe(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::SHIFT,
        ));
        assert_eq!(state.keys.newline_hint(), "Shift+Enter");
    }

    #[test]
    fn exit_quits_through_the_command_path() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        assert!(!state.should_quit);
        run_command(&mut state, &core, "exit", "");
        assert!(state.should_quit);
        core.shutdown();
    }

    // --- Phase 3 item 3: specialist invocation wired to `/` ---

    /// Records the `specialist_id` every submitted turn carried, so a
    /// test can assert on it without this crate needing to depend on
    /// `se-provider` outside of tests -- `Runner::run`'s signature
    /// requires naming `se_provider::Message`, which is why this type
    /// lives behind `[dev-dependencies]`, not the real dependency list.
    struct RecordingRunner {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    impl se_core::Runner for RecordingRunner {
        fn describe(&self) -> (String, String) {
            ("test".to_string(), "recording".to_string())
        }
        fn run(
            &self,
            turn: &se_protocol::UserTurn,
            _history: &[se_provider::Message],
            _sink: &mut se_core::TurnSink,
        ) -> se_protocol::Outcome {
            self.seen.lock().unwrap().push(turn.specialist_id.clone());
            se_protocol::Outcome::Completed {
                final_text: "ok".into(),
            }
        }
    }

    fn specialist_spec(id: &str) -> se_command::CommandSpec {
        se_command::CommandSpec {
            id: format!("specialist:{id}"),
            path: id.to_string(),
            aliases: Vec::new(),
            category: "Agents".to_string(),
            description: "a test specialist".to_string(),
            provenance: se_command::Provenance::Builtin,
            handler: se_command::HandlerKind::SpecialistInvoke,
            security_critical: false,
        }
    }

    #[test]
    fn a_specialist_command_is_registered_and_resolvable_by_its_bare_id() {
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        state
            .commands
            .register(specialist_spec("backend-lead"))
            .unwrap();
        let resolved = state.commands.resolve("backend-lead").unwrap();
        assert_eq!(resolved.handler, se_command::HandlerKind::SpecialistInvoke);
        assert_eq!(resolved.path, "backend-lead");
    }

    #[test]
    fn submitting_a_specialist_command_with_text_dispatches_through_submit_specialist() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let core = se_core::start(RecordingRunner { seen: seen.clone() });
        let mut state = AppState::new();
        state
            .commands
            .register(specialist_spec("backend-lead"))
            .unwrap();

        run_command(&mut state, &core, "backend-lead", "fix the checkout bug");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !core
            .drain_blocking(Duration::from_millis(10))
            .iter()
            .any(|e| e.is_terminal())
        {
            if std::time::Instant::now() > deadline {
                panic!("the turn never finished");
            }
        }
        core.shutdown();

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [Some("backend-lead".to_string())],
            "the turn the runner actually saw must carry the specialist id"
        );
    }

    #[test]
    fn selecting_a_specialist_with_no_trailing_text_does_not_submit_anything() {
        // This is exactly what the picker's accept path calls today:
        // `run_command(path, "")`. A specialist must refuse rather than
        // submit a meaningless empty turn.
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        state
            .commands
            .register(specialist_spec("backend-lead"))
            .unwrap();

        run_command(&mut state, &core, "backend-lead", "");

        assert!(
            !core.is_busy(),
            "an empty-args specialist invocation must never reach submit_specialist"
        );
        assert!(state
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::Notice(n) if n.contains("needs a request"))));
        core.shutdown();
    }
}
