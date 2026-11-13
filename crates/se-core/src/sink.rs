//! The handle a running turn uses to report what it is doing.
//!
//! A [`Runner`](crate::Runner) never touches a channel, an id or the
//! sequence counter directly; it is handed a `TurnSink` and reports
//! through it. That keeps three things true without each runner having
//! to remember them:
//!
//! * every event is correlated and sequenced (I6);
//! * cancellation is checked cooperatively at points the runner chooses,
//!   rather than a thread being killed mid-write;
//! * asking for approval parks only this worker thread, never the
//!   interface (I1).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use se_protocol::{
    CoreEvent, DeltaKind, Payload, SeqCounter, SessionId, Span, Stage, TaskId, ToolCallId, TurnId,
    Usage,
};

/// Why a send failed. Only ever because the interface is gone, which
/// means the process is shutting down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disconnected;

/// Reporting channel for one task.
pub struct TurnSink {
    session: SessionId,
    task: TaskId,
    turn: Option<TurnId>,
    seq: Arc<SeqCounter>,
    events: SyncSender<CoreEvent>,
    cancelled: Arc<AtomicBool>,
    /// Decisions arriving back from the supervisor, one at a time.
    approvals: Receiver<bool>,
    /// Spans recorded so far, attached to the terminal event.
    pub(crate) timings: se_protocol::Timings,
    /// Set once a disconnect is seen so a runner that ignores the error
    /// cannot spin forever trying to send.
    disconnected: bool,
}

impl TurnSink {
    pub(crate) fn new(
        session: SessionId,
        task: TaskId,
        seq: Arc<SeqCounter>,
        events: SyncSender<CoreEvent>,
        cancelled: Arc<AtomicBool>,
        approvals: Receiver<bool>,
    ) -> Self {
        TurnSink {
            session,
            task,
            turn: None,
            seq,
            events,
            cancelled,
            approvals,
            timings: se_protocol::Timings::default(),
            disconnected: false,
        }
    }

    pub fn session(&self) -> SessionId {
        self.session
    }

    pub fn task(&self) -> TaskId {
        self.task
    }

