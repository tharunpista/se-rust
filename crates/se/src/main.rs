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
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
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
}

fn main() -> std::process::ExitCode {
    let startup = se_protocol::Running::start(Stage::Startup);
    let cli = Cli::parse();

    match &cli.command {
        Some(Command::Exec { request }) => exec(&cli, request),
        Some(Command::Connect { provider, from_env }) => {
            connect(provider.as_deref(), from_env.as_deref())
        }
        Some(Command::Disconnect { provider }) => disconnect(provider),
        Some(Command::Providers) => providers(),
        Some(Command::Models { provider }) => models(&cli, provider.as_deref()),
        Some(Command::Doctor) => doctor(startup),
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

    let core = match &resolved {
        Ok((provider_id, model)) => match ProviderRunner::new(provider_id, model) {
            Ok(runner) => se_core::start(runner),
            Err(e) => {
                let _ = restore();
                eprintln!("se: {e}");
                return Ok(());
            }
        },
        // No usable provider: the interface still runs, against the echo
        // runner, so the user sees the application and reads the reason
        // instead of being dropped back to a bare shell.
        Err(_) => se_core::start(EchoRunner::instant()),
    };

    let first_frame = startup.finish();
    let result = se_tui::run(&mut terminal, &core);
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore() -> io::Result<()> {
    let mut stdout = io::stdout();
    // Each step is attempted even if an earlier one failed: leaving raw
    // mode enabled is far worse than a redundant escape sequence.
    let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
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

    if let Err(e) = credentials::store(id, &secret) {
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
    println!("    providers        OpenAI-compatible streaming (NVIDIA, OpenAI, Gemini,");
    println!("                     OpenRouter, DeepSeek, Groq, Ollama, custom endpoints)");
    println!("    credentials      OS-native secure storage");
    println!();
    println!("  Not implemented yet, and not pretended otherwise:");
    println!("    tools / sandbox  Phase 3 — the model cannot read or change files");
    println!("    persistence      Phase 4 — sessions do not survive exit");
    println!("    specialists      Phase 5");
    println!("    external CLIs    Phase 6 — Claude Code, Codex, OpenCode");
    std::process::ExitCode::SUCCESS
}
