//! The vocabulary SE's core and its interfaces share.
//!
//! This crate is the boundary that the previous implementation did not
//! have. There, the terminal called the agent loop directly and received
//! progress through a `&mut dyn FnMut(&AgentEvent)` callback invoked on
//! the UI thread, which is why a model call froze the interface and why
//! approval had to run a nested event loop to get a keypress.
//!
//! Here the relationship is inverted and typed: an interface sends
//! [`CoreOp`] and receives [`CoreEvent`]. Neither side holds a reference
//! to the other. That is what makes invariants I1 and I2 structural
//! rather than something each new call site has to remember.
//!
//! This crate deliberately contains no policy and no I/O. It depends on
//! `serde` and the standard library, nothing more, so it stays cheap to
//! compile and safe for every layer to depend on.

pub mod ids;
pub mod timings;

pub use ids::{SeqCounter, SessionId, TaskId, ToolCallId, TurnId};
pub use timings::{Running, Span, Stage, Timings};

use serde::{Deserialize, Serialize};

/// What the user asked for, as handed to the core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTurn {
    pub session: SessionId,
    /// Allocated by the caller, so the identity it was told about and
    /// the identity the core runs under are the same one. Two separate
    /// allocations meant an interface could hold an id that no event
    /// ever referred to.
    pub task: TaskId,
    pub text: String,
    /// Files the user attached. Paths only: the core decides what is
    /// readable and how much of it to include, because that is a policy
    /// and context-budget decision, not a UI one.
    #[serde(default)]
    pub attachments: Vec<std::path::PathBuf>,
    /// An explicit per-turn model override -- routing precedence level
    /// 1 in charter §6.6 ("explicit task override"), and the only level
    /// that travels with the task rather than living in a profile. Any
    /// other level a future routing phase adds (specialist, team,
    /// department, global default) is resolved elsewhere, against
    /// whatever profile store exists then; this field exists so a task
    /// can carry an override that nothing else can.
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// The catalogue id of the specialist this turn is routed through,
    /// if any. Deliberately just the bare id, not the resolved
    /// definition: setting this costs nothing on the interface thread
    /// that builds a `UserTurn`. Resolving it (body, content hash)
    /// happens later, inside `Runner::run` on the worker thread, which
    /// is where this workspace's provider calls already do blocking
    /// I/O -- not here, where it would block the render/input loop.
    #[serde(default)]
    pub specialist_id: Option<String>,
    /// The team-run graph node this turn executes, if any. Set only by a
    /// team scheduler: such a turn is never the user's own choice, so a
    /// non-default vendor goes through the gateway even when no
    /// specialist is named (it will be picked automatically).
    #[serde(default)]
    pub team_node: Option<String>,
}

/// A specific provider and model, named rather than inferred. The same
/// two strings [`Payload::TurnStarted`] already reports for what
/// actually ran -- this type exists so routing code has one shared
/// vocabulary for "which model" instead of two loose `String`s per call
/// site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

/// A decision about a tool call that needed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecision {
    /// The task that asked. The supervisor routes on this (E8); the
    /// worker still checks `call` so a stale decision is never applied.
    pub task: TaskId,
    pub call: ToolCallId,
    pub allow: bool,
}

/// Input to the core.
///
/// Note what is absent: there is no "render" or "redraw" operation. The
/// core never asks an interface to do anything; it only reports. An
/// interface that wants to show something derives it from state it built
/// out of events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoreOp {
    /// Begin work on a user request.
    SubmitUserTurn(UserTurn),
    /// Cooperative cancellation of a running task. The task still
    /// reports a terminal event (I5); this is not a kill.
    Interrupt { task: TaskId },
    /// Answer a pending [`CoreEvent::ApprovalRequired`].
    Approve(ApprovalDecision),
    /// Set or (`None`) clear the specialist unnamed turns stay with.
    SetSpecialist(Option<String>),
    /// Switch how tool calls are handled from the next call on.
    SetMode(WorkMode),
    /// Stop accepting work and shut the core down.
    Shutdown,
}

/// How a turn's tool calls are handled. A policy `deny` rule and path
/// confinement apply in every mode; the mode only changes what happens
/// to a call the policy leaves to a human.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkMode {
    /// Every tool call the policy leaves open (reads included) and every
    /// agent spawn asks the user first.
    #[default]
    Manual,
    /// Calls the policy would ask about run without asking.
    Auto,
    /// Read and search only, run without asking: nothing in the project is changed.
    Plan,
}