    /// True once the user has asked to stop. A runner should check this
    /// between steps and while waiting on anything slow; the task is
    /// still expected to return an [`Outcome`](se_protocol::Outcome), so
    /// cancellation stays a cooperative unwind rather than a kill (I5).
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// A standalone view of the cancellation flag.
    ///
    /// A runner that hands a "should I stop?" callback to something else
    /// (a provider streaming over a socket) needs to ask about
    /// cancellation while also holding the sink mutably to report
    /// deltas. Cloning the flag keeps both possible without interior
    /// mutability or a lock on the hot path.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub(crate) fn emit(&mut self, payload: Payload) -> Result<(), Disconnected> {
        if self.disconnected {
            return Err(Disconnected);
        }
        let event = CoreEvent {
            seq: self.seq.next(),
            session: self.session,
            task: self.task,
            turn: self.turn,
            payload,
        };
        // A full queue blocks this worker, which is the backpressure
        // invariant I3 doing its job: the model is made to wait for the
        // interface, never the other way round.
        match self.events.send(event) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.disconnected = true;
                Err(Disconnected)
            }
        }
    }

    /// Opens a turn. Subsequent events carry its id until the next call.
    pub fn start_turn(&mut self, provider: &str, model: &str) -> Result<TurnId, Disconnected> {
        let turn = TurnId::next();
        self.turn = Some(turn);
        self.emit(Payload::TurnStarted {
            provider: provider.to_string(),
            model: model.to_string(),
        })?;
        Ok(turn)
    }

    pub fn finish_turn(&mut self) -> Result<(), Disconnected> {
        if let Some(turn) = self.turn {
            self.emit(Payload::TurnCompleted { turn })?;
        }
        self.turn = None;
        Ok(())
    }

    pub fn text_delta(&mut self, text: &str) -> Result<(), Disconnected> {
        self.emit(Payload::Delta {
            kind: DeltaKind::Text,
            text: text.to_string(),
        })
    }

    pub fn reasoning_delta(&mut self, text: &str) -> Result<(), Disconnected> {
        self.emit(Payload::Delta {
            kind: DeltaKind::Reasoning,
            text: text.to_string(),
        })
    }

    pub fn usage(&mut self, usage: Usage) -> Result<(), Disconnected> {
        self.emit(Payload::UsageUpdated(usage))
    }

    pub fn tool_requested(
        &mut self,
        name: &str,
        summary: &str,
    ) -> Result<ToolCallId, Disconnected> {
        let call = ToolCallId::next();
        self.emit(Payload::ToolRequested {
            call,
            name: name.to_string(),
            summary: summary.to_string(),
        })?;
        Ok(call)
    }

    pub fn tool_completed(
        &mut self,
        call: ToolCallId,
        name: &str,
        ok: bool,
        preview: &str,
    ) -> Result<(), Disconnected> {
        self.emit(Payload::ToolCompleted {
            call,
            name: name.to_string(),
            ok,
            preview: preview.to_string(),
        })
    }

    /// Asks the user to approve a tool call and waits for the answer.
    ///
    /// Parks this worker thread only. The interface thread stays in its
    /// own loop, drawing and reading input, which is the difference from
    /// the previous implementation's nested `event::read()`.
    ///
    /// Returns `false` if the task was cancelled while waiting or the
    /// interface disappeared: a withheld decision is never treated as
    /// permission.
    pub fn request_approval(&mut self, name: &str, summary: &str) -> bool {
        let call = ToolCallId::next();
        if self
            .emit(Payload::ApprovalRequired {
                call,
                name: name.to_string(),
                summary: summary.to_string(),
            })
            .is_err()
        {
            return false;
        }
        let waiting = se_protocol::Running::start(Stage::ApprovalWait);
        let decision = self.approvals.recv().unwrap_or(false);
        let span = waiting.finish();
        self.timings.record(span);
        let allowed = decision && !self.is_cancelled();
        if !allowed {
            let _ = self.emit(Payload::ToolDenied {
                call,
                name: name.to_string(),
            });
        }
        allowed
    }

    /// Records a completed measurement against this task.
    pub fn record(&mut self, span: Span) {
        self.timings.record(span);
    }

    /// Times a closure and records it. The common case, so it does not
    /// need three lines at every call site.
    pub fn timed<T>(&mut self, stage: Stage, f: impl FnOnce() -> T) -> T {
        let running = se_protocol::Running::start(stage);
        let out = f();
        self.timings.record(running.finish());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    fn sink() -> (
        TurnSink,
        Receiver<CoreEvent>,
        SyncSender<bool>,
        Arc<AtomicBool>,
    ) {
        let (event_tx, event_rx) = sync_channel(64);
        let (approval_tx, approval_rx) = sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let sink = TurnSink::new(
            SessionId::from_raw(1),
            TaskId::from_raw(1),
            Arc::new(SeqCounter::new()),
            event_tx,
            cancelled.clone(),
            approval_rx,
        );
        (sink, event_rx, approval_tx, cancelled)
    }

    #[test]
    fn events_are_sequenced_and_carry_their_correlation() {
        let (mut sink, rx, _a, _c) = sink();
        sink.text_delta("one").unwrap();
        sink.text_delta("two").unwrap();

        let first = rx.recv().unwrap();
        let second = rx.recv().unwrap();
        assert_eq!(first.seq, 0);
        assert_eq!(second.seq, 1);
        assert_eq!(first.session, SessionId::from_raw(1));
        assert_eq!(first.task, TaskId::from_raw(1));
    }

    #[test]
    fn a_turn_id_is_attached_only_between_start_and_finish() {
        let (mut sink, rx, _a, _c) = sink();
        sink.text_delta("before").unwrap();
        let turn = sink.start_turn("mock", "mock-1").unwrap();
        sink.text_delta("inside").unwrap();
        sink.finish_turn().unwrap();
        sink.text_delta("after").unwrap();

        let before = rx.recv().unwrap();
        let started = rx.recv().unwrap();
        let inside = rx.recv().unwrap();
        let completed = rx.recv().unwrap();
        let after = rx.recv().unwrap();

        assert_eq!(before.turn, None);
        assert_eq!(started.turn, Some(turn));
        assert_eq!(inside.turn, Some(turn));
        assert_eq!(completed.turn, Some(turn));
        assert_eq!(after.turn, None);
    }

    #[test]
    fn approval_blocks_the_worker_and_returns_the_decision() {
        let (mut sink, rx, approvals, _c) = sink();
        let answered = std::thread::spawn(move || {
            // Mirrors the supervisor: see the request, then answer.
            let request = rx.recv().unwrap();
            assert!(matches!(request.payload, Payload::ApprovalRequired { .. }));
            approvals.send(true).unwrap();
            rx
        });
        assert!(sink.request_approval("run_shell", "cargo test"));
        let _rx = answered.join().unwrap();
    }

    #[test]
    fn a_denied_call_is_reported_as_denied() {
        let (mut sink, rx, approvals, _c) = sink();
        let joined = std::thread::spawn(move || {
            let _request = rx.recv().unwrap();
            approvals.send(false).unwrap();
            rx
        });
        assert!(!sink.request_approval("write_file", "src/main.rs"));
        let rx = joined.join().unwrap();
        let denied = rx.recv().unwrap();
        assert!(matches!(denied.payload, Payload::ToolDenied { .. }));
    }

    #[test]
    fn cancellation_while_waiting_is_never_read_as_permission() {
        let (mut sink, rx, approvals, cancelled) = sink();
        let joined = std::thread::spawn(move || {
            let _request = rx.recv().unwrap();
            // The supervisor's interrupt path: raise the flag, then
            // unpark the waiter. Even though this says `true`, the
            // cancellation must win.
            cancelled.store(true, Ordering::SeqCst);
            approvals.send(true).unwrap();
            rx
        });
        assert!(
            !sink.request_approval("run_shell", "rm -rf /"),
            "a cancelled task must not act on a stale approval"
        );
        let _rx = joined.join().unwrap();
    }

    #[test]
    fn a_withheld_decision_denies_rather_than_allows() {
        let (mut sink, rx, approvals, _c) = sink();
        // The interface goes away without answering.
        drop(approvals);
        assert!(!sink.request_approval("run_shell", "anything"));
        drop(rx);
    }

    #[test]
    fn time_spent_waiting_on_a_human_is_recorded_as_such() {
        let (mut sink, rx, approvals, _c) = sink();
        let joined = std::thread::spawn(move || {
            let _ = rx.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));
            approvals.send(true).unwrap();
            rx
        });
        sink.request_approval("run_shell", "ls");
        let _rx = joined.join().unwrap();

        let waited = sink.timings.of(Stage::ApprovalWait).expect("recorded");
        assert!(waited >= std::time::Duration::from_millis(20));
        // And it must not be counted as SE being slow.
        assert_eq!(sink.timings.local(), std::time::Duration::ZERO);
    }

    #[test]
    fn a_disconnected_interface_stops_further_sends_instead_of_spinning() {
        let (mut sink, rx, _a, _c) = sink();
        drop(rx);
        assert_eq!(sink.text_delta("x"), Err(Disconnected));
        // Still an error, and it short-circuits without touching the
        // channel again.
        assert_eq!(sink.text_delta("y"), Err(Disconnected));
    }
}
