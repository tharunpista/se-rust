//! The `se` binary: wiring and terminal lifecycle.
//!
//! Three responsibilities, deliberately no more: parse the command line,
//! own raw mode and the alternate screen, and connect a core to an
//! interface. All behaviour lives in the libraries, so this file stays
//! short enough to audit by eye -- the previous implementation's binary
//! crate grew to 15,000 lines precisely because it kept acquiring
//! "just one more" responsibility.

mod clipboard;
mod team_cmd;

use std::io::{self, IsTerminal, Stdout, Write};
use std::time::Duration;

use clap::{Parser, Subcommand};
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use se_core::{EchoRunner, ProviderRunner};
use se_protocol::{timings::format_duration, Outcome, Payload, Stage};
use se_provider::{credentials, Registry};

#[derive(Parser, Clone)]
#[command(name = "se", version, about = "SE — a standalone engineering agent")]
struct Cli {
    /// Provider to use, e.g. `nvidia`. With exactly one provider
    /// configured SE uses it; with several it asks rather than guessing.
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Model id, exactly as the provider reports it.
    #[arg(long, global = true)]
    model: Option<String>,

    /// Continue a saved conversation: `--resume` for the latest in this
    /// directory, `--resume=<id>` for a specific one. The model and
    /// provider may differ from the ones it was started with.
    #[arg(
        long,
        global = true,
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "latest"
    )]
    resume: Option<String>,

    /// How tool calls are handled: `manual` asks before each change (default),
    /// `auto` runs them without asking, `plan` only reads.
    #[arg(long, global = true, value_parser = parse_mode, default_value = "manual")]
    mode: se_protocol::WorkMode,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Clone)]
