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
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::Duration;

/// How often the approval wait rechecks cancellation.
///
/// This is the fix for E9: `recv()` blocked unconditionally, so a
/// dropped `UNPARK` (the supervisor's `try_send` to a full 1-slot
/// channel silently fails if a stale decision is still sitting there)
/// left the worker parked forever even though `cancelled` was already
/// true. A short poll is the cost of closing that gap; 25ms is well
/// under anything a human would perceive as a delay in Ctrl+C or
/// Shutdown taking effect.
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(25);

use se_protocol::{
    AgentEvent, AgentState, CoreEvent, DeltaKind, Payload, SeqCounter, SessionId, Span, Stage,
    TaskId, ToolCallId, TurnId, Usage,
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
    /// Decisions arriving back from the supervisor, each naming the
    /// call it answers (see `request_approval`). Shared with this task's
    /// sub-agents (`child`); holding the lock from prompt to answer is
    /// what queues their approvals one at a time.
    approvals: Arc<std::sync::Mutex<Receiver<(ToolCallId, bool)>>>,
    /// Set on a sub-agent's sink: its id and role. Its events are then
    /// wrapped as that agent's `Activity` (see [`TurnSink::child`]).
    agent: Option<(String, String)>,
    /// Spans recorded so far, attached to the terminal event.
    pub(crate) timings: se_protocol::Timings,
    /// Set once a disconnect is seen so a runner that ignores the error
    /// cannot spin forever trying to send.
    disconnected: bool,
    /// A compaction the runner asks the core to apply to the shared
    /// transcript once the turn ends: replace its first `.0` messages
    /// with `.1` (see [`TurnSink::compact_history`]).
    pub(crate) compaction: Option<(usize, Vec<se_provider::Message>)>,
}

impl TurnSink {
    /// Asks the core to replace the first `covers` messages of the
    /// conversation with `with` (a summary), so later turns, a resumed
    /// session and a switched provider all send the summary instead of
    /// the full older history. Applied only if the transcript still
    /// starts with what this turn saw.
    pub fn compact_history(&mut self, covers: usize, with: Vec<se_provider::Message>) {
        self.compaction = Some((covers, with));
    }

    pub(crate) fn new(
        session: SessionId,
        task: TaskId,
        seq: Arc<SeqCounter>,
        events: SyncSender<CoreEvent>,
        cancelled: Arc<AtomicBool>,
        approvals: Receiver<(ToolCallId, bool)>,
    ) -> Self {
        TurnSink {
            session,
            task,
            turn: None,
            seq,
            events,
            cancelled,
            approvals: Arc::new(std::sync::Mutex::new(approvals)),
            agent: None,
            timings: se_protocol::Timings::default(),
            disconnected: false,
            compaction: None,
        }
    }

    /// A sink for sub-agent `agent_id` (catalogue `role`) of this task:
    /// same session, task, sequence and channels, its own cancel flag.
    /// Its events reach the interface as `Payload::Agent(Activity)`,
    /// except an approval prompt, which is sent as an ordinary prompt
    /// naming the agent so the interface asks the human as usual.
    pub(crate) fn child(&self, agent_id: &str, role: &str, cancelled: Arc<AtomicBool>) -> TurnSink {
        TurnSink {
            session: self.session,
            task: self.task,
            turn: None,
            seq: self.seq.clone(),
            events: self.events.clone(),
            cancelled,
            approvals: self.approvals.clone(),
            agent: Some((agent_id.to_string(), role.to_string())),
            timings: se_protocol::Timings::default(),
            disconnected: false,
            compaction: None,
        }
    }

    /// `role#id` on a sub-agent's sink, for the audit trail.
    pub(crate) fn agent_tag(&self) -> Option<String> {
        self.agent.as_ref().map(|(id, role)| format!("{role}#{id}"))
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
        let Some((id, role)) = self.agent.clone() else {
            return self.send(payload);
        };
        let wrap = |payload| {
            Payload::Agent(AgentEvent::Activity {
                agent_id: id.clone(),
                payload: Box::new(payload),
            })
        };
        match payload {
            // Never nested: lifecycle events go out as they are.
            Payload::Agent(_) => self.send(payload),
            Payload::ApprovalRequired {
                call,
                name,
                summary,
            } => {
                let shown = format!("[{role} #{id}] wants to run {name}: {summary}");
                self.send(wrap(Payload::ApprovalRequired {
                    call,
                    name: name.clone(),
                    summary: summary.clone(),
                }))?;
                self.send(Payload::ApprovalRequired {
                    call,
                    name,
                    summary: shown,
                })
            }
            other => self.send(wrap(other)),
        }
    }

