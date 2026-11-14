//! The `se` binary: wiring and terminal lifecycle.
//!
//! Three responsibilities, deliberately no more: parse the command line,
//! own raw mode and the alternate screen, and connect a core to an
//! interface. All behaviour lives in the libraries, so this file stays
//! short enough to audit by eye -- the previous implementation's binary
//! crate grew to 15,000 lines precisely because it kept acquiring
//! "just one more" responsibility.

use std::io::{self, IsTerminal, Stdout, Write};
use std::time::Duration;

use clap::{Parser, Subcommand};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use se_core::{EchoRunner, ProviderRunner};
use se_protocol::{timings::format_duration, Outcome, Payload, Stage};
use se_provider::{credentials, Registry};

#[derive(Parser)]
#[command(name = "se", version, about = "SE — a standalone engineering agent")]
struct Cli {
    /// Provider to use, e.g. `nvidia`. With exactly one provider
    /// configured SE uses it; with several it asks rather than guessing.
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Model id, exactly as the provider reports it.
    #[arg(long, global = true)]
    model: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
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
    Models { provider: Option<String> },
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
        Some(Command::Connect { provider, from_env }) => {
            connect(provider.as_deref(), from_env.as_deref())
        }
        Some(Command::Disconnect { provider }) => disconnect(provider),
        Some(Command::Providers) => providers(),
        Some(Command::Models { provider }) => models(&cli, provider.as_deref()),
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
    let provider_id = match &cli.provider {
        Some(id) => {
            let provider = registry
                .get(id)
                .ok_or_else(|| format!("`{id}` is not a known provider — try `se providers`"))?;
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
                        "several providers are configured ({}) — choose one with --provider",
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

    let mut terminal = enter()?;
    // Restoration must survive a panic, or a crash leaves the user with
    // no echo and no cursor.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        default_hook(info);
    }));

    // Whether this session will actually reach a model, decided once
    // and shown everywhere a user could be looking: the header never
    // calls an echoed response an AI-generated one, and this banner is
    // the on-screen counterpart of the same fact.
    let (core, banner) = match &resolved {
        Ok((provider_id, model)) => match ProviderRunner::new(provider_id, model) {
            Ok(runner) => (se_core::start(runner), None),
            Err(e) => {
                let _ = restore();
                eprintln!("se: {e}");
                return Ok(());
            }
        },
        // No usable provider: the interface still runs, against a local
        // echo, so the user sees the application and reads why rather
        // than being dropped back to a bare shell -- but it must never
        // be mistaken for a real reply, which is what this banner is for.
        Err(reason) => (
            se_core::start(EchoRunner::instant()),
            Some(format!(
                "⚠ no model connected ({reason}) — replies below are a local echo, not AI"
            )),
        ),
    };

    // Best-effort: a session that cannot be logged still runs, exactly
    // as it always did -- persistence is an added guarantee, not a new
    // way for SE to refuse to start. A failure here is folded into the
    // banner the user already sees, rather than silently dropped.
    let mut banner = banner;
    if let Ok(cwd) = std::env::current_dir() {
        let path = se_core::session_store::default_sessions_dir(&cwd)
            .join(format!("{}.jsonl", core.session()));
        if let Err(e) = core.attach_session_log(&path) {
            banner = Some(match banner {
                Some(existing) => format!("{existing} — also: session not logged ({e})"),
                None => format!("⚠ session not logged ({e})"),
            });
        }
    }

    let first_frame = startup.finish();
    let result = se_tui::run(&mut terminal, &core, banner);
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

