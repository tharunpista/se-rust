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

pub mod agents;
pub mod composer;
pub mod input;
pub mod keys;
pub mod picker;
pub mod render;
pub mod services;
pub mod sessions;
pub mod state;
pub mod team;

pub use composer::Composer;
pub use input::{action_for_key, action_for_key_at, action_for_mouse, apply_local, Action};
pub use services::{
    Inbound, KeySaver, ModelLister, Opened, ProviderEntry, ProviderLister, ProviderModels, Report,
    Services, SessionOpener,
};
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
    core: &mut CoreHandle,
    banner: Option<String>,
    specialists: Vec<se_command::CommandSpec>,
    services: Services,
    prior: Vec<Entry>,
) -> io::Result<()> {
    let mut state = AppState::new();
    state.mode = services.mode;
    core.set_mode(services.mode);
    state.model_listing = services::Listing::new(services.list_models);
    state.providers_report = services.providers;
    state.doctor_report = services.doctor;
    state.remember_model = services::Hook(services.remember_model);
    state.list_providers = services::Hook(services.list_providers);
    state.save_key = services::Hook(services.save_key);
    state.copy = services::Hook(services.copy);
    state.default_model = services.default_model;
    state.open_session = services::Hook(services.open_session);
    state.session_id = services.session_id;
    state.sessions_dir = services.sessions_dir;
    state.recent_projects = services.recent_projects;
    state.folder = folder_label();
    // A resumed conversation's earlier turns, drawn before anything new.
    for entry in prior {
        state.push(entry);
    }
    let mut panel = team::Panel::new(services.team);
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
    // The binary starts with capture off; the binary's restore() always
    // disables it on exit or panic, so only the toggles happen here.
    let mut capture_applied = false;

    while !state.should_quit {
        // 1. Core events first, so input is handled against the freshest
        //    state (pressing `y` must act on the approval that is
        //    actually pending).
        let model_before = state.turn_model.clone();
        for event in core.drain() {
            state.apply(&event);
            dirty = true;
        }
        if state.turn_model != model_before {
            record_model(&state);
        }
        dirty |= panel.tick(&mut state);
        dirty |= state.agents.clock_moved(std::time::Instant::now());
        dirty |= poll_models(&mut state);
        dirty |= drain_inbox(&mut state);

        // 2. Input, with a short bounded wait.
        if event::poll(TICK)? {
            // Gather everything arriving back to back, so an un-bracketed
            // (Windows console) paste is seen as one burst, not keystrokes.
            let mut batch = vec![event::read()?];
            // Only keys extend a batch, so a stream of mouse moves never
            // holds up drawing.
            while batch.len() < 65_536
                && matches!(batch.last(), Some(Event::Key(_)))
                && event::poll(input::PASTE_GAP)?
            {
                batch.push(event::read()?);
            }
            for ev in input::coalesce_paste(batch) {
                match ev {
                    Event::Key(key) => {
                        // Recorded before the action is resolved, so the
                        // probe sees what the terminal delivered even for a
                        // chord SE does not currently bind.
                        state.keys.observe(key);
                        let action = action_for_key(&state, key);
                        dirty |= dispatch_with(&mut state, core, &mut panel, action);
                    }
                    Event::Mouse(mouse) => {
                        let action = action_for_mouse(&state, mouse);
                        dirty |= dispatch_with(&mut state, core, &mut panel, action);
                    }
                    Event::Paste(text) => {
                        // Bracketed paste must be enabled by the binary for
                        // this arm to ever fire; see `se`'s `enter()`.
                        // Pasted text, including embedded newlines, is
                        // inserted verbatim rather than key-by-key, so a
                        // multi-line paste cannot submit on its first
                        // newline the way un-bracketed paste would.
                        if state.sessions.is_some() {
                            sessions::paste(&mut state, &text);
                        } else {
                            state.composer.insert_str(&text);
                        }
                        dirty = true;
                    }
                    Event::Resize(_, _) => dirty = true,
                    _ => {}
                }
                if state.switch_to.is_some() {
                    switch_session(&mut state, core);
                    dirty = true;
                }
            }
        }

        if state.mouse_capture != capture_applied {
            capture_applied = state.mouse_capture;
            let mut out = io::stdout();
            // Best-effort: a terminal that ignores it just keeps its old mode.
            let _ = if capture_applied {
                crossterm::execute!(out, crossterm::event::EnableMouseCapture)
            } else {
                crossterm::execute!(out, crossterm::event::DisableMouseCapture)
            };
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
#[cfg(test)]
fn dispatch(state: &mut AppState, core: &CoreHandle, action: Action) -> bool {
    dispatch_with(state, core, &mut team::Panel::default(), action)
}

/// `/team` is answered by the team panel, which owns the run; every
/// other command goes through `run_command`.
fn command(
    state: &mut AppState,
    core: &CoreHandle,
    panel: &mut team::Panel,
    path: &str,
    args: &str,
) {
    if state.commands.resolve(path).is_ok_and(|s| s.id == "team") {
        panel.command(state, args);
    } else {
        run_command(state, core, path, args);
    }
}

fn dispatch_with(
    state: &mut AppState,
    core: &CoreHandle,
    panel: &mut team::Panel,
    action: Action,
) -> bool {
    match &action {
        Action::Nothing => false,
        // /connect owns the composer: an address moves on to the key field; a key
        // goes to secure storage on a background thread, never submitted, shown or logged.
        Action::Submit(text) if state.connect.is_some() => {
            let step = state.connect.take().expect("checked");
            state.composer.clear();
            match step {
                state::ConnectStep::Key { id, name, url } => {
                    let key = text.trim();
                    if key.contains(char::is_whitespace) || key.starts_with('/') {
                        state.push(Entry::Notice(
                            "a key has no spaces and does not start with /: paste just the key (Esc cancels)"
                                .to_string(),
                        ));
                        state.connect = Some(state::ConnectStep::Key { id, name, url });
                    } else {
                        save_key(state, &id, &name, url, key)
                    }
                }
                state::ConnectStep::Address { id, name } => {
                    match state.providers.iter().find(|p| p.id == id).cloned() {
                        Some(p) => address_entered(state, p, text),
                        None => state.push(Entry::Notice(format!(
                            "{name} is no longer listed; run /connect again"
                        ))),
                    }
                }
            }
            true
        }
        Action::ClearComposer if state.connect.is_some() => {
            state.connect = None;
            state.composer.clear();
            state.push(Entry::Notice(
                "connect cancelled; nothing was saved".to_string(),
            ));
            true
        }
        // A pasted key never reaches the model, the transcript or a session file,
        // whether alone, inside a sentence, or as an argument to a command.
        Action::Submit(text) if se_core::looks_like_secret(text) => {
            state.picker.close();
            state.push(Entry::Notice(
                "That looks like an API key, so it was not sent or saved; edit it out, or \
                 to connect a provider use /connect <provider>."
                    .to_string(),
            ));
            true
        }
        Action::Submit(text)
            if state.connect_pending.is_some()
                && leading_command(text, &state.commands).is_none() =>
        {
            state.picker.close();
            state.push(Entry::Notice(
                "still loading the provider list for /connect; nothing was sent".to_string(),
            ));
            true
        }
        Action::Submit(text) => {
            apply_local(state, &action);
            // Charter §3.1: a registry-known slash path is never sent
            // to the model, whether it was accepted from the picker or
            // typed out in full and submitted directly -- the picker
            // only ever closes on whitespace, so a command with
            // arguments reaches here, not `Action::PickerAccept`.
            if let Some((path, raw_args)) = leading_command(text, &state.commands) {
                command(state, core, panel, &path, &raw_args);
                return true;
            }
            let trimmed = text.trim_start();
            // `//text` is the escape for a message that starts with `/`.
            let text = if trimmed.starts_with("//") {
                trimmed[1..].to_string()
            } else if trimmed.starts_with('/')
                && trimmed[1..]
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_whitespace())
            {
                let word = trimmed[1..].split_whitespace().next().unwrap_or("");
                state.push(Entry::Notice(unknown_command(&state.commands, word)));
                state.composer.set(text.clone());
                return true;
            } else {
                text.clone()
            };
            let text = &text;
            match core.submit_with_model(text, Vec::new(), state.model_override.clone()) {
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
        Action::CycleMode => {
            let all = se_protocol::WorkMode::ALL;
            let next =
                all[(all.iter().position(|m| *m == state.mode).unwrap_or(0) + 1) % all.len()];
            set_mode(state, core, next);
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
                let is_specialist = state
                    .commands
                    .resolve(&path)
                    .is_ok_and(|s| s.handler == se_command::HandlerKind::SpecialistInvoke);
                let is_model = state
                    .commands
                    .resolve(&path)
                    .is_ok_and(|s| s.handler == se_command::HandlerKind::ModelSelect);
                let is_provider = state
                    .commands
                    .resolve(&path)
                    .is_ok_and(|s| s.handler == se_command::HandlerKind::ProviderConnect);
                state.picker.close();
                if is_model {
                    state.composer.clear();
                    set_model(state, &path);
                } else if is_provider {
                    state.composer.clear();
                    begin_connect(state, &path);
                } else if is_specialist {
                    // A specialist needs a request after it (refused
                    // otherwise, see `run_specialist_invoke`), so
                    // accepting one from the picker prefills it ready
                    // to type rather than clearing-then-immediately-
                    // refusing an empty submission the user then has
                    // to retype from scratch.
                    state.composer.set(format!("/{path} "));
                } else {
                    state.composer.clear();
                    command(state, core, panel, &path, "");
                }
            } else {
                // Nothing matched; leave the draft alone so the user
                // can correct it rather than losing what they typed.
                state.picker.close();
            }
            true
        }
        Action::Decide(allow) => {
            if let (Some(call), Some(task)) = (state.awaiting_approval(), state.running_task()) {
                core.approve(ApprovalDecision {
                    task,
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
        for loaded in loaded {
            register_loaded(state, loaded);
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

/// Registers one loaded command file over the builtins. A collision is
/// itself worth surfacing: the registry already decides the winner by
/// precedence, but silence here would hide that two sources defined the
/// same path. Overriding an ordinary builtin works, with a notice;
/// `/help`, `/commands` and `/exit` are protected (security-critical).
fn register_loaded(state: &mut AppState, loaded: se_command::discovery::Loaded) {
    let se_command::discovery::Loaded { spec, def } = loaded;
    let id = spec.id.clone();
    let path = spec.path.clone();
    match state.commands.register(spec) {
        Ok(se_command::RegisterOutcome::Shadowed { winner_id }) => {
            state.push(Entry::Notice(format!(
                "a loaded command was shadowed by {winner_id} (higher precedence already owns that path)"
            )));
        }
        Ok(outcome) => {
            if let se_command::RegisterOutcome::Replaced { displaced_id } = &outcome {
                let builtin = se_command::builtins::builtins()
                    .iter()
                    .any(|b| b.id == displaced_id.as_str());
                if builtin && *displaced_id != id {
                    state.push(Entry::Notice(format!(
                        "/{path} is overridden by a custom command; the built-in {displaced_id} is hidden"
                    )));
                }
            }
            state.templates.insert(id, def);
        }
        Err(se_command::RegisterError::SecurityCriticalShadowed { path, builtin_id }) => {
            state.push(Entry::Notice(format!(
                "/{path} cannot be redefined -- it belongs to the built-in {builtin_id}"
            )));
        }
    }
}

/// If `text` names a registered command, returns its path and the
/// remaining text as raw arguments. Returns `None` for ordinary prose,
/// including prose that happens to contain a slash -- only a leading
/// `/<known-path>` counts.
fn leading_command(text: &str, commands: &se_command::Registry) -> Option<(String, String)> {
    let after_slash = text.trim_start().strip_prefix('/')?;
    // `//...` is a literal message, not a command (`resolve` would strip the second `/`).
    if after_slash.starts_with('/') {
        return None;
    }
    let (word, rest) = match after_slash.find(char::is_whitespace) {
        Some(i) => (&after_slash[..i], after_slash[i..].trim_start()),
        None => (after_slash, ""),
    };
    commands.resolve(word).ok()?;
    Some((word.to_string(), rest.to_string()))
}

/// "unknown command /foo", with the nearest known command when one is close.
fn unknown_command(commands: &se_command::Registry, word: &str) -> String {
    let w = word.to_lowercase();
    let best = commands
        .query("")
        .into_iter()
        .map(|c| (edit_distance(&w, &c.path.to_lowercase()), c.path.as_str()))
        .filter(|(d, p)| (*d <= 2 && *d < w.chars().count()) || (w.len() >= 2 && p.starts_with(&w)))
        .min_by_key(|(d, _)| *d);
    match best {
        Some((_, p)) => format!("unknown command /{word} \u{2014} did you mean /{p}?"),
        None => {
            format!("unknown command /{word} (start with // to send a message beginning with /)")
        }
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
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
        let notice = unknown_command(&state.commands, path);
        state.push(Entry::Notice(notice));
        return;
    };
    let id = spec.id.clone();
    if spec.handler == se_command::HandlerKind::PromptTemplate {
        run_prompt_template(state, core, &id, path, raw_args);
        return;
    }
    // Typed out in full (`/claude-code/sonnet`, `/nvidia`) rather than
    // picked from a list: the same as picking it.
    if spec.handler == se_command::HandlerKind::ModelSelect {
        let path = spec.path.clone();
        set_model(state, &path);
        return;
    }
    if spec.handler == se_command::HandlerKind::ProviderConnect {
        let path = spec.path.clone();
        begin_connect(state, &path);
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
            "agents" if raw_args.trim() == "none" => {
                core.set_specialist(None);
                state.agent = None;
                state.push(Entry::Notice(
                    "no specialist: the next message is routed afresh".to_string(),
                ));
            }
            "agents" => {
                // The single gateway into the 194-specialist catalogue
                // (OpenCode's `agent.list` -> dedicated dialog, ported
                // as a scope pivot of the same inline picker rather
                // than a second UI component). Reopening at a fresh
                // `/` means the live query starts empty, so every
                // specialist is a candidate until the user narrows it.
                state.composer.set("/".to_string());
                state
                    .picker
                    .open_scoped(0, crate::picker::PickerScope::Agents);
            }
            "models" if raw_args.trim().is_empty() => show_models(state),
            "models" if raw_args.trim() == "refresh" => refresh_models(state),
            "models" => set_model(state, raw_args),
            "providers" => {
                let report = state.providers_report.as_ref().map_or_else(
                    || "run `se providers` in a terminal for the provider list".to_string(),
                    |r| (r.0)(),
                );
                state.push(Entry::Notice(report));
            }
            "doctor" => {
                let report = state
                    .doctor_report
                    .clone()
                    .unwrap_or_else(|| "run `se doctor` in a terminal".to_string());
                state.push(Entry::Notice(report));
            }
            "usage" => {
                let text = format!(
                    "last task: {}\nthis session: {}",
                    usage_line(&state.usage),
                    usage_line(&state.session_usage)
                );
                state.push(Entry::Notice(text));
            }
            "connect" if raw_args.trim().is_empty() => open_connect(state, None),
            "connect" => open_connect(state, Some(raw_args.trim().to_string())),
            "sessions" => sessions::open(state),
            "new" => state.switch_to = Some(None),
            "mode" if raw_args.trim().is_empty() => state.push(Entry::Notice(format!(
                "mode: {} (Shift+Tab cycles; /mode manual|auto|plan)",
                state.mode.name()
            ))),
            "mode" => match se_protocol::WorkMode::parse(raw_args) {
                Some(m) => set_mode(state, core, m),
                None => state.push(Entry::Notice("usage: /mode manual|auto|plan".to_string())),
            },
            "copy" => copy_answer(state, raw_args),
            "mouse" => {
                state.mouse_capture = !state.mouse_capture;
                state.push(Entry::Notice(mouse_notice(state.mouse_capture).to_string()));
            }
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
                        let not_built = matches!(
                            se_command::builtins::availability(&c.id),
                            Some(se_command::builtins::Availability::NotImplemented(_))
                        );
                        let mark = if not_built { "  (not built yet)" } else { "" };
                        lines.push(format!("    /{:<12} {}{mark}", c.path, c.description));
                    }
                }
                lines.push(String::new());
                lines.push("keys:".to_string());
                lines.push(format!(
                    "    Enter send  \u{b7}  {} new line  \u{b7}  / commands  \u{b7}  Esc stop a reply",
                    state.keys.newline_hint()
                ));
                lines.push(
                    "    PgUp/PgDn scroll (wheel too while F2 mouse is on)  \u{b7}  Ctrl+U clear input  \u{b7}  Ctrl+D timings  \u{b7}  Ctrl+C twice exit"
                        .to_string(),
                );
                lines.push(
                    "    F2 mouse capture on/off (off: drag selects text)  \u{b7}  /copy [N] copies an answer"
                        .to_string(),
                );
                lines.push(
                    "    /resume (/sessions) picks an earlier conversation  \u{b7}  /new (/clear) starts a fresh one; the old one stays saved"
                        .to_string(),
                );
                lines.push(
                    "    in /resume: ↑↓ select  \u{b7}  Enter resume  \u{b7}  Tab this project / all recent projects  \u{b7}  r rename  \u{b7}  p pin  \u{b7}  K/J (or Shift+↑↓) move  \u{b7}  d delete (to .se/sessions/.trash)  \u{b7}  u undo delete  \u{b7}  Esc close"
                        .to_string(),
                );
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

fn set_mode(state: &mut AppState, core: &CoreHandle, mode: se_protocol::WorkMode) {
    state.mode = mode;
    core.set_mode(mode);
    state.push(Entry::Notice(
        match mode {
            se_protocol::WorkMode::Manual => "manual mode: SE asks before each tool call (reads included) and each agent it starts",
            se_protocol::WorkMode::Auto => "auto mode: every write, edit and shell command runs WITHOUT asking (there are no deny rules yet; shell is not limited to this folder); Shift+Tab to leave",
            se_protocol::WorkMode::Plan => "plan mode: read and search run without asking; write, edit and shell are refused",
        }
        .to_string(),
    ));
}

fn mouse_notice(on: bool) -> &'static str {
    if on {
        "mouse capture on: the wheel scrolls; F2 or /mouse again to select text with the mouse"
    } else {
        "mouse capture off: drag to select text; PgUp/PgDn scroll (F2 or /mouse for the wheel)"
    }
}

/// `/copy [N]`: the Nth-from-last answer's stored text onto the clipboard.
fn copy_answer(state: &mut AppState, raw_args: &str) {
    let arg = raw_args.trim();
    let n = if arg.is_empty() {
        Some(1)
    } else {
        arg.parse::<usize>().ok().filter(|n| *n >= 1)
    };
    let Some(n) = n else {
        state.push(Entry::Notice(format!(
            "/copy takes a number (1 = last answer), not {arg:?}"
        )));
        return;
    };
    let Some(text) = state
        .nth_last_answer(n)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
    else {
        state.push(Entry::Notice(format!(
            "nothing to copy: there is no answer #{n} from the end"
        )));
        return;
    };
    let notice = match state.copy.0.clone() {
        None => "copy unavailable here (no clipboard)".to_string(),
        Some(copy) => match copy(&text) {
            Ok(()) => format!("copied {} chars", text.chars().count()),
            Err(e) => format!("copy failed: {e}"),
        },
    };
    state.push(Entry::Notice(notice));
}

/// The working folder as the header shows it: its last two parts.
fn folder_label() -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return String::new();
    };
    let parts: Vec<String> = cwd
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(std::path::MAIN_SEPARATOR_STR)
}

/// `/connect` (a list of every provider with its real state) or
/// `/connect <id> [api-url]`. The list involves login checks, so it is built on a
/// background thread and arrives through the inbox.
fn open_connect(state: &mut AppState, id: Option<String>) {
    // Keys typed during a task would race its approval prompt for the composer.
    if state.is_busy() {
        state.push(Entry::Notice(
            "finish or interrupt the running task (Esc) before /connect".to_string(),
        ));
        return;
    }
    if let Some(arg) = &id {
        let word = arg.split_whitespace().next().unwrap_or("");
        if state.providers.iter().any(|p| p.id == word) {
            begin_connect(state, arg);
            return;
        }
    }
    let Some(lister) = state.list_providers.0.clone() else {
        state.push(Entry::Notice(
            "provider list unavailable here; run `se connect <provider>` in a terminal".to_string(),
        ));
        return;
    };
    state.connect_pending = id.or(Some(String::new()));
    state.push(Entry::Notice(
        "checking your providers and logins...".to_string(),
    ));
    let inbox = state.inbox.clone();
    std::thread::spawn(move || {
        // A panicking lister still answers (with nothing), so the pending lookup clears.
        let list =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lister())).unwrap_or_default();
        inbox.send(services::Inbound::Providers(list));
    });
}

/// One provider chosen: for an API provider, an address field when SE has no
/// address for it, then a hidden key field; for an agent CLI, its login state
/// (it keeps its own login). `arg` is `<id> [api-url]`.
fn begin_connect(state: &mut AppState, arg: &str) {
    let mut words = arg.split_whitespace();
    let id = words.next().unwrap_or("");
    let url = words.next().map(str::to_string);
    let Some(p) = state.providers.iter().find(|p| p.id == id).cloned() else {
        let shown = if se_core::looks_like_secret(id) || id.len() > 40 {
            "that".to_string()
        } else {
            format!("`{id}`")
        };
        state.push(Entry::Notice(format!(
            "{shown} is not a provider SE knows (see /connect)"
        )));
        return;
    };
    if !p.uses_key {
        let mark = if p.ready { "\u{2713}" } else { "\u{2717}" };
        state.push(Entry::Notice(format!("{mark} {}: {}", p.name, p.status)));
        if p.ready {
            state.push(Entry::Notice(format!(
                "nothing to paste: SE uses {}'s own login. Pick one of its models with /models",
                p.name
            )));
        }
        return;
    }
    match url {
        Some(u) => address_entered(state, p, &u),
        None if p.needs_url => open_address_field(state, p, None),
        None => open_key_field(state, p, None),
    }
}

/// The composer becomes a visible address field. Never leaves the user at the
/// chat box when SE is waiting for connection details (the pasted-key incident).
fn open_address_field(state: &mut AppState, p: services::ProviderEntry, why: Option<&str>) {
    state.push(Entry::Notice(format!(
        "{}{} needs its API address first (in their docs or dashboard, usually ending in /v1). \
         Paste it below and press Enter; the hidden key field comes next. Esc cancels.",
        why.map(|w| format!("{w} ")).unwrap_or_default(),
        p.name
    )));
    state.composer.clear();
    state.picker.close();
    state.connect = Some(state::ConnectStep::Address {
        id: p.id,
        name: p.name,
    });
}

/// Checks a typed or pasted address. A key pasted here is neither echoed nor kept.
fn address_entered(state: &mut AppState, p: services::ProviderEntry, text: &str) {
    let text = text.trim();
    if se_core::looks_like_secret(text) {
        open_address_field(
            state,
            p,
            Some("That looks like an API key, not an address, so it was not used or saved."),
        );
    } else if !(text.starts_with("https://") || text.starts_with("http://"))
        || text.contains(char::is_whitespace)
    {
        open_address_field(state, p, Some("An address starts with https:// ."));
    } else {
        // Whether it may receive a key (TLS, no userinfo) is decided by the saver.
        open_key_field(state, p, Some(text.to_string()));
    }
}

/// The composer becomes a hidden key field for `p`.
fn open_key_field(state: &mut AppState, p: services::ProviderEntry, url: Option<String>) {
    let note = if p.ready {
        "A key is already stored; a new one replaces it."
    } else {
        "No key stored yet."
    };
    state.push(Entry::Notice(format!(
        "{}: {note} Paste the API key below and press Enter. It is hidden, saved to secure storage, \
         checked against {}, and never shown, logged or sent to a model. Esc cancels.",
        p.name, p.name
    )));
    state.composer.clear();
    state.picker.close();
    state.connect = Some(state::ConnectStep::Key {
        id: p.id,
        name: p.name,
        url,
    });
}

fn save_key(state: &mut AppState, id: &str, name: &str, url: Option<String>, key: &str) {
    if key.is_empty() {
        state.push(Entry::Notice(
            "no key entered; nothing was saved".to_string(),
        ));
        return;
    }
    let Some(saver) = state.save_key.0.clone() else {
        state.push(Entry::Notice("saving keys is unavailable here".to_string()));
        return;
    };
    state.push(Entry::Notice(format!(
        "saving and checking the {name} key..."
    )));
    let (inbox, id, key) = (state.inbox.clone(), id.to_string(), key.to_string());
    std::thread::spawn(move || {
        let result = saver(&id, &key, url.as_deref());
        drop(key);
        match result {
            Ok(line) => {
                inbox.send(services::Inbound::KeySaved);
                inbox.send(services::Inbound::Notice(format!("\u{2713} {line}")));
            }
            Err(line) => inbox.send(services::Inbound::Notice(format!("\u{2717} {line}"))),
        }
    });
}

/// Applies what background threads produced since the last frame.
fn drain_inbox(state: &mut AppState) -> bool {
    let messages = state.inbox.take();
    let any = !messages.is_empty();
    for m in messages {
        match m {
            services::Inbound::Notice(text) => state.push(Entry::Notice(text)),
            services::Inbound::KeySaved => {
                state.model_listing.reset();
                state.providers.clear();
            }
            services::Inbound::Providers(list) => {
                for p in &list {
                    let mark = if p.ready { "\u{2713}" } else { "\u{b7}" };
                    let _ = state.commands.register(se_command::CommandSpec {
                        id: format!("provider:{}", p.id),
                        path: p.id.clone(),
                        aliases: Vec::new(),
                        category: if p.uses_key { "API key" } else { "own login" }.to_string(),
                        description: format!("{mark} {} \u{2014} {}", p.name, p.status),
                        provenance: se_command::Provenance::Builtin,
                        handler: se_command::HandlerKind::ProviderConnect,
                        security_critical: false,
                    });
                }
                state.providers = list;
                match state.connect_pending.take() {
                    Some(id) if !id.is_empty() && !state.composer.is_empty() => {
                        state.push(Entry::Notice(
                            "provider list ready: send or clear your draft, then run /connect again"
                                .to_string(),
                        ))
                    }
                    Some(id) if !id.is_empty() => begin_connect(state, &id),
                    Some(_) if state.composer.text().is_empty() => {
                        state.composer.set("/".to_string());
                        state
                            .picker
                            .open_scoped(0, crate::picker::PickerScope::Providers);
                    }
                    _ => {}
                }
            }
        }
    }
    any
}

fn usage_line(u: &se_protocol::Usage) -> String {
    let n = |v: Option<u64>| v.map_or_else(|| "not reported".to_string(), |t| t.to_string());
    format!(
        "{} in ({} cached), {} out",
        n(u.input_tokens),
        n(u.cached_input_tokens),
        n(u.output_tokens)
    )
}

/// Performs the switch `/resume` or `/new` asked for: opens the target with
/// a fresh core (exactly the `se --resume` path), stops a running turn on
/// the old one, then swaps cores and redraws. A failure to open leaves the
/// current conversation untouched.
pub fn switch_session(state: &mut AppState, core: &mut CoreHandle) {
    let Some(target) = state.switch_to.take() else {
        return;
    };
    let Some(open) = state.open_session.0.clone() else {
        state.push(Entry::Notice(
            "switching conversation is unavailable here: quit and run `se` or `se --resume=<id>`"
                .to_string(),
        ));
        return;
    };
    let opened = match open(target.as_deref()) {
        Ok(o) => o,
        Err(e) => {
            state.push(Entry::Notice(format!(
                "could not open the conversation: {e}"
            )));
            return;
        }
    };
    if let Some(task) = state.running_task() {
        core.interrupt(task);
        // The old turn ends cancelled (nothing saved); a reply that lands
        // first is saved to the old conversation, never the new one.
        pump_until_idle(state, core, Duration::from_secs(3));
    }
    let old = std::mem::replace(core, opened.core);
    let _ = old.drain(); // into the old event log
    old.shutdown();
    // A new core starts in manual; keep the mode the header shows.
    core.set_mode(state.mode);
    state.reset_conversation();
    state.sessions = None;
    state.session_id = Some(opened.id);
    state.agent = opened.specialist;
    for entry in opened.prior {
        state.push(entry);
    }
    if let Some(w) = opened.warning {
        state.push(Entry::Notice(w));
    }
}

/// Writes `<id>.model` beside the conversation, for the `/resume` list.
fn record_model(state: &AppState) {
    if let (Some(dir), Some(id), Some(model)) =
        (&state.sessions_dir, &state.session_id, &state.turn_model)
    {
        // shortcut: best effort; a lost write only blanks the picker's model column.
        let _ = std::fs::write(dir.join(format!("{id}.model")), model);
    }
}

/// Bare `/models`: lists what the providers report (off the interface
/// thread) and opens a picker of `provider/model` entries.
fn show_models(state: &mut AppState) {
    set_model(state, "");
    // Always ask again: providers add and drop models during a session, and the
    // on-disk verdict cache keeps a re-list to one `/models` call plus new probes.
    if state.model_listing.state() != services::ListingState::Loading {
        state.model_listing.reset();
    }
    match state.model_listing.state() {
        services::ListingState::Ready(list) => open_model_picker(state, list),
        services::ListingState::Loading => {
            state.open_models_when_ready = true;
            state.push(Entry::Notice("still listing models...".to_string()));
        }
        services::ListingState::NotLoaded => {
            if state.model_listing.start() {
                state.open_models_when_ready = true;
                state.push(Entry::Notice(
                    "listing models from your providers... the list opens when it is ready"
                        .to_string(),
                ));
            } else {
                state.push(Entry::Notice(
                    "model listing is not available here; set one with `/models <provider>/<model>`"
                        .to_string(),
                ));
            }
        }
    }
}

/// `/models refresh`: re-checks which models each key can use, then opens the picker.
fn refresh_models(state: &mut AppState) {
    if state.model_listing.refresh() {
        state.open_models_when_ready = true;
        state.push(Entry::Notice(
            "re-checking which models your keys can use... the list opens when it is ready"
                .to_string(),
        ));
    } else {
        state.push(Entry::Notice(
            "model listing is not available here".to_string(),
        ));
    }
}

/// Opens the model picker once a requested listing has arrived.
fn poll_models(state: &mut AppState) -> bool {
    if !state.open_models_when_ready {
        return false;
    }
    let services::ListingState::Ready(list) = state.model_listing.state() else {
        return false;
    };
    state.open_models_when_ready = false;
    open_model_picker(state, list);
    true
}

fn open_model_picker(state: &mut AppState, list: Vec<services::ProviderModels>) {
    // This listing replaces the last one, so a model the provider dropped stops being offered.
    state
        .commands
        .unregister_where(|c| c.handler == se_command::HandlerKind::ModelSelect);
    let mut count = 0;
    for pm in list {
        if let Some(note) = &pm.note {
            state.push(Entry::Notice(format!("{}: {note}", pm.provider)));
        }
        match pm.models {
            Ok(models) => {
                for model in models {
                    let path = format!("{}/{model}", pm.provider);
                    let description = if pm.unchecked.contains(&model) {
                        "unchecked".to_string()
                    } else {
                        String::new()
                    };
                    let _ = state.commands.register(se_command::CommandSpec {
                        id: format!("model:{path}"),
                        path,
                        aliases: Vec::new(),
                        category: pm.provider.clone(),
                        description,
                        provenance: se_command::Provenance::Builtin,
                        handler: se_command::HandlerKind::ModelSelect,
                        security_critical: false,
                    });
                    count += 1;
                }
            }
            Err(e) => state.push(Entry::Notice(format!(
                "{}: could not list models: {e}",
                pm.provider
            ))),
        }
    }
    if count == 0 {
        state.push(Entry::Notice("no provider reported a model".to_string()));
    } else if state.composer.text().is_empty() {
        // Never over a draft the user is typing; they can ask again.
        state.composer.set("/".to_string());
        state
            .picker
            .open_scoped(0, crate::picker::PickerScope::Models);
    } else {
        state.push(Entry::Notice(format!(
            "{count} models listed: type /models to pick one"
        )));
    }
}

/// `/models <provider>/<model>` selects the model for later turns,
/// `/models default` clears it, and bare `/models` shows the choice.
/// Nothing is validated here: listing or probing a provider is network
/// I/O and this runs on the interface thread (I1). An unknown or
/// unconfigured provider fails the next turn with the reason instead.
fn set_model(state: &mut AppState, raw_args: &str) {
    let arg = raw_args.trim();
    let message = if arg.is_empty() {
        match (&state.model_override, state.current_model()) {
            (Some(_), Some(m)) => {
                format!("model: {m} (`/models default` goes back to the startup model)")
            }
            (None, Some(m)) => format!("model: {m}"),
            (_, None) => "no model chosen yet".to_string(),
        }
    } else if arg == "default" {
        state.model_override = None;
        "model reset to the configured default".to_string()
    } else {
        match arg.split_once('/') {
            Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                let picked = se_protocol::ModelRef {
                    provider: provider.to_string(),
                    model: model.to_string(),
                };
                if let Some(remember) = &state.remember_model.0 {
                    remember(&picked);
                }
                state.model_override = Some(picked);
                format!("model set to {provider}/{model} (also used next time you start SE)")
            }
            _ => format!("expected `/models <provider>/<model>`, got `{arg}`"),
        }
    };
    state.push(Entry::Notice(message));
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
    if let Err(e) = core.submit_with_model(&rendered, Vec::new(), state.model_override.clone()) {
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
    if let Err(e) = core.submit_specialist_with_model(
        specialist_id,
        raw_args,
        Vec::new(),
        state.model_override.clone(),
    ) {
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
                // As if pressed once the prompt has been visible long enough.
                let later = Instant::now() + crate::state::APPROVAL_ARM_DELAY;
                let action = action_for_key_at(&state, key('y'), later);
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
        // `/skills` is honest-unavailable until skill discovery is
        // wired into the TUI (`/agents`, its sibling, is Ready now
        // that Phase 3 lands specialist dispatch).
        for c in "/skills".chars() {
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
            notice.contains("skill loader exists"),
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

    /// The structural point of the whole `/agents` gateway: a
    /// registered specialist must never appear in the default,
    /// everything-mixed-in picker list -- mirroring upstream
    /// OpenCode's split between its `/` autocomplete and its separate
    /// agent-select dialog, so `/` alone stays a short list of real
    /// commands rather than 194 specialist rows plus a handful of
    /// builtins.
    #[test]
    fn the_default_picker_scope_never_shows_a_registered_specialist() {
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        state
            .commands
            .register(specialist_spec("backend-lead"))
            .unwrap();
        for c in "/".chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }
        let candidates = state.picker_candidates();
        assert!(
            candidates.iter().all(|c| c.path != "backend-lead"),
            "a specialist must be invisible in the default picker scope: {:?}",
            candidates.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn running_slash_agents_pivots_the_picker_into_a_specialists_only_scope() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        state
            .commands
            .register(specialist_spec("backend-lead"))
            .unwrap();

        run_command(&mut state, &core, "agents", "");

        assert_eq!(state.picker.scope(), crate::picker::PickerScope::Agents);
        let candidates = state.picker_candidates();
        assert!(
            candidates.iter().any(|c| c.path == "backend-lead"),
            "the agents scope must show the registered specialist: {:?}",
            candidates.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
        assert!(
            candidates
                .iter()
                .all(|c| c.handler == se_command::HandlerKind::SpecialistInvoke),
            "the agents scope must never mix in an ordinary command: {:?}",
            candidates.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
        core.shutdown();
    }

    #[test]
    fn accepting_a_specialist_from_the_picker_prefills_rather_than_submits_empty() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        state
            .commands
            .register(specialist_spec("backend-lead"))
            .unwrap();
        run_command(&mut state, &core, "agents", "");
        for c in "backend-lead".chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }

        dispatch(&mut state, &core, Action::PickerAccept);

        assert_eq!(state.composer.text(), "/backend-lead ");
        assert!(
            !state
                .transcript
                .iter()
                .any(|e| matches!(e, Entry::Notice(n) if n.contains("needs a request"))),
            "accepting from the picker must prefill, not submit empty and then refuse"
        );
        core.shutdown();
    }

    fn wait_for_models(state: &mut AppState) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !poll_models(state) {
            assert!(Instant::now() < deadline, "the model list never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The picker shows only usable models, marks unchecked ones, says in
    /// one line what was hidden, and `/models refresh` asks for a re-check.
    #[test]
    fn models_picker_says_what_was_hidden_and_refresh_rechecks() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        let refreshes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = refreshes.clone();
        let lister: services::ModelLister = std::sync::Arc::new(move |refresh| {
            seen.lock().unwrap().push(refresh);
            vec![services::ProviderModels {
                provider: "apinex".into(),
                models: Ok(vec!["good".into(), "maybe".into()]),
                unchecked: vec!["maybe".into()],
                note: Some(
                    "3 hidden: 3 need a subscription or balance — /models refresh to re-check"
                        .into(),
                ),
            }]
        });
        state.model_listing = services::Listing::new(Some(lister));

        run_command(&mut state, &core, "models", "");
        wait_for_models(&mut state);
        let shown: Vec<(String, String)> = state
            .picker_candidates()
            .iter()
            .map(|c| (c.path.clone(), c.description.clone()))
            .collect();
        assert_eq!(
            shown,
            [
                ("apinex/good".to_string(), String::new()),
                ("apinex/maybe".to_string(), "unchecked".to_string())
            ]
        );
        assert!(state.transcript.iter().any(|e| matches!(e, Entry::Notice(n)
            if n == "apinex: 3 hidden: 3 need a subscription or balance — /models refresh to re-check")));

        state.composer.set(String::new());
        run_command(&mut state, &core, "models", "refresh");
        wait_for_models(&mut state);
        assert_eq!(*refreshes.lock().unwrap(), [false, true]);
    }

    /// Each `/models` asks the providers again, and a model one stopped
    /// offering is no longer listed or typeable.
    #[test]
    fn models_relists_every_time_and_drops_a_vanished_model() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = calls.clone();
        let lister: services::ModelLister = std::sync::Arc::new(move |_| {
            let first = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            let models = if first {
                vec!["old", "kept"]
            } else {
                vec!["kept", "new"]
            };
            vec![services::ProviderModels {
                provider: "nvidia".into(),
                models: Ok(models.into_iter().map(String::from).collect()),
                unchecked: Vec::new(),
                note: None,
            }]
        });
        state.model_listing = services::Listing::new(Some(lister));
        let paths = |state: &AppState| -> Vec<String> {
            state
                .picker_candidates()
                .iter()
                .map(|c| c.path.clone())
                .collect()
        };

        run_command(&mut state, &core, "models", "");
        wait_for_models(&mut state);
        assert_eq!(paths(&state), ["nvidia/kept", "nvidia/old"]);

        state.composer.set(String::new());
        run_command(&mut state, &core, "models", "");
        wait_for_models(&mut state);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(paths(&state), ["nvidia/kept", "nvidia/new"]);
        assert!(state.commands.resolve("nvidia/old").is_err());
    }

    /// `/models` lists what providers report off the interface thread,
    /// opens a picker of them, and picking one sets the session model.
    /// A provider that failed to list is reported, not hidden.
    #[test]
    fn models_opens_a_picker_of_reported_models_and_picking_one_sets_it() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        let lister: services::ModelLister = std::sync::Arc::new(|_| {
            vec![
                services::ProviderModels {
                    provider: "claude-code".into(),
                    models: Ok(vec!["sonnet".into(), "opus".into()]),
                    unchecked: Vec::new(),
                    note: None,
                },
                services::ProviderModels {
                    provider: "ollama".into(),
                    models: Err("connection refused".into()),
                    unchecked: Vec::new(),
                    note: None,
                },
            ]
        });
        state.model_listing = services::Listing::new(Some(lister));

        run_command(&mut state, &core, "models", "");
        wait_for_models(&mut state);
        assert_eq!(state.picker.scope(), crate::picker::PickerScope::Models);
        let shown: Vec<String> = state
            .picker_candidates()
            .iter()
            .map(|c| c.path.clone())
            .collect();
        assert_eq!(shown, ["claude-code/opus", "claude-code/sonnet"]);
        assert!(state
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::Notice(n) if n.contains("ollama: could not list models"))));

        for c in "opus".chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }
        dispatch(&mut state, &core, Action::PickerAccept);
        assert_eq!(
            state.model_override,
            Some(se_protocol::ModelRef {
                provider: "claude-code".into(),
                model: "opus".into()
            })
        );
        assert_eq!(
            state.composer.text(),
            "",
            "nothing is left to send to the model"
        );

        // Model entries never crowd the ordinary `/` command list.
        state.composer.set("/".to_string());
        state.picker.open(0);
        assert!(state
            .picker_candidates()
            .iter()
            .all(|c| c.handler != se_command::HandlerKind::ModelSelect));
        core.shutdown();
    }

    #[test]
    fn copy_takes_the_stored_text_of_the_nth_from_last_answer() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        let copied = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = copied.clone();
        state.copy = services::Hook(Some(std::sync::Arc::new(move |t: &str| {
            sink.lock().unwrap().push(t.to_string());
            Ok(())
        })));
        run_command(&mut state, &core, "copy", "");
        state.push(Entry::User("q1".into()));
        state.push(Entry::Assistant("first **answer**\nline two".into()));
        state.push(Entry::Notice("between".into()));
        state.push(Entry::Assistant("héllo 😀".into()));
        run_command(&mut state, &core, "copy", "");
        run_command(&mut state, &core, "copy", "2");
        run_command(&mut state, &core, "copy", "3");
        run_command(&mut state, &core, "copy", "0");
        assert_eq!(
            *copied.lock().unwrap(),
            ["héllo 😀", "first **answer**\nline two"],
            "the raw stored text, newest first, nothing for out of range"
        );
        let notices: Vec<String> = state
            .transcript
            .iter()
            .filter_map(|e| match e {
                Entry::Notice(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert!(notices[0].starts_with("nothing to copy"), "{notices:?}");
        assert!(
            notices.contains(&"copied 7 chars".to_string()),
            "{notices:?}"
        );
        assert!(
            notices.contains(&"copied 25 chars".to_string()),
            "{notices:?}"
        );
        assert!(
            notices.iter().any(|n| n.contains("no answer #3")),
            "{notices:?}"
        );
        assert!(
            notices.iter().any(|n| n.contains("/copy takes a number")),
            "{notices:?}"
        );
        core.shutdown();
    }

    #[test]
    fn shift_tab_cycles_the_work_mode_and_slash_mode_sets_it() {
        use se_protocol::WorkMode;
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        assert_eq!(state.mode, WorkMode::Manual, "asks first by default");
        let tab = action_for_key(&state, key_code(crossterm::event::KeyCode::BackTab));
        assert_eq!(tab, Action::CycleMode);
        for expected in [WorkMode::Auto, WorkMode::Plan, WorkMode::Manual] {
            dispatch(&mut state, &core, tab.clone());
            assert_eq!(state.mode, expected);
        }
        run_command(&mut state, &core, "mode", "plan");
        assert_eq!(state.mode, WorkMode::Plan);
        run_command(&mut state, &core, "mode", "sideways");
        assert_eq!(
            state.mode,
            WorkMode::Plan,
            "an unknown mode changes nothing"
        );
        core.shutdown();
    }

    #[test]
    fn mouse_capture_starts_off_and_f2_and_slash_mouse_toggle_it() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        assert!(
            !state.mouse_capture,
            "off by default so the terminal can select text"
        );
        let f2 = action_for_key(&state, key_code(crossterm::event::KeyCode::F(2)));
        assert_eq!(f2, Action::ToggleMouse);
        dispatch(&mut state, &core, f2.clone());
        assert!(state.mouse_capture);
        dispatch(&mut state, &core, f2);
        assert!(!state.mouse_capture);
        run_command(&mut state, &core, "mouse", "");
        assert!(state.mouse_capture);
        // Keyboard scrolling does not depend on capture.
        assert_eq!(
            action_for_key(&state, key_code(crossterm::event::KeyCode::PageUp)),
            Action::ScrollUp(10)
        );
        run_command(&mut state, &core, "mouse", "");
        assert!(!state.mouse_capture);
        core.shutdown();
    }

    /// The commands that used to answer "panel is not built" now answer.
    #[test]
    fn ready_commands_answer_instead_of_saying_their_panel_is_missing() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        state.providers_report = Some(services::Report(std::sync::Arc::new(|| "PROVIDERS".into())));
        state.doctor_report = Some("DOCTOR".into());
        for cmd in [
            "providers",
            "doctor",
            "usage",
            "connect",
            "sessions",
            "help",
            "keys",
        ] {
            run_command(&mut state, &core, cmd, "");
        }
        let notices: Vec<&String> = state
            .transcript
            .iter()
            .filter_map(|e| match e {
                Entry::Notice(n) => Some(n),
                _ => None,
            })
            .collect();
        assert!(
            notices.iter().all(|n| !n.contains("panel is not built")),
            "{notices:?}"
        );
        assert!(notices.iter().any(|n| n.as_str() == "PROVIDERS"));
        assert!(notices.iter().any(|n| n.as_str() == "DOCTOR"));
        assert!(notices.iter().any(|n| n.contains("this session:")));
        // With no provider lister (a test), `/connect` says where to go
        // instead; with one, it opens the provider list (tested below).
        assert!(notices.iter().any(|n| n.contains("se connect")));
        core.shutdown();
    }

    /// Every builtin marked ready is handled by name: none can fall into
    /// a "not built" reply again.
    #[test]
    fn no_ready_builtin_reaches_the_not_built_fallback() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        let ready: Vec<String> = se_command::builtins::registry_with_builtins()
            .query("")
            .into_iter()
            .filter(|c| {
                se_command::builtins::availability(&c.id)
                    == Some(se_command::builtins::Availability::Ready)
            })
            .map(|c| c.path.clone())
            .collect();
        for path in ready
            .iter()
            .filter(|p| !["exit", "team"].contains(&p.as_str()))
        {
            run_command(&mut state, &core, path, "");
        }
        assert!(!state
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::Notice(n) if n.contains("panel is not built"))));
        core.shutdown();
    }

    fn drain_until(state: &mut AppState, done: impl Fn(&AppState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(state) {
            assert!(Instant::now() < deadline, "background work never arrived");
            drain_inbox(state);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn connect_state(saved: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) -> AppState {
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        let lister: services::ProviderLister = std::sync::Arc::new(|| {
            vec![
                services::ProviderEntry {
                    id: "openai".into(),
                    name: "OpenAI".into(),
                    uses_key: true,
                    needs_url: false,
                    ready: false,
                    status: "no key yet".into(),
                },
                services::ProviderEntry {
                    id: "codex".into(),
                    name: "Codex".into(),
                    uses_key: false,
                    needs_url: false,
                    ready: true,
                    status: "logged in with your ChatGPT plan".into(),
                },
            ]
        });
        let saver: services::KeySaver =
            std::sync::Arc::new(move |id: &str, key: &str, _url: Option<&str>| {
                saved
                    .lock()
                    .unwrap()
                    .push((id.to_string(), key.to_string()));
                Ok(format!("{id}: key saved"))
            });
        state.list_providers = services::Hook(Some(lister));
        state.save_key = services::Hook(Some(saver));
        state
    }

    /// The incident: `/connect apinex` with no address left the user at the chat box,
    /// and the key they pasted went to the model. Now: an address field, then the
    /// hidden key field; a key pasted into the address field is neither used nor shown.
    #[test]
    fn connect_asks_for_the_address_then_the_key_and_never_leaks_either() {
        let core = se_core::start(EchoRunner::instant());
        let got = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        state.providers = vec![services::ProviderEntry {
            id: "apinex".into(),
            name: "Apinex".into(),
            uses_key: true,
            needs_url: true,
            ready: false,
            status: "needs its API address".into(),
        }];
        let sink = got.clone();
        state.save_key = services::Hook(Some(std::sync::Arc::new(
            move |id: &str, key: &str, url: Option<&str>| {
                *sink.lock().unwrap() =
                    Some((id.to_string(), key.to_string(), url.map(str::to_string)));
                Ok("saved".to_string())
            },
        )));
        let key = "sk-abcdefghij0123456789ABCDEFGHIJ0123456789abcdefghij0";
        let submit = |state: &mut AppState, text: &str| {
            dispatch(state, &core, Action::Submit(text.to_string()));
        };

        submit(&mut state, "/connect apinex");
        assert!(matches!(
            state.connect,
            Some(state::ConnectStep::Address { .. })
        ));

        submit(&mut state, key); // pasted into the wrong field
        assert!(matches!(
            state.connect,
            Some(state::ConnectStep::Address { .. })
        ));

        submit(&mut state, "https://api.example.com/v1");
        assert!(matches!(
            state.connect,
            Some(state::ConnectStep::Key { .. })
        ));

        submit(&mut state, key);
        drain_until(&mut state, |_| got.lock().unwrap().is_some());
        assert_eq!(
            *got.lock().unwrap(),
            Some((
                "apinex".to_string(),
                key.to_string(),
                Some("https://api.example.com/v1".to_string())
            ))
        );
        assert!(state.connect.is_none() && state.composer.is_empty());
        assert!(
            !state.transcript.iter().any(|e| match e {
                Entry::User(t) | Entry::Notice(t) | Entry::Assistant(t) => t.contains(key),
                _ => false,
            }),
            "the key never enters the transcript"
        );
        assert!(!state.transcript.iter().any(|e| matches!(e, Entry::User(_))));
        core.shutdown();
    }

    /// A key pasted into the normal chat box, alone, in a sentence, or as a
    /// `/connect` argument, never reaches the core, the model or the transcript.
    #[test]
    fn a_key_pasted_into_the_chat_box_is_blocked_before_the_core() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        discover_and_register_commands(&mut state);
        let key = "sk-abcdefghij0123456789ABCDEFGHIJ0123456789abcdefghij0";
        for text in [
            key.to_string(),
            format!("please connect {key}"),
            format!("/connect openai {key}"),
        ] {
            for c in text.chars() {
                crate::input::apply_local(&mut state, &Action::InsertChar(c));
            }
            dispatch(&mut state, &core, Action::Submit(text.clone()));
            assert_eq!(state.composer.text(), text, "kept so the user can edit it");
            crate::input::apply_local(&mut state, &Action::ClearComposer);
        }
        std::thread::sleep(Duration::from_millis(50));
        drain_inbox(&mut state);
        for e in core.drain() {
            state.apply(&e);
        }
        assert!(
            !state.transcript.iter().any(|e| matches!(e, Entry::User(_))),
            "nothing sent"
        );
        assert!(
            !state.transcript.iter().any(|e| match e {
                Entry::User(t) | Entry::Notice(t) | Entry::Assistant(t) => t.contains(key),
                _ => false,
            }),
            "the key is never shown"
        );
        assert!(state
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::Notice(n) if n.contains("looks like an API key"))));
        core.shutdown();
    }

    /// Review items 5-7: the key field refuses non-keys and stays open; chat is held
    /// while a /connect lookup is pending; /connect is refused during a task.
    #[test]
    fn connect_refuses_non_keys_holds_chat_while_pending_and_waits_for_tasks() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = connect_state(std::sync::Arc::default());

        state.connect = Some(state::ConnectStep::Key {
            id: "openai".into(),
            name: "OpenAI".into(),
            url: None,
        });
        dispatch(&mut state, &core, Action::Submit("/models".to_string()));
        dispatch(&mut state, &core, Action::Submit("two words".to_string()));
        assert!(
            matches!(state.connect, Some(state::ConnectStep::Key { .. })),
            "still open"
        );

        state.connect = None;
        state.connect_pending = Some("openai".to_string());
        dispatch(&mut state, &core, Action::Submit("hello model".to_string()));
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            core.drain().is_empty(),
            "nothing reached the core while pending"
        );

        state.connect_pending = None;
        state.activity = Activity::Working {
            task: se_protocol::TaskId::from_raw(1),
        };
        open_connect(&mut state, Some("openai".to_string()));
        assert!(state.connect.is_none() && state.connect_pending.is_none());
        core.shutdown();
    }

    /// A failing provider lister still clears the pending lookup, so chat is never held for good.
    #[test]
    fn a_failing_provider_lister_clears_the_pending_connect_and_releases_chat() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = connect_state(std::sync::Arc::default());
        let lister: services::ProviderLister = std::sync::Arc::new(|| panic!("lister failed"));
        state.list_providers = services::Hook(Some(lister));
        run_command(&mut state, &core, "connect", "");
        assert!(state.connect_pending.is_some());
        drain_until(&mut state, |s| s.connect_pending.is_none());
        dispatch(&mut state, &core, Action::Submit("hello after".to_string()));
        assert!(pump_until_idle(&mut state, &core, Duration::from_secs(5)));
        assert!(state
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::User(t) if t == "hello after")));
        core.shutdown();
    }

    fn notices(state: &AppState) -> Vec<String> {
        state
            .transcript
            .iter()
            .filter_map(|e| match e {
                Entry::Notice(n) => Some(n.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_unknown_slash_command_is_not_sent_and_suggests_the_nearest() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        dispatch(&mut state, &core, Action::Submit("/hepl me".to_string()));
        dispatch(&mut state, &core, Action::Submit("/zzzzqqqq".to_string()));
        std::thread::sleep(Duration::from_millis(50));
        assert!(core.drain().is_empty(), "nothing reached the core");
        let n = notices(&state);
        assert!(
            n.iter()
                .any(|t| t == "unknown command /hepl \u{2014} did you mean /help?"),
            "{n:?}"
        );
        assert!(
            n.iter()
                .any(|t| t.starts_with("unknown command /zzzzqqqq") && !t.contains("did you mean")),
            "{n:?}"
        );
        assert_eq!(
            state.composer.text(),
            "/zzzzqqqq",
            "the draft is kept to fix"
        );

        // A known command still runs.
        dispatch(&mut state, &core, Action::Submit("/exit".to_string()));
        assert!(state.should_quit);
        core.shutdown();
    }

    #[test]
    fn a_double_slash_sends_the_literal_slash_message() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = AppState::new();
        dispatch(
            &mut state,
            &core,
            Action::Submit("//help is a word".to_string()),
        );
        assert!(pump_until_idle(&mut state, &core, Duration::from_secs(5)));
        assert!(state
            .transcript
            .iter()
            .any(|e| matches!(e, Entry::User(t) if t == "/help is a word")));
        assert!(!notices(&state)
            .iter()
            .any(|t| t.contains("unknown command")));
        core.shutdown();
    }

    #[test]
    fn a_project_cannot_override_help_but_may_override_another_builtin_with_a_notice() {
        let dir = TempCommandDir::new();
        dir.write("help.md", "hijacked help");
        dir.write("sessions.md", "my sessions");
        let (loaded, _) = se_command::discovery::scan(&dir.0, se_command::Provenance::Project);
        let mut state = AppState::new();
        for l in loaded {
            register_loaded(&mut state, l);
        }
        assert_eq!(
            state.commands.resolve("help").unwrap().id,
            "help",
            "help is protected"
        );
        assert_eq!(
            state.commands.resolve("sessions").unwrap().handler,
            se_command::HandlerKind::PromptTemplate,
            "the override takes effect"
        );
        let n = notices(&state);
        assert!(
            n.iter().any(|t| t.contains("/help cannot be redefined")),
            "{n:?}"
        );
        assert_eq!(
            n.iter()
                .filter(|t| t.contains("built-in sessions is hidden"))
                .count(),
            1,
            "{n:?}"
        );
    }

    /// The request: a place inside SE to paste a provider's API key. The
    /// key goes to the saver and nowhere else: not the transcript, not
    /// the model, not the screen.
    #[test]
    fn connect_takes_a_hidden_key_and_hands_it_only_to_the_saver() {
        let core = se_core::start(EchoRunner::instant());
        let saved = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut state = connect_state(saved.clone());
        run_command(&mut state, &core, "connect", "");
        drain_until(&mut state, |s| {
            s.picker.scope() == crate::picker::PickerScope::Providers
        });
        let listed: Vec<String> = state
            .picker_candidates()
            .iter()
            .map(|c| c.path.clone())
            .collect();
        assert_eq!(listed, ["codex", "openai"]);

        for c in "openai".chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }
        dispatch(&mut state, &core, Action::PickerAccept);
        assert!(state.connect.is_some(), "a key field is open");

        let key = "sk-test/SECRET-1234";
        for c in key.chars() {
            let a = action_for_key(
                &state,
                crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char(c)),
            );
            dispatch(&mut state, &core, a);
        }
        assert!(
            !state.picker.is_open(),
            "a `/` inside a key never opens the picker"
        );
        let screen = {
            let backend = ratatui::backend::TestBackend::new(100, 20);
            let mut t = ratatui::Terminal::new(backend).unwrap();
            t.draw(|f| crate::render::draw(f, &mut state)).unwrap();
            format!("{:?}", t.backend().buffer())
        };
        assert!(!screen.contains("SECRET"), "the key is never drawn");

        let enter = action_for_key(
            &state,
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter),
        );
        dispatch(&mut state, &core, enter);
        drain_until(&mut state, |_| !saved.lock().unwrap().is_empty());
        assert_eq!(
            saved.lock().unwrap()[0],
            ("openai".to_string(), key.to_string())
        );
        assert!(state.composer.is_empty() && state.connect.is_none());
        drain_until(&mut state, |s| {
            s.transcript
                .iter()
                .any(|e| matches!(e, Entry::Notice(n) if n.contains("key saved")))
        });
        assert!(
            !state.transcript.iter().any(|e| match e {
                Entry::User(t) | Entry::Notice(t) | Entry::Assistant(t) => t.contains("SECRET"),
                _ => false,
            }),
            "the key never enters the transcript"
        );
        assert!(
            !state.transcript.iter().any(|e| matches!(e, Entry::User(_))),
            "nothing was sent to the model"
        );
        core.shutdown();
    }

    /// The complaint: SE told a logged-in user to log in. A logged-in CLI
    /// shows its state and asks for nothing.
    #[test]
    fn connect_to_a_logged_in_cli_shows_its_login_and_asks_for_nothing() {
        let core = se_core::start(EchoRunner::instant());
        let mut state = connect_state(std::sync::Arc::default());
        run_command(&mut state, &core, "connect", "codex");
        drain_until(&mut state, |s| !s.providers.is_empty());
        assert!(state.connect.is_none(), "no key field for a CLI login");
        let text: Vec<&String> = state
            .transcript
            .iter()
            .filter_map(|e| match e {
                Entry::Notice(n) => Some(n),
                _ => None,
            })
            .collect();
        assert!(
            text.iter()
                .any(|n| n.contains("logged in with your ChatGPT plan")),
            "{text:?}"
        );
        assert!(!text.iter().any(|n| n.contains("codex login")), "{text:?}");
        core.shutdown();
    }

    #[test]
    fn esc_abandons_a_key_entry_and_wipes_it() {
        let core = se_core::start(EchoRunner::instant());
        let saved = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut state = connect_state(saved.clone());
        run_command(&mut state, &core, "connect", "openai");
        drain_until(&mut state, |s| s.connect.is_some());
        for c in "half-typed".chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }
        let esc = action_for_key(
            &state,
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Esc),
        );
        dispatch(&mut state, &core, esc);
        assert!(state.connect.is_none() && state.composer.is_empty());
        std::thread::sleep(Duration::from_millis(50));
        assert!(saved.lock().unwrap().is_empty(), "nothing saved");
        core.shutdown();
    }

    struct ModelRecordingRunner {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Option<se_protocol::ModelRef>>>>,
    }

    impl se_core::Runner for ModelRecordingRunner {
        fn describe(&self) -> (String, String) {
            ("test".to_string(), "model-recording".to_string())
        }
        fn run(
            &self,
            turn: &se_protocol::UserTurn,
            _history: &[se_provider::Message],
            _sink: &mut se_core::TurnSink,
        ) -> se_protocol::Outcome {
            self.seen.lock().unwrap().push(turn.model.clone());
            se_protocol::Outcome::Completed {
                final_text: "ok".into(),
            }
        }
    }

    #[test]
    fn slash_models_selects_the_model_every_later_turn_carries() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let core = se_core::start(ModelRecordingRunner { seen: seen.clone() });
        let mut state = AppState::new();
        state
            .commands
            .register(specialist_spec("backend-lead"))
            .unwrap();
        let wait = |core: &CoreHandle| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !core
                .drain_blocking(Duration::from_millis(10))
                .iter()
                .any(|e| e.is_terminal())
            {
                assert!(std::time::Instant::now() < deadline, "turn never finished");
            }
        };

        run_command(&mut state, &core, "models", "not-a-ref");
        assert_eq!(state.model_override, None, "a malformed ref sets nothing");
        run_command(&mut state, &core, "models", "beta/beta-large");
        run_command(&mut state, &core, "backend-lead", "fix it");
        wait(&core);
        run_command(&mut state, &core, "models", "default");
        run_command(&mut state, &core, "backend-lead", "again");
        wait(&core);
        core.shutdown();

        let beta = se_protocol::ModelRef {
            provider: "beta".into(),
            model: "beta-large".into(),
        };
        assert_eq!(seen.lock().unwrap().as_slice(), [Some(beta), None]);
    }

    /// Records every `set_specialist` the core forwards.
    struct SpecialistRecorder {
        set: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    impl se_core::Runner for SpecialistRecorder {
        fn describe(&self) -> (String, String) {
            ("test".to_string(), "recording".to_string())
        }
        fn run(
            &self,
            _turn: &se_protocol::UserTurn,
            _history: &[se_provider::Message],
            _sink: &mut se_core::TurnSink,
        ) -> se_protocol::Outcome {
            se_protocol::Outcome::Completed {
                final_text: "ok".into(),
            }
        }
        fn set_specialist(&self, id: Option<String>) {
            self.set.lock().unwrap().push(id);
        }
    }

    #[test]
    fn typing_slash_agents_none_clears_the_specialist() {
        let set = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let core = se_core::start(SpecialistRecorder { set: set.clone() });
        let mut state = AppState::new();
        state.agent = Some("backend-lead".into());
        let text = "/agents none";
        for c in text.chars() {
            crate::input::apply_local(&mut state, &Action::InsertChar(c));
        }
        dispatch(&mut state, &core, Action::Submit(text.to_string()));
        assert_eq!(state.agent, None, "the header drops the specialist");
        assert!(!core.is_busy(), "a command is never a turn");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while set.lock().unwrap().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "never reached the runner"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        core.shutdown();
        assert_eq!(set.lock().unwrap().as_slice(), [None]);
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

    // --- /resume and /new ---

    fn press(state: &mut AppState, core: &CoreHandle, code: crossterm::event::KeyCode) {
        let ev = crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        let action = action_for_key(state, ev);
        dispatch(state, core, action);
    }

    fn save_convo(dir: &std::path::Path, id: &str, ask: &str, answer: &str) {
        use se_provider::{Message, Role};
        std::fs::create_dir_all(dir).unwrap();
        let json = serde_json_free(&[
            Message::text(Role::User, ask),
            Message::text(Role::Assistant, answer),
        ]);
        std::fs::write(dir.join(format!("{id}.conversation.json")), json).unwrap();
    }

    /// Written by hand: se-tui has no serde_json dependency.
    fn serde_json_free(m: &[se_provider::Message]) -> String {
        let parts: Vec<String> = m
            .iter()
            .map(|m| format!(r#"{{"role":"{:?}","content":"{}"}}"#, m.role, m.content))
            .collect();
        format!("[{}]", parts.join(","))
    }

    /// The binary's opener, minus providers: the conversation file loaded
    /// into a fresh core and redrawn, as `se --resume` does.
    fn opener(dir: std::path::PathBuf) -> services::SessionOpener {
        let next = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(1000));
        std::sync::Arc::new(move |id: Option<&str>| {
            let id = id.map_or_else(
                || {
                    next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                        .to_string()
                },
                str::to_string,
            );
            let path = dir.join(format!("{id}.conversation.json"));
            let prior = se_core::load_conversation(&path)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|m| match m.role {
                    se_provider::Role::User => Entry::User(m.content),
                    _ => Entry::Assistant(m.content),
                })
                .collect();
            let core = se_core::start_conversation(EchoRunner::instant(), &path)
                .map_err(|e| e.to_string())?;
            let specialist = std::fs::read_to_string(dir.join(format!("{id}.specialist"))).ok();
            Ok(services::Opened {
                core,
                id,
                prior,
                specialist,
                warning: None,
            })
        })
    }

    fn sessions_state(dir: &std::path::Path, open: &str) -> AppState {
        let mut state = AppState::new();
        state.sessions_dir = Some(dir.to_path_buf());
        state.session_id = Some(open.to_string());
        state.open_session = services::Hook(Some(opener(dir.to_path_buf())));
        state
    }

    #[test]
    fn resume_from_the_picker_stops_the_running_turn_and_loads_the_conversation() {
        let tmp = TempCommandDir::new();
        let dir = tmp.0.join(".se").join("sessions");
        save_convo(&dir, "100", "old question", "old answer");
        std::fs::write(dir.join("100.specialist"), "backend-lead").unwrap();
        let mut state = sessions_state(&dir, "200");
        let mut core = se_core::start(EchoRunner::slow(Duration::from_millis(30), 200));
        dispatch(
            &mut state,
            &core,
            Action::Submit("a long running turn".to_string()),
        );
        let started = Instant::now();
        while state.running_task().is_none() && started.elapsed() < Duration::from_secs(5) {
            pump(&mut state, &core);
        }
        assert!(state.running_task().is_some());

        run_command(&mut state, &core, "resume", "");
        assert!(state.sessions.is_some(), "/resume opens the picker");
        assert_eq!(
            state.sessions.as_ref().unwrap().items[0].row.title,
            "old question"
        );
        press(&mut state, &core, crossterm::event::KeyCode::Enter);
        assert_eq!(state.switch_to, Some(Some("100".to_string())));
        switch_session(&mut state, &mut core);

        assert!(state.sessions.is_none());
        assert!(!state.is_busy(), "the old turn was stopped");
        assert_eq!(state.session_id.as_deref(), Some("100"));
        assert_eq!(state.agent.as_deref(), Some("backend-lead"));
        assert!(
            matches!(&state.transcript[..], [Entry::User(q), Entry::Assistant(a)] if q == "old question" && a == "old answer")
        );
        // The new core continues that file.
        dispatch(&mut state, &core, Action::Submit("follow up".to_string()));
        assert!(pump_until_idle(&mut state, &core, Duration::from_secs(5)));
        assert_eq!(
            se_core::load_conversation(&dir.join("100.conversation.json"))
                .unwrap()
                .len(),
            4
        );
        core.shutdown();
    }

    #[test]
    fn new_and_clear_start_a_fresh_conversation_and_keep_the_old_one() {
        let tmp = TempCommandDir::new();
        let dir = tmp.0.join(".se").join("sessions");
        save_convo(&dir, "100", "q", "a");
        let mut state = sessions_state(&dir, "100");
        let mut core =
            se_core::start_conversation(EchoRunner::instant(), &dir.join("100.conversation.json"))
                .unwrap();
        state.push(Entry::User("q".into()));
        state.push(Entry::Assistant("a".into()));
        for cmd in ["new", "clear"] {
            let before = state.session_id.clone();
            run_command(&mut state, &core, cmd, "");
            switch_session(&mut state, &mut core);
            assert!(
                state.transcript.is_empty(),
                "/{cmd}: {:?}",
                state.transcript
            );
            assert_ne!(state.session_id, before, "/{cmd} gets a new id");
        }
        assert!(dir.join("100.conversation.json").exists());
        core.shutdown();
    }

    #[test]
    fn the_picker_refuses_to_delete_the_open_conversation_and_undo_brings_one_back() {
        let tmp = TempCommandDir::new();
        let dir = tmp.0.join(".se").join("sessions");
        save_convo(&dir, "100", "open one", "a");
        save_convo(&dir, "101", "other one", "a");
        let core = se_core::start(EchoRunner::instant());
        let mut state = sessions_state(&dir, "100");
        crate::sessions::open(&mut state);
        let at = |s: &AppState, id: &str| {
            s.sessions
                .as_ref()
                .unwrap()
                .items
                .iter()
                .position(|i| i.row.id == id)
                .unwrap()
        };
        state.sessions.as_mut().unwrap().selected = at(&state, "100");
        press(&mut state, &core, crossterm::event::KeyCode::Char('d'));
        assert!(state
            .sessions
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .contains("open conversation"));
        assert!(dir.join("100.conversation.json").exists());

        state.sessions.as_mut().unwrap().selected = at(&state, "101");
        press(&mut state, &core, crossterm::event::KeyCode::Char('d'));
        press(&mut state, &core, crossterm::event::KeyCode::Char('y'));
        assert!(!dir.join("101.conversation.json").exists());
        assert!(dir
            .join(".trash")
            .join("101")
            .join("101.conversation.json")
            .exists());
        press(&mut state, &core, crossterm::event::KeyCode::Char('u'));
        assert!(dir.join("101.conversation.json").exists());
        assert_eq!(state.sessions.as_ref().unwrap().items.len(), 2);
        core.shutdown();
    }

    #[test]
    fn rename_pin_and_move_keys_persist_in_the_index() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let tmp = TempCommandDir::new();
        let dir = tmp.0.join(".se").join("sessions");
        save_convo(&dir, "1", "first", "a");
        save_convo(&dir, "2", "second", "a");
        let core = se_core::start(EchoRunner::instant());
        let mut state = sessions_state(&dir, "9");
        crate::sessions::open(&mut state);
        let ids = |s: &AppState| {
            s.sessions
                .as_ref()
                .unwrap()
                .items
                .iter()
                .map(|i| i.row.id.clone())
                .collect::<Vec<_>>()
        };
        let start = ids(&state);
        // Shift+Down moves the top row down; K moves it back.
        let shifted = crossterm::event::KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT);
        let action = action_for_key(&state, shifted);
        dispatch(&mut state, &core, action);
        assert_eq!(ids(&state), [start[1].clone(), start[0].clone()]);
        press(&mut state, &core, KeyCode::Char('K'));
        assert_eq!(ids(&state), start);
        // r, type, Enter renames; pasted text goes into the title too.
        press(&mut state, &core, KeyCode::Char('r'));
        press(&mut state, &core, KeyCode::Char('u'));
        for _ in 0..20 {
            press(&mut state, &core, KeyCode::Backspace);
        }
        crate::sessions::paste(&mut state, "Renamed");
        press(&mut state, &core, KeyCode::Char('!'));
        press(&mut state, &core, KeyCode::Enter);
        let top = start[0].clone();
        assert_eq!(
            se_core::sessions::list(&dir)
                .0
                .iter()
                .find(|r| r.id == top)
                .unwrap()
                .title,
            "Renamed!"
        );
        // p pins the second row, which then comes first.
        press(&mut state, &core, KeyCode::Down);
        press(&mut state, &core, KeyCode::Char('p'));
        assert_eq!(ids(&state)[0], start[1]);
        assert!(std::fs::read_to_string(dir.join("index.json"))
            .unwrap()
            .contains("pinned"));
        // Esc closes, and the composer never saw any of those keys.
        press(&mut state, &core, KeyCode::Esc);
        assert!(state.sessions.is_none());
        assert!(state.composer.is_empty());
        core.shutdown();
    }

    #[test]
    fn a_row_from_another_project_says_where_to_start_se_instead_of_switching() {
        let tmp = TempCommandDir::new();
        let here = tmp.0.join("here").join(".se").join("sessions");
        let other = tmp.0.join("other");
        save_convo(&here, "1", "local", "a");
        save_convo(&other.join(".se").join("sessions"), "7", "elsewhere", "a");
        let recent = tmp.0.join("home").join("recent-projects.json");
        se_core::sessions::touch_recent(&recent, &other, 5).unwrap();
        se_core::sessions::touch_recent(&recent, &tmp.0.join("here"), 6).unwrap();
        let core = se_core::start(EchoRunner::instant());
        let mut state = sessions_state(&here, "1");
        state.recent_projects = Some(recent);
        crate::sessions::open(&mut state);
        assert_eq!(
            state.sessions.as_ref().unwrap().items.len(),
            1,
            "this project only"
        );
        press(&mut state, &core, crossterm::event::KeyCode::Tab);
        let ov = state.sessions.as_ref().unwrap();
        assert_eq!(ov.items.len(), 2, "this project's row is not listed twice");
        let i = ov.items.iter().position(|i| i.row.id == "7").unwrap();
        state.sessions.as_mut().unwrap().selected = i;
        press(&mut state, &core, crossterm::event::KeyCode::Enter);
        assert_eq!(state.switch_to, None);
        let msg = state.sessions.as_ref().unwrap().message.clone().unwrap();
        assert!(
            msg.contains("start SE in") && msg.contains("other") && msg.contains("--resume=7"),
            "{msg}"
        );
        core.shutdown();
    }
}