    fn send(&mut self, payload: Payload) -> Result<(), Disconnected> {
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

    /// Records which specialist this turn is routed through and the
    /// content hash of the exact definition that was resolved for it --
    /// the invocation-provenance record Phase 3 item 4 asks for. Call
    /// once, right after [`TurnSink::start_turn`], only on the
    /// specialist path.
    pub fn specialist_invoked(
        &mut self,
        agent_id: &str,
        content_hash: u64,
    ) -> Result<(), Disconnected> {
        self.emit(Payload::SpecialistInvoked {
            agent_id: agent_id.to_string(),
            content_hash,
        })
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

    /// Announces that `call` -- an id already minted by
    /// [`TurnSink::request_approval`] -- is now actually running.
    ///
    /// Takes the id rather than minting its own, which is the E7 fix:
    /// before this, a call was approved under one `ToolCallId` and then
    /// announced and completed under a second, unrelated one that
    /// `tool_requested` minted for itself, so nothing connected what
    /// the user approved to what actually ran. There is now exactly one
    /// id per call, created once, at the point permission is asked.
    pub fn tool_requested(
        &mut self,
        call: ToolCallId,
        name: &str,
        summary: &str,
    ) -> Result<(), Disconnected> {
        self.emit(Payload::ToolRequested {
            call,
            name: name.to_string(),
            summary: summary.to_string(),
        })
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
    /// Returns the call's id on approval -- the same id to pass to
    /// [`TurnSink::tool_requested`] and [`TurnSink::tool_completed`], so
    /// the approved call and the executed call are structurally one
    /// thing, not two ids a caller must remember to keep in sync.
    /// Returns `None` if denied, if the task was cancelled while
    /// waiting, or if the interface disappeared: a withheld decision is
    /// never treated as permission.
    pub fn request_approval(&mut self, name: &str, summary: &str) -> Option<ToolCallId> {
        let id = self.agent.as_ref().map(|(id, _)| id.clone());
        let state = |agent_id: &String, state| {
            Payload::Agent(AgentEvent::State {
                agent_id: agent_id.clone(),
                state,
            })
        };
        if let Some(id) = &id {
            let _ = self.emit(state(id, AgentState::WaitingApproval));
        }
        // One prompt at a time per task: a second agent waits here.
        let approvals = self.approvals.clone();
        let queue = approvals.lock().unwrap_or_else(|e| e.into_inner());
        // Cancelled while queued: never show a prompt nobody should answer.
        let decided = if self.is_cancelled() {
            None
        } else {
            self.announce_approval(name, summary)
                .and_then(|call| self.wait(&queue, call, name))
        };
        drop(queue);
        if let Some(id) = &id {
            if !self.is_cancelled() {
                let _ = self.emit(state(id, AgentState::Running));
            }
        }
        decided
    }

    /// First half of [`request_approval`](Self::request_approval): mints
    /// the call id and tells the interface. Split out so a test can
    /// deliver replies *before* the worker parks (row 4.9).
    pub(crate) fn announce_approval(&mut self, name: &str, summary: &str) -> Option<ToolCallId> {
        let call = ToolCallId::next();
        self.emit(Payload::ApprovalRequired {
            call,
            name: name.to_string(),
            summary: summary.to_string(),
        })
        .ok()?;
        Some(call)
    }

    /// Second half: waits for the decision naming `call`.
    #[cfg(test)]
    pub(crate) fn await_decision(&mut self, call: ToolCallId, name: &str) -> Option<ToolCallId> {
        let approvals = self.approvals.clone();
        let queue = approvals.lock().unwrap_or_else(|e| e.into_inner());
        self.wait(&queue, call, name)
    }

    fn wait(
        &mut self,
        approvals: &Receiver<(ToolCallId, bool)>,
        call: ToolCallId,
        name: &str,
    ) -> Option<ToolCallId> {
        let waiting = se_protocol::Running::start(Stage::ApprovalWait);
        // A decision must name the call it answers. Anything else is
        // discarded and the wait continues.
        //
        // The defect this closes, reproduced before it was fixed: a
        // decision used to be forwarded as a bare boolean into a
        // one-slot channel. A second keypress while a tool was running
        // found the slot empty, was kept there, and then answered the
        // *next* request instantly -- so pressing `y` twice silently
        // approved a tool the user was never shown.
        let decision = loop {
            match approvals.recv_timeout(APPROVAL_POLL_INTERVAL) {
                Ok((answered, allow)) if answered == call => break allow,
                // Not for this call: a stale or duplicated decision.
                Ok(_) => continue,
                // The interface is gone; a withheld decision is never
                // permission.
                Err(RecvTimeoutError::Disconnected) => break false,
                // No decision yet. Rechecked here rather than relied on
                // solely via the sentinel `UNPARK` send, because that
                // send can itself be dropped (E9): a 1-slot channel's
                // `try_send` fails silently if a stale decision already
                // occupies it, and a dropped UNPARK must not mean a
                // cancelled task waits forever.
                Err(RecvTimeoutError::Timeout) => {
                    if self.is_cancelled() {
                        break false;
                    }
                    continue;
                }
            }
        };
        let span = waiting.finish();
        self.timings.record(span);
        let allowed = decision && !self.is_cancelled();
        if !allowed {
            let _ = self.emit(Payload::ToolDenied {
                call,
                name: name.to_string(),
            });
            return None;
        }
        Some(call)
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

    /// Harness: the sink under test, the events it emits, the channel a
    /// supervisor would answer decisions on, and the cancellation flag.
    type Harness = (
        TurnSink,
        Receiver<CoreEvent>,
        SyncSender<(ToolCallId, bool)>,
        Arc<AtomicBool>,
    );

    /// Reads the call id out of an `ApprovalRequired` event, the way the
    /// interface and supervisor do.
    fn requested_call(event: &CoreEvent) -> ToolCallId {
        match event.payload {
            Payload::ApprovalRequired { call, .. } => call,
            ref other => panic!("expected an approval request, got {other:?}"),
        }
    }

    fn sink() -> Harness {
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
            // Mirrors the supervisor: see the request, then answer the
            // call it actually names.
            let request = rx.recv().unwrap();
            approvals.send((requested_call(&request), true)).unwrap();
            rx
        });
        assert!(sink.request_approval("run_shell", "cargo test").is_some());
        let _rx = answered.join().unwrap();
    }

    #[test]
    fn a_denied_call_is_reported_as_denied() {
        let (mut sink, rx, approvals, _c) = sink();
        let joined = std::thread::spawn(move || {
            let request = rx.recv().unwrap();
            approvals.send((requested_call(&request), false)).unwrap();
            rx
        });
        assert!(sink.request_approval("write_file", "src/main.rs").is_none());
        let rx = joined.join().unwrap();
        let denied = rx.recv().unwrap();
        assert!(matches!(denied.payload, Payload::ToolDenied { .. }));
    }

    #[test]
    fn cancellation_while_waiting_is_never_read_as_permission() {
        let (mut sink, rx, approvals, cancelled) = sink();
        let joined = std::thread::spawn(move || {
            let request = rx.recv().unwrap();
            // The supervisor's interrupt path: raise the flag, then
            // unpark the waiter. Even though this says `true`, the
            // cancellation must win.
            cancelled.store(true, Ordering::SeqCst);
            approvals.send((requested_call(&request), true)).unwrap();
            rx
        });
        assert!(
            sink.request_approval("run_shell", "rm -rf /").is_none(),
            "a cancelled task must not act on a stale approval"
        );
        let _rx = joined.join().unwrap();
    }

    #[test]
    fn a_dropped_unpark_does_not_leave_a_cancelled_task_waiting_forever() {
        // E9: the supervisor's interrupt path sends UNPARK via a
        // `try_send` to a 1-slot channel. If a stale decision from an
        // *earlier* call is still sitting in that slot, the send
        // silently fails and no UNPARK ever arrives -- simulated here
        // by filling the slot with a decision for a call id that does
        // not exist and never sending anything else. Only the
        // cancellation flag is set, exactly as it would be if the
        // UNPARK send had been dropped. Before the fix this hung
        // forever on a bare `recv()`; now the periodic recheck inside
        // `request_approval` must still notice the flag and return.
        let (mut sink, rx, approvals, cancelled) = sink();
        let stale_call = ToolCallId::next();
        approvals.send((stale_call, true)).unwrap();
        let joined = std::thread::spawn(move || {
            let request = rx.recv().unwrap();
            cancelled.store(true, Ordering::SeqCst);
            // Deliberately nothing else sent: the real UNPARK is the
            // thing this test treats as lost.
            let _ = requested_call(&request);
            rx
        });
        let started = std::time::Instant::now();
        assert!(
            sink.request_approval("run_shell", "anything").is_none(),
            "a cancelled task must not hang waiting for an UNPARK that never arrives"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the poll must notice cancellation well within a couple of intervals, took {:?}",
            started.elapsed()
        );
        let _rx = joined.join().unwrap();
    }

    #[test]
    fn a_withheld_decision_denies_rather_than_allows() {
        let (mut sink, rx, approvals, _c) = sink();
        // The interface goes away without answering.
        drop(approvals);
        assert!(sink.request_approval("run_shell", "anything").is_none());
        drop(rx);
    }

    #[test]
    fn time_spent_waiting_on_a_human_is_recorded_as_such() {
        let (mut sink, rx, approvals, _c) = sink();
        let joined = std::thread::spawn(move || {
            let request = rx.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(30));
            approvals.send((requested_call(&request), true)).unwrap();
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
