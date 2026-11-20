//! `se team run <file>` and `se team resume <journal>`: a multi-agent,
//! multi-vendor run from the command line, with no interface needed.
//!
//! Progress goes to stderr, each task's final answer to stdout, so the
//! output is pipeable. Approvals are asked on the terminal; with no
//! terminal attached, a call needing approval is refused, never granted.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use se_core::protocol::{ApprovalDecision, Outcome, Payload};
use se_core::team::{file, monitor, Limits, NodeState, TeamRun};
use se_core::ProviderRunner;
use se_permission::Ruleset;

pub fn run_file(cli: &crate::Cli, path: &Path) -> ExitCode {
    match start(cli, path, cli.mode) {
        Ok((run, notes)) => {
            for n in notes {
                eprintln!("se: {n}");
            }
            drive(run)
        }
        Err(e) => {
            eprintln!("se: {e}");
            ExitCode::from(2)
        }
    }
}

/// Parses `path`, builds a runner with exactly the vendor consent the
/// file gives, and starts a journaled run under `.se/runs/`. Shared by
/// `se team run` and the interface's `/team run`; returns lines for the
/// operator instead of printing them, so it is safe inside the TUI.
pub fn start(
    cli: &crate::Cli,
    path: &Path,
    mode: se_core::protocol::WorkMode,
) -> Result<(TeamRun, Vec<String>), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let team = file::parse(&text)?;
    let (runner, warning) = build_runner(
        cli,
        &team.graph,
        Ruleset::from_rules(team.provider_rules),
        mode,
    )?;
    let journal = std::env::current_dir()
        .map_err(|e| format!("no working directory: {e}"))?
        .join(".se")
        .join("runs")
        .join(format!("{}.jsonl", unix_ms()));
    let run = TeamRun::start_journaled(team.graph, runner, team.limits, &journal)
        .map_err(|e| format!("cannot create the run journal {}: {e}", journal.display()))?;
    let mut notes: Vec<String> = warning.into_iter().collect();
    notes.push(format!(
        "run journal {} (resume with `se team resume <path>`)",
        journal.display()
    ));
    Ok((run, notes))
}

pub fn resume(cli: &crate::Cli, journal: &Path, max_concurrent: Option<usize>) -> ExitCode {
    // Consent comes from the journaled graph itself: exactly the vendors
    // the original file named, nothing more.
    let (graph, saved_limits) = match se_core::team::journal::resume(journal) {
        Ok(r) => (r.graph, r.limits),
        Err(e) => {
            eprintln!("se: cannot read {}: {e}", journal.display());
            return ExitCode::from(2);
        }
    };
    let rules = file::provider_rules(&graph);
    let runner = match build_runner(cli, &graph, Ruleset::from_rules(rules), cli.mode) {
        Ok((runner, warning)) => {
            if let Some(w) = warning {
                eprintln!("se: {w}");
            }
            runner
        }
        Err(e) => {
            eprintln!("se: {e}");
            return ExitCode::from(2);
        }
    };
    // The run keeps the limits it was started with; only concurrency may
    // be changed on resume.
    let limits = max_concurrent.map(|n| Limits {
        max_concurrent: n,
        ..saved_limits.unwrap_or_default()
    });
    let mut run = match TeamRun::resume(journal, runner, limits) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("se: cannot resume {}: {e}", journal.display());
            return ExitCode::FAILURE;
        }
    };
    let interrupted: Vec<String> = run
        .graph()
        .nodes()
        .iter()
        .filter(|n| run.graph().state(&n.id) == Some(NodeState::NeedsConfirmation))
        .map(|n| n.id.clone())
        .collect();
    for node in interrupted {
        // Its side effects are unknown: only a human may say what happened.
        let answer = crate::ask_terminal(&format!(
            "task `{node}` was running when SE stopped. Did its work finish? \
             [y = mark done, r = run it again, anything else = cancel] "
        ));
        let result = match answer.as_deref() {
            Some("y") | Some("yes") => run.confirm(&node, true),
            Some("r") => run.confirm(&node, false),
            _ => run.cancel(&node),
        };
        if let Err(e) = result {
            eprintln!("se: {e}");
        }
    }
    drive(run)
}

fn build_runner(
    cli: &crate::Cli,
    graph: &se_core::team::TaskGraph,
    ruleset: Ruleset,
    mode: se_core::protocol::WorkMode,
) -> Result<(ProviderRunner, Option<String>), String> {
    let (provider_id, model) = default_model(cli, graph)?;
    let runner = ProviderRunner::new(&provider_id, &model)
        .map_err(|e| e.to_string())?
        .with_ruleset(ruleset);
    let runner = match crate::catalogue_root() {
        Some(root) => runner
            .with_catalogue_root(root)
            .with_auto_route(true)
            .with_router_model(crate::router_model())
            .with_role_models(crate::role_models()),
        None => runner,
    };
    se_core::Runner::set_mode(&runner, mode);
    Ok(crate::with_audit_log(runner))
}