enum Command {
    /// Run a team of specialists, each on any vendor's model, from a
    /// YAML team file (see `se_core::team::file` for the format).
    Team {
        #[command(subcommand)]
        command: TeamCommand,
    },
    /// Run one request without the interface, streaming to stdout.
    Exec {
        /// What to ask for.
        request: String,
    },
    /// Store a provider credential in OS-native secure storage, then
    /// confirm it works by listing the provider's models.
    Connect {
        /// Provider id, e.g. `nvidia`. Omit to list what is available.
        provider: Option<String>,
        /// The provider's OpenAI-compatible API address, e.g.
        /// `https://api.example.com/v1`. Needed only when SE has none built in.
        url: Option<String>,
        /// Read the key from this environment variable rather than an
        /// argument, so it never lands in shell history.
        #[arg(long)]
        from_env: Option<String>,
    },
    /// Remove a stored credential.
    Disconnect { provider: String },
    /// List providers and their real state.
    Providers,
    /// List the models a provider actually reports.
    Models {
        provider: Option<String>,
        /// Re-check which models the key can use instead of trusting the cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Report what this build can and cannot do, honestly.
    Doctor,
    /// List every slash command the interface offers.
    ///
    /// Projected from the same `se-command` registry the TUI picker and
    /// `/help` read, so the two surfaces cannot disagree about what
    /// exists (charter §12).
    Commands,
    /// List or inspect SE specialist agents from the real catalogue
    /// (`~/SE/se/agents`), never a synthesized or cached description.
    Agents {
        /// A specific agent id to show in full (frontmatter + body).
        /// Omit to list every loaded id with its one-line mission.
        id: Option<String>,
    },
}

fn main() -> std::process::ExitCode {
    let startup = se_protocol::Running::start(Stage::Startup);
    let cli = Cli::parse();

    match &cli.command {
        Some(Command::Commands) => {
            commands();
            std::process::ExitCode::SUCCESS
        }
        Some(Command::Exec { request }) => exec(&cli, request),
        Some(Command::Team { command }) => match command {
            TeamCommand::Run { file } => team_cmd::run_file(&cli, file),
            TeamCommand::Resume {
                journal,
                max_concurrent,
            } => team_cmd::resume(&cli, journal, *max_concurrent),
        },
        Some(Command::Connect { provider, url, from_env }) => {
            connect(provider.as_deref(), url.as_deref(), from_env.as_deref())
        }
        Some(Command::Disconnect { provider }) => disconnect(provider),
        Some(Command::Providers) => providers(),
        Some(Command::Models { provider, refresh }) => models(&cli, provider.as_deref(), *refresh),
        Some(Command::Doctor) => doctor(startup),
        Some(Command::Agents { id }) => agents(id.as_deref()),
        None => {
            // A pipe is not a terminal: entering raw mode would fail or
            // corrupt the stream, so the headless path is named instead
            // of pretending.
            if !io::stdout().is_terminal() {
                eprintln!("se: stdout is not a terminal; use `se exec \"...\"` for headless use");
                return std::process::ExitCode::from(2);
            }
            match interactive(&cli, startup) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("se: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}

/// Decides which provider and model to use.
///
/// Never guesses between several: with more than one configured and no
/// explicit choice it reports the options and stops. Silently picking
/// one would leave the user unable to tell what their money was spent
/// on, which the product direction rules out.
fn resolve(cli: &Cli) -> Result<(String, String), String> {
    let registry = Registry::new();
    // Nothing named: the model last picked with `/models`, if its
    // provider is still usable.
    if cli.provider.is_none() && cli.model.is_none() {
        if let Some((p, m)) = load_last_model() {
            if registry.get(&p).is_some_and(|x| x.detect().configured) {
                return Ok((p, m));
            }
        }
    }
    let provider_id = match &cli.provider {
        Some(id) => {
            let provider = registry
                .get(id)
                .ok_or_else(|| format!("{} is not a known provider — try `se providers`", shown_id(id)))?;
            let status = provider.detect();
            if !status.configured {
                return Err(format!("`{id}` is not configured: {}", status.detail));
            }
            id.clone()
        }
        None => {
            let configured = registry.configured();
            match configured.len() {
                0 => {
                    return Err(
                        "no provider is configured — run `se connect nvidia --from-env <VAR>`"
                            .to_string(),
                    )
                }
                1 => configured[0].id().to_string(),
                _ => {
                    let ids: Vec<&str> = configured.iter().map(|p| p.id()).collect();
                    return Err(format!(
                        "several providers are configured ({}) — choose one with --provider and --model, or pick one in `se` with /models (it is remembered)",
                        ids.join(", ")
                    ));
                }
            }
        }
    };

    let Some(model) = &cli.model else {
        return Err(format!(
            "no model selected — run `se models {provider_id}` and pass --model <id>"
        ));
    };
    Ok((provider_id, model.clone()))
}

fn interactive(cli: &Cli, startup: se_protocol::Running) -> io::Result<()> {
    // Resolved before the screen is taken over, so a configuration
    // problem is readable rather than flashing past inside an alternate
    // screen that is immediately torn down.
    let resolved = resolve(cli);
    let convo = match conversation(cli.resume.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("se: {e}");
            return Ok(());
        }
    };

    let mut terminal = enter()?;
    // Restoration must survive a panic, or a crash leaves the user with
    // no echo and no cursor.
    // A panic on a task's worker thread is reported by the engine as a
    // failed task while the interface keeps running, so only a panic on
    // this (the interface) thread may tear the screen down. Every panic is
    // kept in `.se/crash.log`, because whatever reaches stderr while the
    // alternate screen is up is lost.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        append_crash_log(info);
        if std::thread::current().name() == Some("main") {
            let _ = restore();
            default_hook(info);
        }
    }));

    // Whether this session will actually reach a model, decided once
    // and shown everywhere a user could be looking: the header never
    // calls an echoed response an AI-generated one, and this banner is
    // the on-screen counterpart of the same fact.
    let catalogue_root = catalogue_root();

    let (mut core, mut banner) = match open_core(&resolved, catalogue_root.clone(), &convo) {
        Ok(opened) => opened,
        Err(e) => {
            let _ = restore();
            eprintln!("se: {e}");
            return Ok(());
        }
    };
    let cwd = std::env::current_dir().ok();
    if let (Some(file), Some(cwd)) = (recent_projects_path(), &cwd) {
        // Best effort: the list only feeds /resume's all-projects view.
        let now = (unix_ms() / 1000) as u64;
        let _ = se_core::sessions::touch_recent(&file, cwd, now);
    }

    // A specialist is browsable/invokable from `/` only if the real
    // catalogue actually loaded -- a missing or unreadable catalogue
    // degrades to an empty specialist list plus a banner note, the
    // same "fold into the banner, don't refuse to start" convention
    // the session log above already follows, never a hard error.
    let mut specialists = Vec::new();
    if let Some(root) = &catalogue_root {
        match se_catalogue::load(root) {
            Ok(report) => {
                specialists = report
                    .agents
                    .iter()
                    .map(|a| se_command::CommandSpec {
                        id: format!("specialist:{}", a.frontmatter.id),
                        path: a.frontmatter.id.clone(),
                        aliases: Vec::new(),
                        category: a.frontmatter.department.clone(),
                        description: a.frontmatter.mission.trim().to_string(),
                        provenance: se_command::Provenance::Builtin,
                        handler: se_command::HandlerKind::SpecialistInvoke,
                        security_critical: false,
                    })
                    .collect();
            }
            Err(e) => {
                banner = Some(match banner {
                    Some(existing) => format!("{existing} — also: specialists not loaded ({e})"),
                    None => format!("⚠ specialists not loaded ({e})"),
                });
            }
        }
    }

    let first_frame = startup.finish();
    // `/team run <file>` builds its runner the same way `se team run`
    // does, at the moment it is asked for.
    let team_cli = cli.clone();
    let launcher: se_tui::team::Launcher = Box::new(move |path, mode| team_cmd::start(&team_cli, path, mode));
    // `/resume` and `/new`: the same conversation + core building as
    // startup and `se --resume=<id>`.
    let open_resolved = resolved.clone();
    let open_root = catalogue_root.clone();
    let opener: se_tui::SessionOpener = std::sync::Arc::new(move |id: Option<&str>| {
        // `latest` is `--resume`'s keyword, never a picked id.
        let convo = conversation(id.filter(|i| *i != "latest"))?;
        let (core, warning) = open_core(&open_resolved, open_root.clone(), &convo)?;
        Ok(se_tui::Opened {
            core,
            prior: prior_entries(&convo, open_resolved.is_err()),
            specialist: std::fs::read_to_string(convo.specialist_path())
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            id: convo.id,
            warning,
        })
    });
    let services = se_tui::Services {
        team: Some(launcher),
        list_models: Some(std::sync::Arc::new(list_all_models)),
        providers: Some(se_tui::Report(std::sync::Arc::new(providers_report))),
        doctor: Some(doctor_report(first_frame.duration)),
        remember_model: Some(std::sync::Arc::new(save_last_model)),
        list_providers: Some(std::sync::Arc::new(list_providers)),
        save_key: Some(std::sync::Arc::new(|id: &str, key: &str, url: Option<&str>| {
            store_and_validate(id, key, url)
        })),
        copy: Some(std::sync::Arc::new(clipboard::copy)),
        default_model: resolved.as_ref().ok().map(|(p, m)| se_protocol::ModelRef {
            provider: p.clone(),
            model: m.clone(),
        }),
        open_session: Some(opener),
        session_id: Some(convo.id.clone()),
        sessions_dir: cwd.as_deref().map(se_core::session_store::default_sessions_dir),
        recent_projects: recent_projects_path(),
        mode: cli.mode,
    };
    let prior = prior_entries(&convo, resolved.is_err());
    let result = se_tui::run(&mut terminal, &mut core, banner, specialists, services, prior);
    core.shutdown();
    restore()?;

    // Printed after the screen is back, so it is still visible.
    if let Err(reason) = &resolved {
        eprintln!("se: {reason}");
        eprintln!("se: ran the echo runner, so nothing was sent to a model");
    }
    eprintln!("se: startup {}", format_duration(first_frame.duration));
    result
}

/// The entries a conversation opens with: its saved turns, redrawn exactly
/// as saved (a compaction summary shows as the summary it is), then its note.
fn prior_entries(convo: &Convo, unresolved: bool) -> Vec<se_tui::Entry> {
    se_core::load_conversation(&convo.path)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| match m.role {
            se_provider::Role::User => Some(se_tui::Entry::User(m.content)),
            se_provider::Role::Assistant => Some(se_tui::Entry::Assistant(m.content)),
            _ => None,
        })
        .chain(std::iter::once(se_tui::Entry::Notice(convo.note())))
        .chain(unresolved.then(|| {
            se_tui::Entry::Notice(
                "no model chosen yet: pick one with /models (remembered for next time), or add a key with /connect"
                    .to_string(),
            )
        }))
        .collect()
}

