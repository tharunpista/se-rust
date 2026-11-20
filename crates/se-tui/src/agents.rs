//! Deployed agents: the strip below the composer and each agent's
//! transcript view. Folded only from [`AgentEvent`]s, whether they come
//! from a `/team run` (via the team panel) or, later, from the main core
//! (`Payload::Agent`), so there is one state model for both.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use se_protocol::{AgentEvent, AgentState, DeltaKind, Payload};

use crate::input::{apply_local, Action};
use crate::state::{AppState, Entry, ToolState};

/// More agents than this show as one summary line until the strip is focused.
pub const COLLAPSE_ABOVE: usize = 4;
/// Most rows the expanded strip takes; it scrolls with the selection.
pub const MAX_ROWS: usize = 8;

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: String,
    pub role: String,
    pub parent: Option<String>,
    pub task: String,
    pub state: AgentState,
    pub tokens: Option<u64>,
    pub started: Instant,
    pub ended: Option<Instant>,
    /// Its own text and tool calls, for the transcript view.
    pub transcript: Vec<Entry>,
}

impl Agent {
    pub fn elapsed(&self, now: Instant) -> std::time::Duration {
        self.ended
            .unwrap_or(now)
            .saturating_duration_since(self.started)
    }
}

/// Where the keyboard is: the composer, a strip row, or an agent's view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Composer,
    Strip(usize),
    View(usize),
}

#[derive(Debug, Clone, Default)]
pub struct Strip {
    pub agents: Vec<Agent>,
    pub focus: Focus,
    /// The main transcript's scroll while a view borrows the scroll keys.
    saved_scroll: u16,
    /// Screen rows the strip drew last frame: (y, agent index; `None` is
    /// the collapsed summary line). For mouse clicks.
    pub hit: Vec<(u16, Option<usize>)>,
    /// Second of elapsed time last drawn, so a running agent's clock ticks.
    last_second: Option<u64>,
}

impl Strip {
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    pub fn collapsed(&self) -> bool {
        self.agents.len() > COLLAPSE_ABOVE && self.focus == Focus::Composer
    }

    /// The highlighted row, in the strip or behind an open view.
    pub fn selected(&self) -> Option<usize> {
        match self.focus {
            Focus::Strip(i) | Focus::View(i) => Some(i),
            Focus::Composer => None,
        }
    }

    pub fn viewing(&self) -> Option<usize> {
        match self.focus {
            Focus::View(i) if i < self.agents.len() => Some(i),
            _ => None,
        }
    }

    /// Whether a running agent's elapsed clock moved to a new second.
    pub fn clock_moved(&mut self, now: Instant) -> bool {
        let second = self
            .agents
            .iter()
            .filter(|a| a.ended.is_none())
            .map(|a| a.elapsed(now).as_secs())
            .max();
        second.is_some() && std::mem::replace(&mut self.last_second, second) != second
    }

    pub fn apply(&mut self, event: &AgentEvent) {
        let now = Instant::now();
        let id = match event {
            AgentEvent::Spawned {
                agent_id,
                role,
                parent,
                task,
            } => {
                if !self.agents.iter().any(|a| &a.id == agent_id) {
                    self.agents.push(Agent {
                        id: agent_id.clone(),
                        role: role.clone(),
                        parent: parent.clone(),
                        task: task.clone(),
                        state: AgentState::Running,
                        tokens: None,
                        started: now,
                        ended: None,
                        transcript: vec![Entry::Notice(format!("{role} [{agent_id}]: {task}"))],
                    });
                }
                return;
            }
            AgentEvent::State { agent_id, .. }
            | AgentEvent::Tokens { agent_id, .. }
            | AgentEvent::Done { agent_id, .. }
            | AgentEvent::Failed { agent_id, .. }
            | AgentEvent::Activity { agent_id, .. } => agent_id,
        };
        // An event for an agent never spawned has nowhere to go.
        let Some(agent) = self.agents.iter_mut().find(|a| &a.id == id) else {
            return;
        };
        let set = |agent: &mut Agent, state: AgentState| {
            agent.state = state;
            agent.ended = state.is_terminal().then(|| agent.ended.unwrap_or(now));
        };
        match event {
            AgentEvent::Spawned { .. } => {}
            AgentEvent::State { state, .. } => set(agent, *state),
            AgentEvent::Tokens { used, .. } => agent.tokens = Some(*used),
            AgentEvent::Done { summary, .. } => {
                set(agent, AgentState::Done);
                agent
                    .transcript
                    .push(Entry::Notice(format!("done: {summary}")));
            }
            AgentEvent::Failed { error, .. } => {
                set(agent, AgentState::Failed);
                agent.transcript.push(Entry::Error(error.clone()));
            }
            AgentEvent::Activity { payload, .. } => fold(&mut agent.transcript, payload),
        }
    }
}

