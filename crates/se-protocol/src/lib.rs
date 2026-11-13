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
}

/// A decision about a tool call that needed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDecision {
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
    /// Stop accepting work and shut the core down.
    Shutdown,
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
        });
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(op, serde_json::from_str::<CoreOp>(&json).unwrap());
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