/// `~/.se/recent-projects.json`, beside `last-model`.
fn recent_projects_path() -> Option<std::path::PathBuf> {
    last_model_path().map(|p| p.with_file_name("recent-projects.json"))
}

/// Builds the core for `convo` (runner, saved specialist, event log), the
/// one path startup, `--resume`, `/resume` and `/new` share. The second
/// value is a warning for the banner.
fn open_core(
    resolved: &Result<(String, String), String>,
    catalogue_root: Option<std::path::PathBuf>,
    convo: &Convo,
) -> Result<(se_core::CoreHandle, Option<String>), String> {
    // Several providers and no model named or remembered: still a real
    // model session, with the model picked in-app (`/models`), never the
    // echo. The echo is only for a machine with no provider at all.
    let unselected = resolved.is_err() && !Registry::new().configured().is_empty();
    let runner = match &resolved {
        Ok((provider_id, model)) => Some(ProviderRunner::new(provider_id, model)),
        Err(_) if unselected => Some(ProviderRunner::unselected()),
        Err(_) => None,
    };
    let (core, banner) = match (runner, &resolved) {
        (Some(runner), _) => match runner {
            Ok(mut runner) => {
                if let Some(root) = catalogue_root.clone() {
                    // A turn that names no specialist gets one picked
                    // from the catalogue automatically.
                    runner = runner.with_catalogue_root(root).with_auto_route(true);
                }
                let (runner, audit_warning) = with_audit_log(
                    runner
                        .with_compaction(compact_at())
                        .with_specialist_file(convo.specialist_path()),
                );
                match se_core::start_conversation(runner, &convo.path) {
                    // The banner is for warnings only; the model is in the
                    // header and the conversation note opens the transcript.
                    Ok(core) => (core, audit_warning),
                    Err(e) => return Err(format!("cannot open {}: {e}", convo.path.display())),
                }
            }
            Err(e) => return Err(e.to_string()),
        },
        // No usable provider: the interface still runs, against a local
        // echo, so the user sees the application and reads why rather
        // than being dropped back to a bare shell -- but it must never
        // be mistaken for a real reply, which is what this banner is for.
        (None, Err(reason)) => (
            se_core::start(EchoRunner::instant()),
            Some(format!(
                "⚠ no model connected ({reason}) — replies below are a local echo, not AI"
            )),
        ),
        (None, Ok(_)) => unreachable!("a resolved model always builds a runner"),
    };

    // Best-effort: a session that cannot be logged still runs, exactly
    // as it always did -- persistence is an added guarantee, not a new
    // way for SE to refuse to start. A failure here is folded into the
    // banner the user already sees, rather than silently dropped.
    let mut banner = banner;
    if let Ok(cwd) = std::env::current_dir() {
        // One event log per core: task ids restart in every core, so two
        // must never share a log file. Named `<id>.<ms>.` so it travels
        // with its conversation (`/resume` delete and undo).
        let path = se_core::session_store::default_sessions_dir(&cwd)
            .join(format!("{}.{}.events.jsonl", convo.id, unix_ms()));
        if let Err(e) = core.attach_session_log(&path) {
            banner = Some(match banner {
                Some(existing) => format!("{existing} — also: session not logged ({e})"),
                None => format!("⚠ session not logged ({e})"),
            });
        }
    }
    Ok((core, banner))
}