/// An agent's own event into its transcript: text and tool calls only.
fn fold(t: &mut Vec<Entry>, payload: &Payload) {
    match payload {
        Payload::Delta { kind, text } => {
            let last = t.last_mut();
            match (kind, last) {
                (DeltaKind::Text, Some(Entry::Assistant(s)))
                | (DeltaKind::Reasoning, Some(Entry::Reasoning(s))) => s.push_str(text),
                (DeltaKind::Text, _) => t.push(Entry::Assistant(text.clone())),
                (DeltaKind::Reasoning, _) => t.push(Entry::Reasoning(text.clone())),
            }
        }
        Payload::ToolRequested { name, summary, .. } => t.push(Entry::Tool {
            name: name.clone(),
            summary: summary.clone(),
            state: ToolState::Running,
        }),
        Payload::ApprovalRequired { name, summary, .. } => t.push(Entry::Approval {
            name: name.clone(),
            summary: summary.clone(),
            decision: None,
        }),
        Payload::ToolCompleted {
            name, ok, preview, ..
        } => {
            let state = if *ok {
                ToolState::Ok
            } else {
                ToolState::Failed
            };
            crate::state::settle_tool(t, name, state, preview);
        }
        Payload::ToolDenied { name, .. } => {
            crate::state::mark_approval_decided(t, false);
            crate::state::settle_tool(t, name, ToolState::Denied, "not run");
        }
        _ => {}
    }
}

/// Moves focus, lending the scroll keys to a view and handing them back.
pub fn set_focus(state: &mut AppState, focus: Focus) {
    let was_view = matches!(state.agents.focus, Focus::View(_));
    let is_view = matches!(focus, Focus::View(_));
    if is_view && !was_view {
        state.agents.saved_scroll = state.scroll;
        state.scroll = 0;
    } else if was_view && !is_view {
        state.scroll = state.agents.saved_scroll;
    }
    state.agents.focus = focus;
}

/// Whether `key` belongs to the strip or an open view right now.
pub fn wants(state: &AppState, key: &KeyEvent) -> bool {
    let nav = matches!(
        key.code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::Enter
            | KeyCode::Esc
            | KeyCode::PageUp
            | KeyCode::PageDown
    );
    match state.agents.focus {
        Focus::Composer => {
            // ↓ in an empty composer at the newest line: nothing to scroll, so
            // it moves into the strip. With a draft or scrolled back, it scrolls.
            key.code == KeyCode::Down
                && key.modifiers.is_empty()
                && state.composer.is_empty()
                && state.scroll == 0
                && !state.agents.is_empty()
        }
        _ => nav,
    }
}

