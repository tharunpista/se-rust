//! `/team` in the interface (Phase 4 item 10): start a team file, watch
//! its tasks change state, answer its approvals, see the monitor table.
//!
//! A team run has its own core and can ask for several approvals at
//! once, so they are not mixed into the chat's single y/n prompt: each
//! is queued and named (task, agent, model, target), and answered in
//! order with `/team allow` or `/team deny`.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use se_core::team::{monitor, NodeState, TeamRun};
use se_protocol::{ApprovalDecision, Outcome, Payload, TaskId, ToolCallId};

use crate::state::{AppState, Entry};

/// Builds a run from a team file. Supplied by the binary, which owns
/// provider, credential and catalogue construction.
/// Returns the run and lines to show the operator (journal path, warnings).
/// Starts a team file in the interface's current work mode.
pub type Launcher = Box<dyn Fn(&Path, se_protocol::WorkMode) -> Result<(TeamRun, Vec<String>), String>>;

#[derive(Default)]
pub struct Panel {
    launcher: Option<Launcher>,
    run: Option<TeamRun>,
    last: HashMap<String, NodeState>,
    approvals: VecDeque<(TaskId, ToolCallId, String)>,
    finished: bool,
    budget_notice: bool,
}

const USAGE: &str = "/team run <file.yaml> · /team (status) · /team allow · /team deny · \
                     /team budget <tokens> · /team cancel";

impl Panel {
    pub fn new(launcher: Option<Launcher>) -> Self {
        Self {
            launcher,
            ..Self::default()
        }
    }

    /// Handles `/team <args>`.
    pub fn command(&mut self, state: &mut AppState, args: &str) {
        let mut words = args.split_whitespace();
        let msg = match (words.next(), words.next()) {
            (None | Some("status"), _) => match &self.run {
                Some(run) => table(run),
                None => format!("no team run. {USAGE}"),
            },
            (Some("run"), Some(file)) => self.start(state, Path::new(file)),
            (Some("allow"), _) => self.answer(true),
            (Some("deny"), _) => self.answer(false),
            (Some("cancel"), _) => match self.run.as_mut() {
                Some(run) if !self.finished => {
                    run.cancel_all();
                    "cancelling every unfinished task".to_string()
                }
                _ => "no team run in progress".to_string(),
            },
            (Some("budget"), Some(n)) => match (self.run.as_mut(), n.parse::<u64>()) {
                (Some(run), Ok(extra)) if extra > 0 => {
                    run.raise_budget(extra);
                    self.budget_notice = false;
                    format!("budget raised by {extra} tokens")
                }
                _ => "expected `/team budget <tokens>` during a run".to_string(),
            },
            _ => USAGE.to_string(),
        };
        state.push(Entry::Notice(msg));
    }

    fn start(&mut self, state: &mut AppState, file: &Path) -> String {
        if self.run.is_some() && !self.finished {
            return "a team run is already in progress (`/team cancel` first)".into();
        }
        let Some(launch) = &self.launcher else {
            return "team runs need a configured provider; none is available here".into();
        };
        match launch(file, state.mode) {
            Ok((run, notes)) => {
                // The strip shows this run's agents only.
                crate::agents::set_focus(state, crate::agents::Focus::Composer);
                state.agents.agents.clear();
                self.run = Some(run);
                self.last.clear();
                self.approvals.clear();
                self.finished = false;
                self.budget_notice = false;
                let mut msg = format!("team run started from {}", file.display());
                for n in notes {
                    msg.push('\n');
                    msg.push_str(&n);
                }
                msg
            }
            Err(e) => e,
        }
    }

    fn answer(&mut self, allow: bool) -> String {
        let (Some(run), Some((task, call, what))) = (&self.run, self.approvals.pop_front()) else {
            return "no team approval is waiting".into();
        };
        run.approve(ApprovalDecision { task, call, allow });
        let verb = if allow { "allowed" } else { "denied" };
        match self.approvals.front() {
            Some((_, _, next)) => format!("{verb}: {what}\nnext waiting: {next}"),
            None => format!("{verb}: {what}"),
        }
    }