/// The runner's default model: `--provider`/`--model` when given, else the
/// first model the team file names, so a file that names its models runs
/// with no extra flags even when several providers are configured.
fn default_model(
    cli: &crate::Cli,
    graph: &se_core::team::TaskGraph,
) -> Result<(String, String), String> {
    if cli.provider.is_none() && cli.model.is_none() {
        if let Some(m) = graph.nodes().iter().find_map(|n| n.model.as_ref()) {
            return Ok((m.provider.clone(), m.model.clone()));
        }
    }
    crate::resolve(cli)
}

fn drive(mut run: TeamRun) -> ExitCode {
    let mut last: HashMap<String, NodeState> = HashMap::new();
    report_changes(&run, &mut last);
    while !run.is_done() {
        for event in run.tick_blocking(Duration::from_millis(100)) {
            if let Payload::ApprovalRequired {
                call,
                name,
                summary,
            } = &event.payload
            {
                // Item 8: every approval names the task, agent and model;
                // the summary names the resolved target.
                let node = run.node_of(event.task).unwrap_or("?").to_string();
                let rec = run.record(&node);
                let agent = rec.and_then(|r| r.agent.as_deref()).unwrap_or("auto");
                let model = rec
                    .and_then(|r| Some(format!("{}/{}", r.provider.as_ref()?, r.model.as_ref()?)))
                    .unwrap_or_else(|| "default".into());
                let prefix = format!("[task {node} | {agent} | {model}] ");
                let allow = crate::ask_approval(&prefix, name, summary);
                run.approve(ApprovalDecision {
                    task: event.task,
                    call: *call,
                    allow,
                });
            }
        }
        for notice in run.take_notices() {
            eprintln!("se: {notice}");
        }
        // No strip here (stderr lines below say the same); drained so they don't pile up.
        run.take_agent_events();
        report_changes(&run, &mut last);
        if let Some(e) = run.journal_error() {
            eprintln!("se: the run journal failed ({e}); the run was stopped");
        }
        // Row 4.16: at the cap nothing more is spent until a human says so.
        if let Some(reason) = run.budget_stalled() {
            let answer = crate::ask_terminal(&format!(
                "se: {reason}. Add how many tokens? [number, or Enter to stop] "
            ));
            match answer
                .and_then(|a| a.parse::<u64>().ok())
                .filter(|n| *n > 0)
            {
                Some(extra) => run.raise_budget(extra),
                None => {
                    eprintln!("se: stopping: the budget is spent");
                    run.cancel_all();
                }
            }
        }
    }

    let mut all_ok = true;
    for n in run.graph().nodes() {
        let state = run.graph().state(&n.id).expect("own node");
        all_ok &= state == NodeState::Completed;
        let who = match (n.agent.as_str(), n.kind) {
            (_, se_core::team::NodeKind::Evaluate) => "evaluator",
            ("", _) => "auto",
            (agent, _) => agent,
        };
        println!("\n===== {} ({who}) — {state:?} =====", n.id);
        match run.outcome(&n.id) {
            Some(Outcome::Completed { final_text }) => println!("{final_text}"),
            Some(Outcome::Failed { detail }) => println!("failed: {detail}"),
            Some(Outcome::Incomplete { reason, .. }) => println!("incomplete: {reason}"),
            Some(other) => println!("{}", other.label()),
            None => println!("(did not run)"),
        }
    }
    // Who did what, on which vendor, at what reported cost (item 10).
    eprintln!();
    for line in monitor::render(&monitor::rows(run.graph(), |n| run.record(n))) {
        eprintln!("se: {line}");
    }
    let (spent, limit, unreported) = run.budget();
    let limit = limit.map_or(String::new(), |l| format!(" of a {l}-token budget"));
    eprintln!("se: {spent} reported tokens{limit}");
    if unreported > 0 {
        eprintln!("se: {unreported} model call(s) reported no usage and are not counted");
    }
    if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// One stderr line per node whose state changed since the last report.
fn report_changes(run: &TeamRun, last: &mut HashMap<String, NodeState>) {
    for n in run.graph().nodes() {
        let state = run.graph().state(&n.id).expect("own node");
        if last.get(&n.id) != Some(&state) {
            let model = n
                .model
                .as_ref()
                .map_or(String::new(), |m| format!(" on {}/{}", m.provider, m.model));
            eprintln!("se: [{}] {state:?}{model}", n.id);
            last.insert(n.id.clone(), state);
        }
    }
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}