/// Handles a key [`wants`] claimed.
pub fn key(state: &mut AppState, key: KeyEvent) {
    let n = state.agents.agents.len();
    if n == 0 {
        set_focus(state, Focus::Composer);
        return;
    }
    match (state.agents.focus, key.code) {
        (Focus::Composer, _) => set_focus(state, Focus::Strip(0)),
        (Focus::Strip(0), KeyCode::Up) | (Focus::Strip(_), KeyCode::Esc) => {
            set_focus(state, Focus::Composer)
        }
        (Focus::Strip(i), KeyCode::Up) => set_focus(state, Focus::Strip(i.min(n) - 1)),
        (Focus::Strip(i), KeyCode::Down) => set_focus(state, Focus::Strip((i + 1).min(n - 1))),
        (Focus::Strip(i), KeyCode::Enter) => set_focus(state, Focus::View(i.min(n - 1))),
        (Focus::View(i), KeyCode::Esc) => set_focus(state, Focus::Strip(i)),
        (Focus::View(_), KeyCode::Up) => apply_local(state, &Action::ScrollUp(1)),
        (Focus::View(_), KeyCode::Down) => apply_local(state, &Action::ScrollDown(1)),
        (Focus::View(_), KeyCode::PageUp) => apply_local(state, &Action::ScrollUp(10)),
        (Focus::View(_), KeyCode::PageDown) => apply_local(state, &Action::ScrollDown(10)),
        _ => {}
    }
}

/// A left click on screen row `y` (mouse capture on): a row opens that
/// agent's view; the collapsed summary line expands the strip.
pub fn click(state: &mut AppState, y: u16) {
    match state.agents.hit.iter().find(|(row, _)| *row == y) {
        Some((_, Some(i))) => {
            let i = *i;
            // Straight from one view to another keeps the main scroll saved.
            if state.agents.viewing().is_some() {
                state.agents.focus = Focus::View(i);
                state.scroll = 0;
            } else {
                set_focus(state, Focus::View(i));
            }
        }
        Some((_, None)) => set_focus(state, Focus::Strip(0)),
        None => {}
    }
}