    /// Drains the run without blocking. Returns whether anything changed.
    pub fn tick(&mut self, state: &mut AppState) -> bool {
        let Some(run) = self.run.as_mut() else {
            return false;
        };
        if self.finished {
            return false;
        }
        let mut changed = false;
        for event in run.tick() {
            if let Payload::ApprovalRequired {
                call,
                name,
                summary,
            } = &event.payload
            {
                let node = run.node_of(event.task).unwrap_or("?").to_string();
                let rec = run.record(&node);
                let agent = rec.and_then(|r| r.agent.as_deref()).unwrap_or("auto");
                let model = rec
                    .and_then(|r| Some(format!("{}/{}", r.provider.as_ref()?, r.model.as_ref()?)))
                    .unwrap_or_else(|| "default".into());
                let what = format!("[task {node} | {agent} | {model}] {name}: {summary}");
                state.push(Entry::Notice(format!(
                    "team approval: {what} — `/team allow` or `/team deny`"
                )));
                self.approvals.push_back((event.task, *call, what));
                changed = true;
            }
        }
        for notice in run.take_notices() {
            state.push(Entry::Notice(notice));
            changed = true;
        }
        // After this tick's events and before the `is_done` check below, so
        // the final Done/Failed arrive in the same call that finishes the run.
        for e in run.take_agent_events() {
            state.agents.apply(&e);
            changed = true;
        }
        for n in run.graph().nodes() {
            let s = run.graph().state(&n.id).expect("own node");
            if self.last.insert(n.id.clone(), s) != Some(s) {
                state.push(Entry::Notice(format!("team [{}] {s:?}", n.id)));
                changed = true;
            }
        }
        if let Some(reason) = run.budget_stalled() {
            if !self.budget_notice {
                state.push(Entry::Notice(format!(
                    "team paused: {reason}. `/team budget <tokens>` to continue, \
                     `/team cancel` to stop"
                )));
                self.budget_notice = true;
                changed = true;
            }
        }
        if run.is_done() {
            self.finished = true;
            for n in run.graph().nodes() {
                let body = match run.outcome(&n.id) {
                    Some(Outcome::Completed { final_text }) => final_text.clone(),
                    Some(other) => other.label().to_string(),
                    None => "(did not run)".into(),
                };
                state.push(Entry::Notice(format!("team [{}] result:\n{body}", n.id)));
            }
            state.push(Entry::Notice(table(run)));
            changed = true;
        }
        changed
    }
}

/// The monitor table plus the budget line.
fn table(run: &TeamRun) -> String {
    let mut lines = monitor::render(&monitor::rows(run.graph(), |n| run.record(n)));
    let (spent, limit, unreported) = run.budget();
    lines.push(match limit {
        Some(l) => format!("{spent} of {l} budget tokens reported spent"),
        None => format!("{spent} tokens reported spent"),
    });
    if unreported > 0 {
        lines.push(format!("{unreported} call(s) reported no usage"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_core::team::{NodeKind, NodeSpec, TaskGraph};
    use se_core::ApprovalRunner;
    use std::time::{Duration, Instant};

    fn launcher() -> Launcher {
        Box::new(|_: &Path, _| {
            let graph = TaskGraph::new(vec![NodeSpec {
                id: "a".into(),
                agent: String::new(),
                request: "x".into(),
                deps: vec![],
                model: None,
                kind: NodeKind::Work,
            }])
            .map_err(|e| e.to_string())?;
            Ok((
                TeamRun::start(graph, ApprovalRunner { tool: "write" }, 1),
                Vec::new(),
            ))
        })
    }

    fn notices(state: &AppState) -> String {
        state
            .transcript
            .iter()
            .filter_map(|e| match e {
                Entry::Notice(n) => Some(n.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_team_approval_is_named_queued_answered_and_the_run_reported() {
        let mut state = AppState::new();
        let mut panel = Panel::new(Some(launcher()));
        panel.command(&mut state, "run team.yaml");
        let deadline = Instant::now() + Duration::from_secs(10);
        while panel.approvals.is_empty() {
            assert!(Instant::now() < deadline, "{}", notices(&state));
            panel.tick(&mut state);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(notices(&state).contains("[task a | auto |"));
        panel.command(&mut state, "allow");
        while !panel.finished {
            assert!(Instant::now() < deadline, "{}", notices(&state));
            panel.tick(&mut state);
            std::thread::sleep(Duration::from_millis(5));
        }
        let all = notices(&state);
        assert!(all.contains("team [a] Completed"), "{all}");
        assert!(all.contains("tokens"), "monitor table shown: {all}");
        // The strip got the same run as agent events, activity included.
        let a = &state.agents.agents;
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].id.as_str(), a[0].state), ("a", se_protocol::AgentState::Done));
        assert!(a[0].ended.is_some());
        assert!(
            a[0].transcript.iter().any(|e| matches!(e, Entry::Approval { .. })),
            "{:?}",
            a[0].transcript
        );
    }

    #[test]
    fn without_a_launcher_or_a_run_it_says_so() {
        let mut state = AppState::new();
        let mut panel = Panel::new(None);
        panel.command(&mut state, "");
        panel.command(&mut state, "run x.yaml");
        panel.command(&mut state, "allow");
        let all = notices(&state);
        assert!(all.contains("no team run"));
        assert!(all.contains("need a configured provider"));
        assert!(all.contains("no team approval is waiting"));
    }
}