fn enter() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste matters for correctness, not comfort: without it
    // a pasted multi-line prompt arrives as individual Enter presses,
    // so the first newline submits a half-pasted request. The interface
    // already handles `Event::Paste`; until now that arm was
    // unreachable because the mode was never enabled.
    // Mouse capture starts off so the terminal's own text selection works;
    // F2 or /mouse turns it on (se-tui), and restore() always turns it off.
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore() -> io::Result<()> {
    let mut stdout = io::stdout();
    // Each step is attempted even if an earlier one failed: leaving raw
    // mode enabled is far worse than a redundant escape sequence.
    let _ = execute!(
        stdout,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    let _ = disable_raw_mode();
    Ok(())
}

#[derive(Subcommand, Clone)]
enum TeamCommand {
    /// Start a run from a team file.
    Run { file: std::path::PathBuf },
    /// Continue a run from its journal after SE stopped.
    Resume {
        journal: std::path::PathBuf,
        /// How many tasks may run at once. Default: what the run was
        /// started with.
        #[arg(long)]
        max_concurrent: Option<usize>,
    },
}

/// Asks a question on the terminal and returns the trimmed, lowercased
/// answer. `None` when no terminal is attached or reading fails: a
/// question nobody can answer is never answered on their behalf.
fn ask_terminal(question: &str) -> Option<String> {
    if !io::stdin().is_terminal() {
        return None;
    }
    eprint!("{question}");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok()?;
    Some(line.trim().to_ascii_lowercase())
}

/// A tool approval asked on the terminal. Only an explicit yes allows;
/// no terminal, an empty line or anything else denies.
fn ask_approval(prefix: &str, name: &str, summary: &str) -> bool {
    match ask_terminal(&format!("{prefix}allow {name}: {summary}? [y/N] ")) {
        Some(a) => a == "y" || a == "yes",
        None => {
            eprintln!("{prefix}denied {name}: no terminal to ask for approval");
            false
        }
    }
}

fn parse_mode(s: &str) -> Result<se_protocol::WorkMode, String> {
    se_protocol::WorkMode::parse(s).ok_or_else(|| format!("`{s}` is not a mode: use manual, auto or plan"))
}

fn exec(cli: &Cli, request: &str) -> std::process::ExitCode {
    let (provider_id, model) = match resolve(cli) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("se: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let runner = match ProviderRunner::new(&provider_id, &model) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("se: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let runner = match catalogue_root() {
        Some(root) => runner.with_catalogue_root(root).with_auto_route(true),
        None => runner,
    };
    let (runner, audit_warning) = with_audit_log(runner.with_compaction(compact_at()));
    if let Some(warning) = audit_warning {
        eprintln!("se: {warning}");
    }
    let convo = match conversation(cli.resume.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("se: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let runner = runner.with_specialist_file(convo.specialist_path());
    let core = match se_core::start_conversation(runner, &convo.path) {
        Ok(core) => core,
        Err(e) => {
            eprintln!("se: cannot open {}: {e}", convo.path.display());
            return std::process::ExitCode::from(2);
        }
    };
    eprintln!("se: {}", convo.note());
    core.set_mode(cli.mode);
    if let Err(e) = core.submit(request, Vec::new()) {
        eprintln!("se: {e}");
        return std::process::ExitCode::FAILURE;
    }

    loop {
        for event in core.drain_blocking(Duration::from_millis(100)) {
            match event.payload {
                Payload::SpecialistInvoked { agent_id, .. } => {
                    eprintln!("se: specialist {agent_id}");
                }
                // Reasoning goes to stderr: stdout carries only the answer,
                // so `se exec` output stays pipeable.
                Payload::Delta {
                    kind: se_protocol::DeltaKind::Reasoning,
                    text,
                } => eprint!("{text}"),
                Payload::Delta { text, .. } => {
                    print!("{text}");
                    let _ = io::stdout().flush();
                }
                // Without this, a call needing approval waited forever:
                // nothing in a headless run ever answered it.
                Payload::ApprovalRequired {
                    call,
                    name,
                    summary,
                } => {
                    let allow = ask_approval("se: ", &name, &summary);
                    core.approve(se_protocol::ApprovalDecision {
                        task: event.task,
                        call,
                        allow,
                    });
                }
                Payload::UsageUpdated(usage) => {
                    // stderr, so stdout stays the answer alone and
                    // remains pipeable.
                    eprintln!(
                        "\nse: tokens in {:?} out {:?} ({})",
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.source.label()
                    );
                }
                Payload::TaskFinished { outcome, timings } => {
                    println!();
                    core.shutdown();
                    eprintln!(
                        "se: {} · ttft {} · local {}",
                        outcome.label(),
                        timings
                            .of(Stage::TimeToFirstToken)
                            .map(format_duration)
                            .unwrap_or_else(|| "n/a".into()),
                        format_duration(timings.local()),
                    );
                    match &outcome {
                        Outcome::Failed { detail } => eprintln!("se: {detail}"),
                        Outcome::Incomplete { reason, .. } => eprintln!("se: {reason}"),
                        _ => {}
                    }
                    return match outcome {
                        Outcome::Completed { .. } => std::process::ExitCode::SUCCESS,
                        Outcome::Cancelled => std::process::ExitCode::from(130),
                        // A turn that ran out of steps did not succeed,
                        // and must not exit zero as though it had.
                        Outcome::ExhaustedTurns { .. } => std::process::ExitCode::from(3),
                        // A partial answer is not success, and is given
                        // its own code rather than being folded into
                        // Failed's: the caller can tell "something
                        // streamed" from "nothing did".
                        Outcome::Incomplete { .. } => std::process::ExitCode::from(4),
                        Outcome::Failed { .. } => std::process::ExitCode::FAILURE,
                    };
                }
                _ => {}
            }
        }
    }
}

fn connect(
    provider: Option<&str>,
    url: Option<&str>,
    from_env: Option<&str>,
) -> std::process::ExitCode {
    let registry = Registry::new();
    let Some(id) = provider else {
        println!("Pass a provider id. Available:");
        for (id, display, status) in registry.statuses() {
            println!("  {id:<12} {display}");
            println!("               {}", status.detail);
        }
        return std::process::ExitCode::from(2);
    };
    let Some(provider) = registry.get(id) else {
        eprintln!("se: {} is not a known provider — run `se connect` to list them", shown_id(id));
        return std::process::ExitCode::from(2);
    };
    // An agent CLI keeps its own login: there is nothing to paste.
    if provider.is_agent_cli() {
        return match provider.login_status() {
            Some(Ok(state)) => {
                println!("{id}: {state} — nothing to store; pick a model with `se models {id}`");
                std::process::ExitCode::SUCCESS
            }
            Some(Err(how)) => {
                eprintln!("se: {id}: {how}");
                std::process::ExitCode::FAILURE
            }
            None => std::process::ExitCode::SUCCESS,
        };
    }

    // Checked before the key is asked for, so nobody pastes a key only to have it refused.
    if let Err(why) = check_url(id, url, provider) {
        eprintln!("se: {why}");
        return std::process::ExitCode::from(2);
    }

    // Never an argument: a key on a command line lands in shell history
    // and the process list. Either a named variable, or a hidden prompt.
    let secret = match from_env {
        Some(var) => match std::env::var(var) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("se: {var} is not set");
                return std::process::ExitCode::from(2);
            }
        },
        None => match read_secret(&format!(
            "API key for {} (hidden; paste it and press Enter, Esc cancels): ",
            provider.display_name()
        )) {
            Some(k) => k,
            None => {
                eprintln!("se: cancelled; nothing was saved");
                return std::process::ExitCode::from(2);
            }
        },
    };
    eprint!("saving and checking… ");
    let _ = io::stderr().flush();
    match store_and_validate(id, &secret, url) {
        Ok(line) => {
            eprintln!();
            println!("{line}");
            std::process::ExitCode::SUCCESS
        }
        Err(line) => {
            eprintln!();
            eprintln!("se: {line}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The address a key will go to: given and safe to send a key to, or already known.
fn check_url(
    id: &str,
    url: Option<&str>,
    provider: &dyn se_provider::ModelProvider,
) -> Result<(), String> {
    match url {
        // Never echoed: it is probably a key, and it is already in shell history.
        Some(u) if credentials::looks_like_secret(u) => Err(
            "that looks like an API key, not an address, so it was not used or saved; \
             give the address here and paste the key at the hidden prompt (revoke that key if it was real)"
                .to_string(),
        ),
        Some(u) if se_provider::openai_compatible::is_safe_origin(u.trim()) => Ok(()),
        Some(_) => Err(
            "that address cannot receive a key: use https://… (http only for localhost), \
             with no user:password in it; nothing was saved"
                .to_string(),
        ),
        None if provider.current_origin().is_some() => Ok(()),
        None => Err(format!(
            "{id} needs its API address first: `se connect {id} https://<api-address>/v1` \
             (in the app: /connect {id} https://<api-address>/v1); nothing was saved"
        )),
    }
}

/// Stores an API key in OS secure storage, pinned to the provider's
/// current endpoint (E2), then checks it against the provider. Shared by
/// `se connect` and the interface's `/connect`. Never echoes the key: the
/// result names only its length.
fn store_and_validate(id: &str, secret: &str, url: Option<&str>) -> Result<String, String> {
    let registry = Registry::new();
    let provider = registry
        .get(id)
        .ok_or_else(|| format!("{} is not a known provider", shown_id(id)))?;
    if provider.is_agent_cli() {
        return Err(format!("{id} uses its own login; there is no key to store"));
    }
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("the key was empty; nothing was saved".to_string());
    }
    check_url(id, url, provider)?;
    let service = *se_provider::CATALOGUE
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("{id} does not take an API key"))?;
    let url = url.map(se_provider::openai_compatible::api_root);
    if let Some(u) = &url {
        // Either override would send this key somewhere other than the address given,
        // or check a different key against it.
        let env_url = std::env::var(service.base_url_var).unwrap_or_default();
        if !env_url.is_empty() && credentials::origin_of(&env_url) != credentials::origin_of(u) {
            return Err(format!(
                "{} is set in your environment and points somewhere else; unset it first. Nothing was saved.",
                service.base_url_var
            ));
        }
        if credentials::source(id, service.key_var) == se_provider::Source::Environment {
            return Err(format!(
                "{} is set in your environment and would be used instead of this key; unset it first. Nothing was saved.",
                service.key_var
            ));
        }
    }
    let base = url
        .clone()
        .or_else(|| se_provider::OpenAiCompatible::new(service).base_url())
        .ok_or_else(|| {
            format!(
                "{id} has no endpoint yet, so the key was not saved: {}",
                provider.detect().detail
            )
        })?;
    // The origin the key is pinned to (E2); nothing to pin means nothing to save.
    let origin = credentials::origin_of(&base)
        .filter(|_| se_provider::openai_compatible::is_safe_origin(&base))
        .ok_or_else(|| format!("{id}'s address cannot receive a key; nothing was saved"))?;
    // Checked before anything is written, so a rejected key never replaces a working one.
    let candidate = se_provider::OpenAiCompatible::with_fixed(service, secret, &base);
    let models = se_provider::ModelProvider::validate(&candidate)
        .map_err(|e| {
            // Only an API's own 401/403 means the key was refused; anything else means the key
            // was never checked, and saying "refused" sent the user hunting for a key problem.
            let why = match &e {
                se_provider::ProviderError::Auth(_) => format!("{id} refused that key"),
                se_provider::ProviderError::BadRequest(_) => format!(
                    "the key was not checked: there is no model list at {base}/models, so the \
                     address is probably wrong (it is usually the API root, ending in /v1)"
                ),
                se_provider::ProviderError::Protocol(_) => format!(
                    "the key was not checked: {base} answered with something that is not an API \
                     (a web page or a firewall block?), so check the address"
                ),
                _ => format!("the key was not checked: {id} could not be reached"),
            };
            // A provider's error body may quote the key back; never show it unredacted.
            let detail = credentials::redact_secrets(&e.to_string());
            format!("{why} ({detail}). Nothing was saved; any key stored before is unchanged.")
        })?;
    credentials::store(id, secret, Some(&origin))
        .map_err(|e| format!("could not save the key for {id}: {e}"))?;
    // Written only once the key is stored, so a failed save never leaves a new address
    // paired with the old key.
    if let Some(u) = &url {
        credentials::store_base_url(id, u).map_err(|e| {
            format!("the key was saved but the address was not ({e}); run /connect {id} <address> again")
        })?;
    }
    // A key in the environment wins over a stored one; say so rather than mislead.
    let shadowed = if credentials::source(id, service.key_var) == se_provider::Source::Environment {
        format!(
            " Note: {} is set in your environment and is used instead of the saved key.",
            service.key_var
        )
    } else {
        String::new()
    };
    Ok(format!(
        "{id}: key saved to secure storage ({}), pinned to {origin}, and accepted: {} model(s) available. Pick one with /models.{shadowed}",
        credentials::fingerprint(secret),
        models.len()
    ))
}

/// A provider id fit to echo: a pasted key or a long blob is never repeated back.
fn shown_id(id: &str) -> String {
    if credentials::looks_like_secret(id) || id.len() > 40 {
        "that".to_string()
    } else {
        format!("`{id}`")
    }
}

/// Reads a secret without echoing it. From a terminal: raw mode, so the
/// key never appears on screen (paste works; Esc or Ctrl+C cancels).
/// Piped: one line from stdin, so `... | se connect x` works too.
fn read_secret(prompt: &str) -> Option<String> {
    use crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
    eprint!("{prompt}");
    let _ = io::stderr().flush();
    if !io::stdin().is_terminal() {
        let mut line = String::new();
        io::stdin().read_line(&mut line).ok()?;
        return Some(line.trim().to_string());
    }
    enable_raw_mode().ok()?;
    let mut key = String::new();
    let result = loop {
        match read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Enter => break Some(std::mem::take(&mut key)),
                KeyCode::Esc => break None,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break None,
                KeyCode::Backspace => {
                    key.pop();
                }
                KeyCode::Char(c) => key.push(c),
                _ => {}
            },
            Ok(Event::Paste(text)) => key.push_str(text.trim()),
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    let _ = disable_raw_mode();
    eprintln!();
    result
}

/// Every provider with its real state, for `/connect`. Agent-CLI login
/// checks start processes, so they run in parallel, and the whole list
/// is built on a background thread.
fn list_providers() -> Vec<se_tui::ProviderEntry> {
    let registry = Registry::new();
    let ids: Vec<String> = registry.ids().into_iter().map(str::to_string).collect();
    std::thread::scope(|scope| {
        let jobs: Vec<_> = ids
            .iter()
            .map(|id| {
                let registry = &registry;
                scope.spawn(move || {
                    let p = registry.get(id).expect("listed by the registry");
                    let status = p.detect();
                    let (uses_key, ready, line) = if p.is_agent_cli() {
                        match (status.configured, p.login_status()) {
                            (false, _) => (false, false, status.detail),
                            (true, Some(Ok(s))) => (false, true, s),
                            (true, Some(Err(s))) => (false, false, s),
                            (true, None) => (false, true, status.detail),
                        }
                    } else if id == "ollama" {
                        (
                            false,
                            status.configured,
                            "local, no key needed: start Ollama to use it".to_string(),
                        )
                    } else if status.configured {
                        let line = match status.source {
                            se_provider::Source::Environment => {
                                format!("key from the {} environment variable", p.env_var())
                            }
                            _ => "key saved".to_string(),
                        };
                        (true, true, line)
                    } else if p.current_origin().is_none() {
                        (true, false, format!("needs its API address: /connect {id} https://<api-address>/v1"))
                    } else {
                        (
                            true,
                            false,
                            "no key yet: choose it to paste one".to_string(),
                        )
                    };
                    se_tui::ProviderEntry {
                        id: id.clone(),
                        name: p.display_name().to_string(),
                        uses_key,
                        needs_url: uses_key && p.current_origin().is_none(),
                        ready,
                        status: line,
                    }
                })
            })
            .collect();
        jobs.into_iter().filter_map(|j| j.join().ok()).collect()
    })
}

fn disconnect(id: &str) -> std::process::ExitCode {
    match credentials::delete(id) {
        Ok(()) => {
            println!("removed any stored credential for {id}");
            let source = Registry::new().get(id).map(|p| p.detect().source);
            if source == Some(se_provider::Source::Environment) {
                println!("note: an environment variable still supplies a key for {id}");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("se: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn providers() -> std::process::ExitCode {
    print!("{}", providers_report());
    std::process::ExitCode::SUCCESS
}

/// Every configured provider's models, listed in parallel so one slow
/// provider (an agent CLI, a cold endpoint) does not hold up the rest.
/// Runs on a background thread for `/models`.
fn list_all_models(refresh: bool) -> Vec<se_tui::ProviderModels> {
    let registry = Registry::new();
    std::thread::scope(|scope| {
        let jobs: Vec<_> = registry
            .configured()
            .into_iter()
            .map(|p| {
                scope.spawn(move || {
                    let usable = p.usable_models(refresh);
                    se_tui::ProviderModels {
                        provider: p.id().to_string(),
                        unchecked: usable
                            .as_ref()
                            .map(|u| u.unchecked.clone())
                            .unwrap_or_default(),
                        note: usable
                            .as_ref()
                            .ok()
                            .and_then(|u| u.hidden_summary("/models refresh")),
                        models: usable
                            .map(|u| u.models.into_iter().map(|m| m.id).collect())
                            .map_err(|e| e.to_string()),
                    }
                })
            })
            .collect();
        jobs.into_iter().filter_map(|j| j.join().ok()).collect()
    })
}

fn providers_report() -> String {
    let mut out = String::new();
    // Writes into `out`, so the same report serves the CLI and `/`-commands.
    macro_rules! println {
        () => {
            out.push('\n')
        };
        ($($arg:tt)*) => {{
            out.push_str(&format!($($arg)*));
            out.push('\n');
        }};
    }
    for (id, display, status) in Registry::new().statuses() {
        let mark = if status.configured { "✓" } else { "·" };
        println!("{mark} {id:<12} {display}");
        println!("               {}", status.detail);
    }
    println!();
    println!("✓ means a credential is present, not that the provider is reachable.");
    println!("Run `se models <provider>` to actually contact one.");
    out
}

/// `~/SE/se/agents` (`%USERPROFILE%` on Windows, `$HOME` elsewhere),
/// the same real catalogue tree `se-catalogue`'s own tests load --
/// overridable by `SE_CATALOGUE_ROOT` for a different install layout.
/// Attaches `.se/audit.jsonl` (every gateway decision, row 2.5). A file
/// that cannot be opened is reported, not fatal, matching session logs.
/// Appends one panic, with its thread and a backtrace, to
/// `.se/crash.log`. Best effort: a crash report must never cause a
/// second panic.
fn append_crash_log(info: &std::panic::PanicHookInfo<'_>) {
    use std::io::Write;
    let Ok(dir) = std::env::current_dir().map(|d| d.join(".se")) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("crash.log"))
    else {
        return;
    };
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let thread = std::thread::current();
    let _ = writeln!(
        file,
        "--- se {} panic at unix {at} on thread `{}`\n{info}\n{}",
        env!("CARGO_PKG_VERSION"),
        thread.name().unwrap_or("unnamed"),
        std::backtrace::Backtrace::force_capture()
    );
}

/// Fail closed: with no writable audit log, tools are refused rather
/// than run unrecorded, and the operator is told why.
/// `~/.se/last-model`: the model last picked with `/models`, one line
/// `provider/model`. Used when no `--provider`/`--model` is given.
fn last_model_path() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| std::path::PathBuf::from(h).join(".se").join("last-model"))
}

fn load_last_model() -> Option<(String, String)> {
    let text = std::fs::read_to_string(last_model_path()?).ok()?;
    let (p, m) = text.trim().split_once('/')?;
    (!p.is_empty() && !m.is_empty()).then(|| (p.to_string(), m.to_string()))
}

/// Best effort: failing to remember a choice never breaks the session.
fn save_last_model(m: &se_protocol::ModelRef) {
    if let Some(path) = last_model_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, format!("{}/{}\n", m.provider, m.model));
    }
}

/// A saved conversation: `.se/sessions/<id>.conversation.json`.
struct Convo {
    id: String,
    path: std::path::PathBuf,
    /// Messages already in it (0 for a new conversation).
    prior: usize,
}

impl Convo {
    /// `<id>.specialist` beside the conversation; outside
    /// `latest_conversation`'s `.conversation.json` filter by design.
    fn specialist_path(&self) -> std::path::PathBuf {
        self.path.with_file_name(format!("{}.specialist", self.id))
    }

    fn note(&self) -> String {
        if self.prior == 0 {
            format!(
                "conversation {} — continue later with `se --resume={}`",
                self.id, self.id
            )
        } else {
            format!(
                "resumed conversation {} ({} saved messages; older turns are kept as a summary)",
                self.id, self.prior
            )
        }
    }
}

/// The conversation this run continues (`--resume`) or starts.
fn conversation(resume: Option<&str>) -> Result<Convo, String> {
    let dir = se_core::session_store::default_sessions_dir(
        &std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?,
    );
    let path_of = |id: &str| dir.join(format!("{id}.conversation.json"));
    let id = match resume {
        None => unix_ms().to_string(),
        Some("latest") => latest_conversation(&dir)
            .ok_or_else(|| format!("no saved conversation in {} to resume", dir.display()))?,
        Some(id) => {
            if !path_of(id).exists() {
                return Err(format!("no saved conversation `{id}` in {}", dir.display()));
            }
            id.to_string()
        }
    };
    let path = path_of(&id);
    let prior = se_core::load_conversation(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?
        .len();
    Ok(Convo { id, path, prior })
}

/// The most recently saved conversation's id in `dir`.
fn latest_conversation(dir: &std::path::Path) -> Option<String> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".conversation.json")?.to_string();
            Some((e.metadata().ok()?.modified().ok()?, id))
        })
        .max()
        .map(|(_, id)| id)
}