impl WorkMode {
    pub const ALL: [WorkMode; 3] = [WorkMode::Manual, WorkMode::Auto, WorkMode::Plan];

    pub fn name(self) -> &'static str {
        match self {
            WorkMode::Manual => "manual",
            WorkMode::Auto => "auto",
            WorkMode::Plan => "plan",
        }
    }

    pub fn parse(s: &str) -> Option<WorkMode> {
        WorkMode::ALL.into_iter().find(|m| m.name() == s.trim())
    }

    /// The tools a plan-mode turn may use; everything else changes the project.
    pub fn allows(self, tool: &str) -> bool {
        self != WorkMode::Plan || matches!(tool, "read" | "search" | "agent")
    }
}

/// A chunk of model output, classified.
///
/// Reasoning is kept distinct from the answer because they are not
/// interchangeable to a reader, and because some providers bill and
/// expose them separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// Token accounting, as reported by whoever actually knows.
///
/// Every field is optional and the source is always recorded, because
/// the honest answer is frequently "the provider did not say". The
/// previous implementation displayed a confident `$0.00` next to a token
/// count it had measured locally, which is how a user ends up trusting a
/// number nobody produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// All input, cache reads included (`cached_input_tokens` is a part of it).
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub source: UsageSource,
}

/// Where a usage figure came from. Not cosmetic: an estimate must never
/// be rendered as though the provider reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UsageSource {
    /// The provider's own response body said so.
    ProviderReported,
    /// SE measured or estimated it locally. Must be labelled as such
    /// wherever it is shown.
    LocallyMeasured,
    /// Nobody reported it. Shown as "not reported", never as zero.
    #[default]
    Unavailable,
}

impl UsageSource {
    pub fn label(self) -> &'static str {
        match self {
            UsageSource::ProviderReported => "provider-reported",
            UsageSource::LocallyMeasured => "locally measured",
            UsageSource::Unavailable => "not reported",
        }
    }
}

/// How a task ended. Exactly one of these reaches the interface per
/// task (I5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// The model produced a final answer.
    Completed { final_text: String },
    /// The user interrupted it. Distinct from failure: nothing is wrong.
    /// Takes precedence over every other outcome -- a user-requested
    /// stop is never relabelled as a truncation or a failure, even if
    /// partial text exists.
    Cancelled,
    /// The turn limit was reached without a final answer. Reported
    /// honestly rather than presenting the last partial output as an
    /// answer.
    ExhaustedTurns { turns: u32 },
    /// A real answer started but did not finish: the provider was
    /// interrupted by something other than the user (truncation at an
    /// output limit, a mid-stream error, a dropped connection, a
    /// whole-turn deadline). `partial_text` is exactly what streamed
    /// before the interruption -- never padded, never silently
    /// discarded. Distinct from `Failed` specifically so a view can
    /// show "here is what we got, and why it stopped" rather than
    /// losing the partial answer or, worse, presenting it as complete.
    Incomplete {
        partial_text: String,
        reason: String,
    },
    /// Something failed with no usable partial answer. `detail` is safe
    /// to display: it never carries a credential (I8).
    Failed { detail: String },
}

impl Outcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Outcome::Completed { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Completed { .. } => "completed",
            Outcome::Cancelled => "cancelled",
            Outcome::ExhaustedTurns { .. } => "exhausted",
            Outcome::Incomplete { .. } => "incomplete",
            Outcome::Failed { .. } => "failed",
        }
    }
}

