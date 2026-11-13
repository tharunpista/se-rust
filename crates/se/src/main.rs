//! The `se` binary: wiring and terminal lifecycle.
//!
//! Three responsibilities, deliberately no more: parse the command line,
//! own raw mode and the alternate screen, and connect a core to an
//! interface. All behaviour lives in the libraries, so this file stays
//! short enough to audit by eye -- the previous implementation's binary
//! crate grew to 15,000 lines precisely because it kept acquiring
//! "just one more" responsibility.

use std::io::{self, IsTerminal, Stdout};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use se_core::EchoRunner;
use se_protocol::Stage;

#[derive(Parser)]
#[command(name = "se", version, about = "SE — a standalone engineering agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one request without the interface and print the result.
    /// Machine-readable progress comes later (Phase 2); this exists now
    /// so the core can be exercised where no tty is available.
    Exec {
        /// What to ask for.
        request: String,
    },
    /// Report what this build can and cannot do, honestly.
    Doctor,
}

fn main() -> std::process::ExitCode {
    let startup = se_protocol::Running::start(Stage::Startup);
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Exec { request }) => exec(&request),
        Some(Command::Doctor) => doctor(startup),
        None => {
            // A pipe is not a terminal: entering raw mode would fail or
            // corrupt the stream, so the headless path is used instead
            // of pretending.
            if !io::stdout().is_terminal() {
                eprintln!("se: stdout is not a terminal; use `se exec \"...\"` for headless use");
                return std::process::ExitCode::from(2);
            }
            match interactive(startup) {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("se: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
    }
}

fn interactive(startup: se_protocol::Running) -> io::Result<()> {
    let mut terminal = enter()?;
    // Restoration must survive a panic, or a crash leaves the user with
    // an unusable terminal and no echo. The hook runs before unwinding
    // reaches the guard below.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        default_hook(info);
    }));

    let core = se_core::start(EchoRunner::instant());
    let first_frame = startup.finish();

    let result = se_tui::run(&mut terminal, &core);

    core.shutdown();
    restore()?;
    // Printed after the screen is back, so it is still visible.
    eprintln!(
        "se: startup {}",
        se_protocol::timings::format_duration(first_frame.duration)
    );
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
    // mode enabled is much worse than a redundant escape sequence.
    let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
    let _ = disable_raw_mode();
    Ok(())
}

fn exec(request: &str) -> std::process::ExitCode {
    use se_protocol::{Outcome, Payload};

    let core = se_core::start(EchoRunner::instant());
    if let Err(e) = core.submit(request, Vec::new()) {
        eprintln!("se: {e}");
        return std::process::ExitCode::FAILURE;
    }

    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        for event in core.drain_blocking(Duration::from_millis(50)) {
            match event.payload {
                Payload::Delta { text, .. } => {
                    print!("{text}");
                    use std::io::Write;
                    let _ = io::stdout().flush();
                }
                Payload::TaskFinished { outcome, timings } => {
                    println!();
                    core.shutdown();
                    eprintln!(
                        "se: {} · local {} · {}",
                        outcome.label(),
                        se_protocol::timings::format_duration(timings.local()),
                        timings.summary()
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
    core.shutdown();
    eprintln!("se: the request did not finish within the time limit");
    std::process::ExitCode::from(4)
}

fn doctor(startup: se_protocol::Running) -> std::process::ExitCode {
    let span = startup.finish();
    println!("SE doctor");
    println!("  version            {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  this executable    {}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    );
    println!(
        "  startup            {}",
        se_protocol::timings::format_duration(span.duration)
    );
    println!(
        "  terminal           {}",
        if io::stdout().is_terminal() {
            "yes — the interface can run"
        } else {
            "no — headless only"
        }
    );
    println!();
    println!("  What works in this build:");
    println!("    interface        event-driven; never blocks on a turn");
    println!("    turn lifecycle   streaming, cancellation, approval, timings");
    println!("    runner           echo only — no model provider yet (Phase 2)");
    println!();
    println!("  Not implemented yet, and not pretended otherwise:");
    println!("    model providers  Phase 2");
    println!("    tools / sandbox  Phase 3");
    println!("    persistence      Phase 4");
    println!("    specialists      Phase 5");
    println!("    external CLIs    Phase 6");
    std::process::ExitCode::SUCCESS
}