/// History size (estimated tokens) at which older turns are summarized:
/// `SE_COMPACT_TOKENS`, default 12000; `0` turns compaction off.
fn compact_at() -> Option<u32> {
    std::env::var("SE_COMPACT_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(Some(12_000))
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn with_audit_log(runner: ProviderRunner) -> (ProviderRunner, Option<String>) {
    let opened = std::env::current_dir()
        .map(|cwd| cwd.join(".se").join("audit.jsonl"))
        .and_then(|path| se_core::audit_log::FileAuditSink::open(&path));
    match opened {
        Ok(sink) => (runner.with_audit_sink(std::sync::Arc::new(sink)), None),
        Err(e) => (
            runner.with_audit_sink(std::sync::Arc::new(
                se_core::audit_log::UnavailableAuditSink(e.to_string()),
            )),
            Some(format!(
                "⚠ no audit log ({e}): tools are disabled until .se/audit.jsonl is writable"
            )),
        ),
    }
}

fn catalogue_root() -> Option<std::path::PathBuf> {
    if let Ok(root) = std::env::var("SE_CATALOGUE_ROOT") {
        return Some(std::path::PathBuf::from(root));
    }
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|home| {
            std::path::PathBuf::from(home)
                .join("SE")
                .join("se")
                .join("agents")
        })
}

/// Lists every agent the real catalogue loads, or shows one in full.
///
/// Reads the catalogue fresh on every invocation (no cache, no
/// directory watcher, per the architecture doc) -- a stale list here
/// would be worse than the cost of a rescan.
fn agents(id: Option<&str>) -> std::process::ExitCode {
    let Some(root) = catalogue_root() else {
        eprintln!("se: could not determine a home directory to find the agent catalogue in");
        return std::process::ExitCode::FAILURE;
    };
    if !root.exists() {
        eprintln!(
            "se: no agent catalogue at {} (set SE_CATALOGUE_ROOT to override)",
            root.display()
        );
        return std::process::ExitCode::FAILURE;
    }
    let report = match se_catalogue::load(&root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "se: could not read the catalogue at {}: {e}",
                root.display()
            );
            return std::process::ExitCode::FAILURE;
        }
    };

    match id {
        None => {
            let mut agents = report.agents;
            agents.sort_by(|a, b| a.frontmatter.id.cmp(&b.frontmatter.id));
            for a in &agents {
                println!("{:<32} {}", a.frontmatter.id, a.frontmatter.mission.trim());
            }
            eprintln!(
                "se: {} agent(s) loaded from {}, {} file(s) skipped",
                agents.len(),
                root.display(),
                report.skipped.len()
            );
            std::process::ExitCode::SUCCESS
        }
        Some(id) => match report.agents.into_iter().find(|a| a.frontmatter.id == id) {
            Some(a) => {
                println!("{} — {}", a.frontmatter.id, a.frontmatter.name);
                println!("department: {}", a.frontmatter.department);
                if !a.frontmatter.tools.is_empty() {
                    println!("tools: {}", a.frontmatter.tools.join(", "));
                }
                println!();
                print!("{}", a.body);
                std::process::ExitCode::SUCCESS
            }
            None => {
                eprintln!("se: no agent `{id}` in the catalogue — try `se agents` to list ids");
                std::process::ExitCode::from(2)
            }
        },
    }
}