/// What the core produced. Carries its own correlation (I6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreEvent {
    pub seq: u64,
    pub session: SessionId,
    pub task: TaskId,
    pub turn: Option<TurnId>,
    pub payload: Payload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Payload {
    TaskStarted {
        /// Echoed back so an interface can render the user's own message
        /// from the event stream rather than keeping a private copy that
        /// could disagree with it.
        request: String,
    },
    TurnStarted {
        provider: String,
        model: String,
    },
    /// Emitted once, right after `TurnStarted`, only when the turn is
    /// routed through a specialist. `content_hash` is the catalogue
    /// definition's hash at the moment it was resolved for this turn --
    /// the invocation-provenance record Phase 3 item 4 asks for. No
    /// separate storage: this flows through the same session log every
    /// other `CoreEvent` already does, so it is durable and inspectable
    /// for free.
    SpecialistInvoked {
        agent_id: String,
        content_hash: u64,
    },
    /// Streamed output. High-frequency: an interface may coalesce these
    /// for rendering (I4 permits it) but must not reorder them.
    Delta {
        kind: DeltaKind,
        text: String,
    },
    ToolRequested {
        call: ToolCallId,
        name: String,
        /// Already redacted and truncated by the core for display.
        summary: String,
    },
    /// The core is blocked until an [`CoreOp::Approve`] arrives. Never
    /// coalesced or dropped (I4).
    ApprovalRequired {
        call: ToolCallId,
        name: String,
        summary: String,
    },
    ToolCompleted {
        call: ToolCallId,
        name: String,
        /// Whether the tool itself reported failure. Separate from
        /// whether SE managed to run it.
        ok: bool,
        /// Captured output, truncated for display. The untruncated
        /// result belongs in the evidence record, not in an event.
        preview: String,
    },
    ToolDenied {
        call: ToolCallId,
        name: String,
    },
    UsageUpdated(Usage),
    TurnCompleted {
        turn: TurnId,
    },
    /// Terminal. Nothing further arrives for this task.
    TaskFinished {
        outcome: Outcome,
        timings: Timings,
    },
    /// A deployed agent's lifecycle or activity. Wrapped, so a consumer
    /// that ignores it can never mistake a child's text for its own.
    Agent(AgentEvent),
}

/// Where a deployed agent (a team node today, a child agent later) is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Running,
    Background,
    WaitingApproval,
    Done,
    Failed,
    Cancelled,
}

impl AgentState {
    pub fn label(self) -> &'static str {
        match self {
            AgentState::Running => "running",
            AgentState::Background => "background",
            AgentState::WaitingApproval => "waiting approval",
            AgentState::Done => "done",
            AgentState::Failed => "failed",
            AgentState::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// Agent lifecycle events. `agent_id` is unique within one run; `parent`
/// is the spawning agent's id (`None`: the main agent or a team run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentEvent {
    Spawned {
        agent_id: String,
        role: String,
        parent: Option<String>,
        /// One-line summary of what it was asked to do.
        task: String,
    },
    State {
        agent_id: String,
        state: AgentState,
    },
    /// Reported tokens so far (input + output), a running total.
    Tokens {
        agent_id: String,
        used: u64,
    },
    /// Terminal: implies state `Done`.
    Done {
        agent_id: String,
        summary: String,
    },
    /// Terminal: implies state `Failed`.
    Failed {
        agent_id: String,
        error: String,
    },
    /// One of the agent's own events (`Delta`, `Tool*`, `ApprovalRequired`)
    /// for its transcript view.
    Activity {
        agent_id: String,
        payload: Box<Payload>,
    },
}

impl CoreEvent {
    /// Whether this event may be dropped or merged when an interface is
    /// behind. Invariant I4: visual noise may be coalesced, meaning may
    /// not.
    pub fn is_coalescable(&self) -> bool {
        matches!(self.payload, Payload::Delta { .. })
    }