pub fn elapsed_label(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::input::action_for_key;
    use crossterm::event::KeyModifiers;
    use se_protocol::ToolCallId;

    pub(crate) fn spawn(state: &mut AppState, ids: &[&str]) {
        for id in ids {
            state.agents.apply(&AgentEvent::Spawned {
                agent_id: id.to_string(),
                role: "auto".into(),
                parent: None,
                task: format!("task {id}"),
            });
        }
    }

    fn press(state: &mut AppState, code: KeyCode) -> Action {
        let a = action_for_key(state, KeyEvent::new(code, KeyModifiers::NONE));
        apply_local(state, &a);
        a
    }

    #[test]
    fn down_in_an_empty_composer_enters_the_strip_and_esc_walks_back_out() {
        let mut s = AppState::new();
        // No agents: ↓ scrolls as before.
        assert_eq!(press(&mut s, KeyCode::Down), Action::ScrollDown(1));
        spawn(&mut s, &["a", "b"]);
        s.scroll = 0;
        assert!(matches!(press(&mut s, KeyCode::Down), Action::AgentKey(_)));
        assert_eq!(s.agents.focus, Focus::Strip(0));
        press(&mut s, KeyCode::Down);
        press(&mut s, KeyCode::Down);
        assert_eq!(s.agents.focus, Focus::Strip(1), "stops at the last row");
        press(&mut s, KeyCode::Enter);
        assert_eq!(s.agents.focus, Focus::View(1));
        // Esc in a view goes back to the strip, not an interrupt.
        s.activity = crate::state::Activity::Working {
            task: se_protocol::TaskId::from_raw(1),
        };
        assert!(matches!(press(&mut s, KeyCode::Esc), Action::AgentKey(_)));
        assert_eq!(s.agents.focus, Focus::Strip(1));
        press(&mut s, KeyCode::Esc);
        assert_eq!(s.agents.focus, Focus::Composer);
        // With the strip left, Esc interrupts again.
        assert_eq!(press(&mut s, KeyCode::Esc), Action::Interrupt);
    }

    #[test]
    fn down_with_a_draft_or_scrolled_back_does_not_steal_focus() {
        let mut s = AppState::new();
        spawn(&mut s, &["a"]);
        apply_local(&mut s, &Action::InsertChar('x'));
        assert_eq!(press(&mut s, KeyCode::Down), Action::ScrollDown(1));
        assert_eq!(s.agents.focus, Focus::Composer);
        apply_local(&mut s, &Action::ClearComposer);
        s.scroll = 3;
        s.last_total_rows = 50;
        s.last_viewport_rows = 10;
        assert_eq!(press(&mut s, KeyCode::Down), Action::ScrollDown(1));
        assert_eq!(s.agents.focus, Focus::Composer);
    }

    #[test]
    fn a_view_borrows_the_scroll_and_typing_returns_to_the_composer() {
        let mut s = AppState::new();
        spawn(&mut s, &["a"]);
        s.agents.focus = Focus::Strip(0);
        s.scroll = 0;
        s.agents.saved_scroll = 0;
        s.last_total_rows = 100;
        s.last_viewport_rows = 10;
        press(&mut s, KeyCode::Enter);
        press(&mut s, KeyCode::PageUp);
        assert_eq!(s.scroll, 10, "PgUp scrolls the agent's transcript");
        press(&mut s, KeyCode::Esc);
        assert_eq!(s.scroll, 0, "the main transcript's position is back");
        press(&mut s, KeyCode::Enter);
        apply_local(&mut s, &Action::InsertChar('h'));
        assert_eq!(s.agents.focus, Focus::Composer);
        assert_eq!(s.composer.text(), "h");
    }

    #[test]
    fn a_main_approval_takes_focus_back_from_a_view() {
        let mut s = AppState::new();
        spawn(&mut s, &["a"]);
        s.agents.focus = Focus::View(0);
        s.apply(&se_protocol::CoreEvent {
            seq: 0,
            session: se_protocol::SessionId::from_raw(1),
            task: se_protocol::TaskId::from_raw(1),
            turn: None,
            payload: Payload::ApprovalRequired {
                call: ToolCallId::from_raw(1),
                name: "write".into(),
                summary: "x".into(),
            },
        });
        assert_eq!(s.agents.focus, Focus::Composer);
    }

    #[test]
    fn activity_and_lifecycle_fold_into_the_agent() {
        let mut s = AppState::new();
        spawn(&mut s, &["a"]);
        let act = |p: Payload| AgentEvent::Activity {
            agent_id: "a".into(),
            payload: Box::new(p),
        };
        for e in [
            act(Payload::Delta {
                kind: DeltaKind::Text,
                text: "hel".into(),
            }),
            act(Payload::Delta {
                kind: DeltaKind::Text,
                text: "lo".into(),
            }),
            act(Payload::ToolRequested {
                call: ToolCallId::from_raw(1),
                name: "read_file".into(),
                summary: "x".into(),
            }),
            act(Payload::ToolCompleted {
                call: ToolCallId::from_raw(1),
                name: "read_file".into(),
                ok: true,
                preview: "ok".into(),
            }),
            AgentEvent::Tokens {
                agent_id: "a".into(),
                used: 12,
            },
            AgentEvent::Done {
                agent_id: "a".into(),
                summary: "all good".into(),
            },
            // Unknown agent: ignored, not a panic.
            AgentEvent::Tokens {
                agent_id: "zz".into(),
                used: 1,
            },
        ] {
            // Through the main fold, the path C2 uses.
            s.apply(&se_protocol::CoreEvent {
                seq: 0,
                session: se_protocol::SessionId::from_raw(1),
                task: se_protocol::TaskId::from_raw(1),
                turn: None,
                payload: Payload::Agent(e),
            });
        }
        let a = &s.agents.agents[0];
        assert_eq!((a.state, a.tokens), (AgentState::Done, Some(12)));
        assert!(a.ended.is_some());
        assert!(a.transcript.contains(&Entry::Assistant("hello".into())));
        assert!(a.transcript.contains(&Entry::Tool {
            name: "read_file".into(),
            summary: "ok".into(),
            state: ToolState::Ok
        }));
        assert!(
            s.transcript.is_empty(),
            "nothing leaked into the main transcript"
        );
        assert_eq!(s.agents.agents.len(), 1);
    }
}