fn models(cli: &Cli, provider: Option<&str>, refresh: bool) -> std::process::ExitCode {
    let registry = Registry::new();
    let id = match provider
        .map(str::to_string)
        .or_else(|| cli.provider.clone())
    {
        Some(id) => id,
        None => {
            let configured = registry.configured();
            if configured.len() == 1 {
                configured[0].id().to_string()
            } else {
                eprintln!("se: name a provider, e.g. `se models nvidia`");
                return std::process::ExitCode::from(2);
            }
        }
    };
    let Some(provider) = registry.get(&id) else {
        eprintln!("se: {} is not a known provider", shown_id(&id));
        return std::process::ExitCode::from(2);
    };
    match provider.usable_models(refresh) {
        Ok(usable) => {
            for m in &usable.models {
                let mut line = m.id.clone();
                if let Some(ctx) = m.context_window {
                    line.push_str(&format!("   context {ctx}"));
                }
                if usable.unchecked.contains(&m.id) {
                    line.push_str("   (unchecked)");
                }
                println!("{line}");
            }
            eprintln!("se: {} usable model(s) from {id}", usable.models.len());
            let hint = format!("se models {id} --refresh");
            if let Some(note) = usable.hidden_summary(&hint) {
                eprintln!("se: {note}");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("se: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Prints the slash-command surface, grouped by category.
///
/// Reads `se_command::builtins::registry_with_builtins()` -- the same
/// construction the TUI uses -- so this output and the picker's list are
/// two projections of one source rather than two lists to keep in sync.
fn commands() {
    let registry = se_command::builtins::registry_with_builtins();
    println!(
        "SE slash commands (also available in the TUI by typing `/`)
"
    );
    for category in se_command::builtins::CATEGORIES {
        let in_category: Vec<&se_command::CommandSpec> = registry
            .query("")
            .into_iter()
            .filter(|c| c.category == *category)
            .collect();
        if in_category.is_empty() {
            continue;
        }
        println!("{category}:");
        for spec in in_category {
            // State is reported, never implied: a command that cannot
            // run yet says so here exactly as it does in the TUI.
            let state = match se_command::builtins::availability(&spec.id) {
                Some(se_command::builtins::Availability::Ready) => String::new(),
                Some(se_command::builtins::Availability::NotImplemented(r))
                | Some(se_command::builtins::Availability::Unconfigured(r)) => {
                    format!("  [unavailable: {r}]")
                }
                None => "  [no availability record]".to_string(),
            };
            let aliases = if spec.aliases.is_empty() {
                String::new()
            } else {
                format!(" ({})", spec.aliases.join(", "))
            };
            println!(
                "  /{:<13}{:<58}{}",
                format!("{}{}", spec.path, aliases),
                spec.description,
                state
            );
        }
        println!();
    }
}

fn doctor(startup: se_protocol::Running) -> std::process::ExitCode {
    print!("{}", doctor_report(startup.finish().duration));
    std::process::ExitCode::SUCCESS
}

fn doctor_report(startup: Duration) -> String {
    let mut out = String::new();
    // Writes into `out`, so the same report serves the CLI and `/`-commands.
    macro_rules! println {
        () => {
            out.push('\n')
        };
        ($($arg:tt)*) => {{
            out.push_str(&format!($($arg)*));
            out.push('\n');
        }};
    }
    let registry = Registry::new();
    let configured: Vec<&str> = registry.configured().iter().map(|p| p.id()).collect();

    println!("SE doctor");
    println!("  version            {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  this executable    {}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    );
    println!("  startup            {}", format_duration(startup));
    println!(
        "  terminal           {}",
        if io::stdout().is_terminal() {
            "yes — the interface can run"
        } else {
            "no — headless only"
        }
    );
    println!(
        "  providers          {} of {} have a credential{}",
        configured.len(),
        registry.ids().len(),
        if configured.is_empty() {
            String::new()
        } else {
            format!(" ({})", configured.join(", "))
        }
    );
    if configured.is_empty() {
        println!("                     run `se connect <provider> --from-env <VAR>`");
    }
    println!(
        "                     a credential is not a reachability check — run `se models <id>`"
    );
    println!();
    println!("  What works in this build:");
    println!("    interface        event-driven; never blocks on a turn");
    println!("    turn lifecycle   streaming, cancellation, approval, timings");
    println!("    providers        OpenAI-compatible streaming adapter (NVIDIA, OpenAI, Gemini,");
    println!("                     OpenRouter, DeepSeek, Groq, Ollama, custom) plus native");
    println!("                     Anthropic. Live-verified on NVIDIA only; `se models <id>`");
    println!("                     lists models, but a listed model may still refuse your key");
    println!("    subscriptions    Claude Code (your Claude plan), Codex (your ChatGPT plan),");
    println!("                     OpenCode, Kilo Code and Cline run as text-only backends");
    println!("                     through their own logins: `se providers` for each one's");
    println!("                     tool limits. SE never reads their login tokens");
    println!("    retries          a 429/5xx/network failure before anything streamed is");
    println!("                     retried twice (1s, 3s, or the provider's retry-after)");
    println!("    prompt caching   native Anthropic: system prompt and newest turn cached");
    println!("    credentials      OS-native secure storage");
    println!("    specialists      every turn runs as one of the catalogue's agents: named");
    println!("                     with /<id>, else picked automatically, else");
    println!("                     chief-orchestrator -- never a bare model. A specialist's");
    println!("                     `tools:` list narrows what the gateway allows it. A");
    println!("                     conversation keeps its specialist; /<id> switches it");
    println!("    conversation     saved to .se/sessions/<id>.conversation.json after every");
    println!("                     turn; `se --resume[=id]` continues it on any provider or");
    println!("                     model. Past SE_COMPACT_TOKENS (default 12000) older turns");
    println!("                     are summarized once and the summary replaces them");
    println!("    model switch     /models <provider>/<model> mid-conversation keeps the");
    println!("                     context (summary + recent turns)");
    println!("    tools            read, write, edit, search, shell -- gated behind a");
    println!("                     permission gateway; every call asks for approval");
    println!("    audit            every gateway decision in .se/audit.jsonl");
    println!("    teams            `se team run <file.yaml>`: several specialists on several");
    println!("                     vendors' models, ensembles with an evaluator that shows");
    println!("                     disagreement (never votes), review stage, budget, resume");
    println!();
    println!("  Not implemented yet, and not pretended otherwise:");
    println!("    ruleset config   no file to pre-allow or pre-deny by agent/tool/path, so");
    println!("                     every tool call asks");
    println!("    skills           the loader exists (se-catalogue) but nothing calls it");
    println!("    MCP, pricing     Phase 5");
    println!("    agent CLI tools  external CLIs answer as text only: SE's own tools are");
    println!("                     not offered to them, so they cannot edit your project");
    out
}