fn enter() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste matters for correctness, not comfort: without it
    // a pasted multi-line prompt arrives as individual Enter presses,
    // so the first newline submits a half-pasted request. The interface
    // already handles `Event::Paste`; until now that arm was
    // unreachable because the mode was never enabled.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
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

    let core = se_core::start(runner);
    if let Err(e) = core.submit(request, Vec::new()) {
        eprintln!("se: {e}");
        return std::process::ExitCode::FAILURE;
    }

    loop {
        for event in core.drain_blocking(Duration::from_millis(100)) {
            match event.payload {
                Payload::Delta { text, .. } => {
                    print!("{text}");
                    let _ = io::stdout().flush();
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

fn connect(provider: Option<&str>, from_env: Option<&str>) -> std::process::ExitCode {
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
        eprintln!("se: `{id}` is not a known provider — run `se connect` to list them");
        return std::process::ExitCode::from(2);
    };

    // Read from a named variable rather than an argument: a key passed
    // on a command line ends up in shell history and in the process
    // list. A masked prompt belongs in the interface, not here.
    let Some(var) = from_env else {
        eprintln!(
            "se: pass --from-env <VARIABLE> holding the key, so it never appears in shell history"
        );
        eprintln!("se: e.g.  $env:SE_KEY = '...' ; se connect {id} --from-env SE_KEY");
        return std::process::ExitCode::from(2);
    };
    let Ok(secret) = std::env::var(var) else {
        eprintln!("se: {var} is not set");
        return std::process::ExitCode::from(2);
    };
    if secret.trim().is_empty() {
        eprintln!("se: {var} is empty");
        return std::process::ExitCode::from(2);
    }

    // Pins the credential to the endpoint it is being connected to now
    // (E2): a later change to the service's base URL -- including by an
    // attacker-set environment variable -- is refused rather than
    // silently honoured. `current_origin()` is `None` only for a
    // service with no resolvable endpoint, which `store` treats as
    // "nothing to pin yet".
    let origin = provider.current_origin();
    if let Err(e) = credentials::store(id, &secret, origin.as_deref()) {
        eprintln!("se: could not store the credential: {e}");
        return std::process::ExitCode::FAILURE;
    }
    // Length only. Never any part of the key.
    println!(
        "stored a credential for {id} ({})",
        credentials::fingerprint(&secret)
    );

    // Storing is not connecting: confirm against the provider's own
    // endpoint before claiming anything works.
    print!("validating… ");
    let _ = io::stdout().flush();
    match provider.validate() {
        Ok(models) if models.is_empty() => {
            println!("the credential was accepted, but the provider reported no models");
            std::process::ExitCode::SUCCESS
        }
        Ok(models) => {
            println!("reachable — {} model(s) reported", models.len());
            for m in models.iter().take(5) {
                println!("  {}", m.id);
            }
            if models.len() > 5 {
                println!("  … run `se models {id}` for the rest");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!();
            eprintln!("se: the credential was stored but did not work: {e}");
            eprintln!("se: replace it with another `se connect {id}`, or remove it with `se disconnect {id}`");
            std::process::ExitCode::FAILURE
        }
    }
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
    for (id, display, status) in Registry::new().statuses() {
        let mark = if status.configured { "✓" } else { "·" };
        println!("{mark} {id:<12} {display}");
        println!("               {}", status.detail);
    }
    println!();
    println!("✓ means a credential is present, not that the provider is reachable.");
    println!("Run `se models <provider>` to actually contact one.");
    std::process::ExitCode::SUCCESS
}

/// `~/SE/se/agents` (`%USERPROFILE%` on Windows, `$HOME` elsewhere),
/// the same real catalogue tree `se-catalogue`'s own tests load --
/// overridable by `SE_CATALOGUE_ROOT` for a different install layout.
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

fn models(cli: &Cli, provider: Option<&str>) -> std::process::ExitCode {
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
        eprintln!("se: `{id}` is not a known provider");
        return std::process::ExitCode::from(2);
    };
    match provider.list_models() {
        Ok(models) => {
            for m in &models {
                let mut line = m.id.clone();
                if let Some(ctx) = m.context_window {
                    line.push_str(&format!("   context {ctx}"));
                }
                println!("{line}");
            }
            eprintln!("se: {} model(s) reported by {id}", models.len());
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
    let span = startup.finish();
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
    println!("  startup            {}", format_duration(span.duration));
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
    println!("    providers        one OpenAI-compatible streaming adapter, with endpoints");
    println!("                     catalogued for NVIDIA, OpenAI, Gemini, OpenRouter,");
    println!("                     DeepSeek, Groq, Ollama and custom roots. Tested against");
    println!("                     mock endpoints only — no live vendor API has been called");
    println!("                     from this build, so `se models <id>` is the real check");
    println!("    credentials      OS-native secure storage");
    println!("    conversation     history is sent back on later turns (in memory only)");
    println!("    tools            read, write, edit, search, shell -- gated behind a");
    println!("                     permission gateway; every call asks for approval");
    println!("                     unless a ruleset says otherwise (none is configured yet,");
    println!("                     so every call asks)");
    println!("    sessions         logged to .se/sessions/<id>.jsonl; `se` resumes a");
    println!("                     killed-mid-turn session as interrupted, not completed");
    println!("    agent catalogue  `se agents` / `se agents <id>` read the real 194-agent");
    println!("                     catalogue; browse-only -- a specialist cannot be");
    println!("                     dispatched as a model turn yet");
    println!();
    println!("  Not implemented yet, and not pretended otherwise:");
    println!("    ruleset config   every tool call asks for approval -- no way yet to");
    println!("                     pre-allow or pre-deny by agent/tool/path");
    println!("    specialist       Phase 3 -- the catalogue loads and lists, but a");
    println!("    dispatch         specialist's body is not yet used as a turn's prompt");
    println!("    skills           Phase 3 -- not loaded by this build yet");
    println!("    multi-model      Phase 4 -- team orchestration, task graphs");
    println!("    external CLIs    Phase 6 -- Claude Code, Codex, OpenCode");
    std::process::ExitCode::SUCCESS
}