    /// Whether this is the last event for its task.
    pub fn is_terminal(&self) -> bool {
        matches!(self.payload, Payload::TaskFinished { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(payload: Payload) -> CoreEvent {
        CoreEvent {
            seq: 0,
            session: SessionId::from_raw(1),
            task: TaskId::from_raw(1),
            turn: None,
            payload,
        }
    }

    #[test]
    fn only_deltas_may_be_coalesced() {
        assert!(event(Payload::Delta {
            kind: DeltaKind::Text,
            text: "hi".into()
        })
        .is_coalescable());

        // The ones that carry meaning rather than appearance.
        assert!(!event(Payload::ApprovalRequired {
            call: ToolCallId::from_raw(1),
            name: "run_shell".into(),
            summary: "rm -rf".into()
        })
        .is_coalescable());
        assert!(!event(Payload::TaskFinished {
            outcome: Outcome::Cancelled,
            timings: Timings::default()
        })
        .is_coalescable());
        assert!(!event(Payload::TurnCompleted {
            turn: TurnId::from_raw(1)
        })
        .is_coalescable());
    }

    #[test]
    fn exactly_the_finish_event_is_terminal() {
        assert!(event(Payload::TaskFinished {
            outcome: Outcome::Completed {
                final_text: "done".into()
            },
            timings: Timings::default()
        })
        .is_terminal());
        assert!(!event(Payload::TurnCompleted {
            turn: TurnId::from_raw(1)
        })
        .is_terminal());
    }

    #[test]
    fn unreported_usage_defaults_to_unavailable_not_zero() {
        // The distinction the previous monitor lost: "nobody told us"
        // must not render as a confident zero.
        let usage = Usage::default();
        assert_eq!(usage.source, UsageSource::Unavailable);
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(UsageSource::Unavailable.label(), "not reported");
    }

    #[test]
    fn cancellation_is_not_a_failure() {
        assert!(!Outcome::Cancelled.is_success());
        assert_eq!(Outcome::Cancelled.label(), "cancelled");
        // Exhausting the turn budget is also not success, and is
        // reported distinctly from an error.
        assert_eq!(Outcome::ExhaustedTurns { turns: 8 }.label(), "exhausted");
        assert!(!Outcome::ExhaustedTurns { turns: 8 }.is_success());
    }

    #[test]
    fn the_protocol_round_trips_through_serde() {
        // Events are persisted and replayed (Phase 4), so the wire shape
        // has to survive a round trip from the start.
        let original = event(Payload::ToolCompleted {
            call: ToolCallId::from_raw(3),
            name: "read_file".into(),
            ok: true,
            preview: "fn main() {}".into(),
        });
        let json = serde_json::to_string(&original).unwrap();
        let back: CoreEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(original, back);

        let op = CoreOp::SubmitUserTurn(UserTurn {
            session: SessionId::from_raw(1),
            task: TaskId::from_raw(1),
            text: "hi".into(),
            attachments: vec![],
            model: None,
            specialist_id: None,
            team_node: None,
        });
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(op, serde_json::from_str::<CoreOp>(&json).unwrap());
    }

    #[test]
    fn a_user_turn_with_a_model_override_round_trips() {
        let op = CoreOp::SubmitUserTurn(UserTurn {
            session: SessionId::from_raw(1),
            task: TaskId::from_raw(1),
            text: "hi".into(),
            attachments: vec![],
            model: Some(ModelRef {
                provider: "anthropic".into(),
                model: "sonnet".into(),
            }),
            specialist_id: None,
            team_node: None,
        });
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(op, serde_json::from_str::<CoreOp>(&json).unwrap());
    }

    #[test]
    fn an_absent_model_override_deserialises_from_json_with_no_model_field_at_all() {
        // #[serde(default)] matters here: a UserTurn serialised before
        // this field existed must still deserialise, not fail to parse
        // a durable session recorded by an older build.
        let json = r#"{"session":1,"task":1,"text":"hi","attachments":[]}"#;
        let turn: UserTurn = serde_json::from_str(json).unwrap();
        assert_eq!(turn.model, None);
    }

    #[test]
    fn agent_events_round_trip_and_old_logs_still_load() {
        let id = || "review".to_string();
        for p in [
            AgentEvent::Spawned {
                agent_id: id(),
                role: "auto".into(),
                parent: Some("main".into()),
                task: "check it".into(),
            },
            AgentEvent::State {
                agent_id: id(),
                state: AgentState::WaitingApproval,
            },
            AgentEvent::Tokens {
                agent_id: id(),
                used: 42,
            },
            AgentEvent::Done {
                agent_id: id(),
                summary: "ok".into(),
            },
            AgentEvent::Failed {
                agent_id: id(),
                error: "no".into(),
            },
            AgentEvent::Activity {
                agent_id: id(),
                payload: Box::new(Payload::Delta {
                    kind: DeltaKind::Text,
                    text: "hi".into(),
                }),
            },
        ] {
            let original = event(Payload::Agent(p));
            let json = serde_json::to_string(&original).unwrap();
            assert_eq!(original, serde_json::from_str::<CoreEvent>(&json).unwrap());
        }
        // A line written before agent events existed parses unchanged.
        let old = r#"{"seq":4,"session":1,"task":1,"turn":null,"payload":{"Delta":{"kind":"Text","text":"hi"}}}"#;
        let back: CoreEvent = serde_json::from_str(old).unwrap();
        assert_eq!(
            back.payload,
            Payload::Delta {
                kind: DeltaKind::Text,
                text: "hi".into()
            }
        );
    }

    #[test]
    fn there_is_no_operation_that_tells_an_interface_to_render() {
        // A structural check of invariant I2, written as a test so that
        // adding such an operation has to argue with this comment:
        // the core reports, it never drives a view.
        let json = serde_json::to_string(&CoreOp::Shutdown).unwrap();
        assert_eq!(json, "\"Shutdown\"");
    }
}
