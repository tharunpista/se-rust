//! The supervisor, and the handle an interface holds.
//!
//! Three threads matter in SE, and keeping them straight is the whole
//! design:
//!
//! 1. the **interface thread** draws and reads input, and must never
//!    block (I1);
//! 2. the **supervisor thread** owns the operation queue and the task
//!    table, and routes interrupts and approvals;
//! 3. a **worker thread per task** actually runs the turn.
//!
//! The supervisor exists because of the second and third of those: if
//! the worker owned the operation queue, it could not receive an
//! interrupt while it was busy, which is precisely the bug the previous
//! implementation had. The supervisor is always idle enough to listen.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use se_protocol::{
    ApprovalDecision, CoreEvent, CoreOp, Outcome, Payload, SeqCounter, SessionId, Stage, TaskId,
    ToolCallId, UserTurn,
};

use se_provider::{Message, Role};

use crate::sink::TurnSink;
use crate::Runner;

/// How many events may be queued toward the interface before a worker
/// blocks (I3). Sized for a streaming turn: at roughly one delta per
/// token, this is several seconds of generation, so a brief render stall
/// never stalls the model, while a genuinely stuck interface still
/// applies backpressure instead of consuming memory without limit.
const EVENT_QUEUE: usize = 1024;

/// Operations queued toward the supervisor. Small: these arrive at human
/// speed.
const OP_QUEUE: usize = 64;

/// Sent to unpark a worker that is waiting on a decision which will
/// never come, because the task is being cancelled. It deliberately
/// matches no real call id, so it can never be mistaken for an answer.
const UNPARK: ToolCallId = ToolCallId::from_raw(0);

/// Why a submission was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// A task is already running. SE runs one task per session in this
    /// phase; the blueprint defers real concurrency until ownership and
    /// worktree isolation exist, and pretending otherwise now would just
    /// mean two workers writing the same files.
    Busy { running: TaskId },
    /// The core has shut down.
    Stopped,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitError::Busy { running } => {
                write!(f, "task {running} is still running — interrupt it first")
            }
            SubmitError::Stopped => write!(f, "the core has stopped"),
        }
    }
}

impl std::error::Error for SubmitError {}

/// What an interface holds. Cloneable so several views can observe, but
/// only the owner of the event receiver consumes events.
pub struct CoreHandle {
    ops: SyncSender<CoreOp>,
    events: Receiver<CoreEvent>,
    session: SessionId,
    /// Mirrors the supervisor's view so `submit` can refuse locally
    /// rather than sending an operation that will be rejected anyway.
    active: Arc<std::sync::Mutex<Option<TaskId>>>,
}

impl CoreHandle {
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Hands a request to the core and returns immediately.
    ///
    /// Returning a `TaskId` rather than a result-of-the-work is the
    /// point: there is nothing to await here, so an interface physically
    /// cannot block on a model call.
    pub fn submit(
        &self,
        text: &str,
        attachments: Vec<std::path::PathBuf>,
    ) -> Result<TaskId, SubmitError> {
        {
            let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(running) = *active {
                return Err(SubmitError::Busy { running });
            }
        }
        // Allocated here and carried in the operation, so the id the
        // caller receives is the id every event about this task uses.
        let task = TaskId::next();
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            *active = Some(task);
        }
        let turn = UserTurn {
            session: self.session,
            task,
            text: text.to_string(),
            attachments,
        };
        if self.ops.send(CoreOp::SubmitUserTurn(turn)).is_err() {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            *active = None;
            return Err(SubmitError::Stopped);
        }
        Ok(task)
    }

    /// Requests cancellation of a running task.
    pub fn interrupt(&self, task: TaskId) {
        let _ = self.ops.send(CoreOp::Interrupt { task });
    }

    /// Answers a pending approval.
    pub fn approve(&self, decision: ApprovalDecision) {
        let _ = self.ops.send(CoreOp::Approve(decision));
    }

    pub fn shutdown(&self) {
        let _ = self.ops.send(CoreOp::Shutdown);
    }

    /// Takes whatever the core has produced since the last call.
    ///
    /// Non-blocking by construction, because this is called from the
    /// render loop. There is no blocking variant of this method on
    /// purpose: offering one would be offering a way to violate I1.
    pub fn drain(&self) -> Vec<CoreEvent> {
        let mut out = Vec::new();
        // An empty queue and a gone core both end the drain: either way
        // there is nothing more to hand the interface this tick.
        while let Ok(event) = self.events.try_recv() {
            if event.is_terminal() {
                let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                *active = None;
            }
            out.push(event);
        }
        out
    }

    /// Blocks for up to `timeout` waiting for at least one event.
    ///
    /// For tests and headless use only -- never the interactive render
    /// loop, which calls [`drain`](Self::drain). Kept separate so the
    /// blocking call is impossible to reach by accident.
    pub fn drain_blocking(&self, timeout: Duration) -> Vec<CoreEvent> {
        match self.events.recv_timeout(timeout) {
            Ok(first) => {
                if first.is_terminal() {
                    let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
                    *active = None;
                }
                let mut out = vec![first];
                out.extend(self.drain());
                out
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => Vec::new(),
        }
    }

    /// Whether a task is currently running, from the interface's view.
    pub fn is_busy(&self) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

/// The conversation so far, shared between the supervisor and the one
/// worker that may be running.
///
/// A mutex rather than message-passing because only one task runs at a
/// time, so it is uncontended, and because the alternative -- the
/// supervisor reading the event stream to learn what was said -- would
/// make the transcript's correctness depend on the supervisor keeping up
/// with it. The lock is never held across a model call: a worker takes a
/// snapshot, releases, and appends once at the end.
type History = Arc<std::sync::Mutex<Vec<Message>>>;

/// A task the supervisor is tracking.
struct ActiveTask {
    id: TaskId,
    cancelled: Arc<AtomicBool>,
    approvals: SyncSender<(ToolCallId, bool)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// Starts the core and returns the interface's handle.
///
/// The `runner` is what actually executes a turn. Phase 1 ships an echo
/// runner so the whole pipeline -- streaming, cancellation, approval,
/// timings -- is exercised and tested with no provider in existence;
/// Phase 2 substitutes a provider-backed runner with no change to any
/// interface, which is the test of whether this boundary was drawn in
/// the right place.
pub fn start<R: Runner>(runner: R) -> CoreHandle {
    let (op_tx, op_rx) = sync_channel(OP_QUEUE);
    let (event_tx, event_rx) = sync_channel(EVENT_QUEUE);
    let session = SessionId::next();
    let active = Arc::new(std::sync::Mutex::new(None));

    let runner = Arc::new(runner);
    let seq = Arc::new(SeqCounter::new());
    let supervisor_events = event_tx.clone();
    let history: History = Arc::new(std::sync::Mutex::new(Vec::new()));

    std::thread::Builder::new()
        .name("se-supervisor".to_string())
        .spawn(move || supervise(op_rx, supervisor_events, runner, seq, session, history))
        .expect("spawning the supervisor thread");

    CoreHandle {
        ops: op_tx,
        events: event_rx,
        session,
        active,
    }
}

fn supervise<R: Runner>(
    ops: Receiver<CoreOp>,
    events: SyncSender<CoreEvent>,
    runner: Arc<R>,
    seq: Arc<SeqCounter>,
    session: SessionId,
    history: History,
) {
    let mut current: Option<ActiveTask> = None;

    loop {
        // A timeout rather than a blocking receive so finished workers
        // are reaped promptly even when the user is idle.
        let op = match ops.recv_timeout(Duration::from_millis(50)) {
            Ok(op) => Some(op),
            Err(RecvTimeoutError::Timeout) => None,
            // Every interface handle is gone: nothing can arrive again.
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if let Some(task) = current.as_mut() {
            if task.worker.as_ref().is_some_and(|w| w.is_finished()) {
                if let Some(worker) = task.worker.take() {
                    // A panicking worker is reported, not swallowed: the
                    // task must still reach a terminal state (I5). The
                    // worker emits its own `TaskFinished` on the normal
                    // paths, so this covers only the abnormal one.
                    if worker.join().is_err() {
                        let _ = events.send(CoreEvent {
                            seq: seq.next(),
                            session,
                            task: task.id,
                            turn: None,
                            payload: Payload::TaskFinished {
                                outcome: Outcome::Failed {
                                    detail: "the task worker stopped unexpectedly".to_string(),
                                },
                                timings: se_protocol::Timings::default(),
                            },
                        });
                    }
                }
                current = None;
            }
        }

        let Some(op) = op else { continue };

        match op {
            CoreOp::SubmitUserTurn(turn) => {
                if let Some(running) = &current {
                    // The handle refuses first, so this is the narrow
                    // window where it believed the previous task was
                    // over (its terminal event had been drained) while
                    // the worker thread had not yet exited.
                    //
                    // Reporting is mandatory, not optional: dropping the
                    // submission silently would leave an interface
                    // waiting forever for a task it was told had
                    // started, which is exactly the breach of I5 that
                    // "every task reports exactly once" exists to stop.
                    let _ = events.send(CoreEvent {
                        seq: seq.next(),
                        session,
                        task: turn.task,
                        turn: None,
                        payload: Payload::TaskFinished {
                            outcome: Outcome::Failed {
                                detail: format!(
                                    "task {} was still running; nothing was sent",
                                    running.id
                                ),
                            },
                            timings: se_protocol::Timings::default(),
                        },
                    });
                    continue;
                }
                let id = turn.task;
                let cancelled = Arc::new(AtomicBool::new(false));
                let (approval_tx, approval_rx) = sync_channel(1);
                let worker = spawn_worker(
                    id,
                    session,
                    turn,
                    runner.clone(),
                    seq.clone(),
                    events.clone(),
                    cancelled.clone(),
                    approval_rx,
                    history.clone(),
                );
                current = Some(ActiveTask {
                    id,
                    cancelled,
                    approvals: approval_tx,
                    worker: Some(worker),
                });
            }
            CoreOp::Interrupt { .. } => {
                // The task id from the interface may be its reserved one
                // rather than the supervisor's, so the running task is
                // the one interrupted: with one task per session there
                // is no ambiguity about which that is.
                if let Some(task) = &current {
                    task.cancelled.store(true, Ordering::SeqCst);
                    // A worker parked on approval would never see the
                    // flag. `UNPARK` matches no real call, so the worker
                    // loops once, re-checks cancellation and unwinds.
                    let _ = task.approvals.try_send((UNPARK, false));
                }
            }
            CoreOp::Approve(ApprovalDecision { call, allow }) => {
                if let Some(task) = &current {
                    // The call id travels with the decision so the
                    // worker can reject one meant for anything else.
                    let _ = task.approvals.try_send((call, allow));
                }
            }
            CoreOp::Shutdown => {
                if let Some(task) = &mut current {
                    task.cancelled.store(true, Ordering::SeqCst);
                    let _ = task.approvals.try_send((UNPARK, false));
                    if let Some(worker) = task.worker.take() {
                        let _ = worker.join();
                    }
                }
                break;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_worker<R: Runner>(
    id: TaskId,
    session: SessionId,
    turn: UserTurn,
    runner: Arc<R>,
    seq: Arc<SeqCounter>,
    events: SyncSender<CoreEvent>,
    cancelled: Arc<AtomicBool>,
    approvals: Receiver<(ToolCallId, bool)>,
    history: History,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("se-task-{id}"))
        .spawn(move || {
            let mut sink = TurnSink::new(session, id, seq, events, cancelled, approvals);
            let whole_task = se_protocol::Running::start(Stage::Generation);

            if sink
                .emit(Payload::TaskStarted {
                    request: turn.text.clone(),
                })
                .is_err()
            {
                return;
            }

            // A snapshot, taken and released before the model call so
            // the lock is never held across it.
            let prior: Vec<Message> = history.lock().unwrap_or_else(|e| e.into_inner()).clone();

            let outcome = runner.run(&turn, &prior, &mut sink);

            // Cancellation observed after the fact still reports as
            // cancelled rather than as whatever the runner managed to
            // return, so the transcript matches what the user asked for.
            let outcome = if sink.is_cancelled() && !matches!(outcome, Outcome::Cancelled) {
                Outcome::Cancelled
            } else {
                outcome
            };

            // The conversation grows only on a real answer. A cancelled
            // or failed turn leaves no half-exchange behind to confuse
            // the next one.
            if let Outcome::Completed { final_text } = &outcome {
                let mut conversation = history.lock().unwrap_or_else(|e| e.into_inner());
                conversation.push(Message {
                    role: Role::User,
                    content: turn.text.clone(),
                });
                conversation.push(Message {
                    role: Role::Assistant,
                    content: final_text.clone(),
                });
            }

            let _ = sink.finish_turn();
            let elapsed = whole_task.finish();
            let mut timings = std::mem::take(&mut sink.timings);
            // The generation span is wall time for the whole task minus
            // what was spent waiting on a human, so it stays comparable
            // between an approved run and an unattended one.
            let waited = timings.of(Stage::ApprovalWait).unwrap_or(Duration::ZERO);
            timings.record(se_protocol::Span {
                stage: Stage::Generation,
                duration: elapsed.duration.saturating_sub(waited),
            });

            let _ = sink.emit(Payload::TaskFinished { outcome, timings });
        })
        .expect("spawning a task worker thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EchoRunner;

    fn collect_until_terminal(handle: &CoreHandle, limit: Duration) -> Vec<CoreEvent> {
        let deadline = std::time::Instant::now() + limit;
        let mut all = Vec::new();
        while std::time::Instant::now() < deadline {
            for event in handle.drain_blocking(Duration::from_millis(20)) {
                let terminal = event.is_terminal();
                all.push(event);
                if terminal {
                    return all;
                }
            }
        }
        panic!(
            "no terminal event within {limit:?}; got {} events",
            all.len()
        );
    }

    #[test]
    fn a_submitted_turn_streams_deltas_and_finishes_exactly_once() {
        let handle = start(EchoRunner::instant());
        handle.submit("hello", vec![]).unwrap();
        let events = collect_until_terminal(&handle, Duration::from_secs(5));

        assert!(matches!(
            events.first().map(|e| &e.payload),
            Some(Payload::TaskStarted { .. })
        ));
        let deltas = events
            .iter()
            .filter(|e| matches!(e.payload, Payload::Delta { .. }))
            .count();
        assert!(deltas > 0, "a turn must stream something");
        let terminal = events.iter().filter(|e| e.is_terminal()).count();
        assert_eq!(terminal, 1, "exactly one terminal event (I5)");

        // Sequence numbers are strictly increasing, so a consumer can
        // detect loss rather than assume ordering.
        let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
        assert!(
            seqs.windows(2).all(|w| w[1] > w[0]),
            "sequence must be monotonic: {seqs:?}"
        );
        handle.shutdown();
    }

    #[test]
    fn submitting_returns_immediately_rather_than_awaiting_the_turn() {
        // The invariant the previous implementation broke. A slow turn
        // must not delay the caller at all.
        let handle = start(EchoRunner::slow(Duration::from_millis(80), 10));
        let started = std::time::Instant::now();
        handle.submit("something long", vec![]).unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "submit must not block: took {elapsed:?}"
        );
        handle.shutdown();
    }

    #[test]
    fn draining_never_blocks_even_with_a_turn_in_flight() {
        let handle = start(EchoRunner::slow(Duration::from_millis(50), 20));
        handle.submit("long", vec![]).unwrap();
        // Exactly what the render loop does, 30 times in a row.
        for _ in 0..30 {
            let started = std::time::Instant::now();
            let _ = handle.drain();
            assert!(
                started.elapsed() < Duration::from_millis(20),
                "drain must not block the render path"
            );
        }
        handle.interrupt(TaskId::from_raw(0));
        handle.shutdown();
    }

    #[test]
    fn a_second_submission_is_refused_while_one_is_running() {
        let handle = start(EchoRunner::slow(Duration::from_millis(40), 20));
        let first = handle.submit("one", vec![]).unwrap();
        let second = handle.submit("two", vec![]);
        assert_eq!(second, Err(SubmitError::Busy { running: first }));
        assert!(handle.is_busy());
        handle.shutdown();
    }

    #[test]
    fn an_interrupt_stops_a_running_turn_and_it_reports_as_cancelled() {
        let handle = start(EchoRunner::slow(Duration::from_millis(30), 100));
        let task = handle.submit("a very long answer", vec![]).unwrap();
        // Let it get going, then stop it.
        std::thread::sleep(Duration::from_millis(80));
        handle.interrupt(task);

        let events = collect_until_terminal(&handle, Duration::from_secs(5));
        let Some(Payload::TaskFinished { outcome, .. }) = events.last().map(|e| e.payload.clone())
        else {
            panic!("expected a terminal event");
        };
        assert_eq!(outcome, Outcome::Cancelled);
        // And it stopped early rather than running all 100 deltas.
        let deltas = events
            .iter()
            .filter(|e| matches!(e.payload, Payload::Delta { .. }))
            .count();
        assert!(
            deltas < 100,
            "cancellation must cut the turn short: {deltas}"
        );
        handle.shutdown();
    }

    #[test]
    fn the_handle_reports_not_busy_once_a_task_finishes() {
        let handle = start(EchoRunner::instant());
        handle.submit("hi", vec![]).unwrap();
        assert!(handle.is_busy());
        let _ = collect_until_terminal(&handle, Duration::from_secs(5));
        assert!(
            !handle.is_busy(),
            "a finished task must free the session for the next one"
        );
        // And a further submission is accepted.
        handle.submit("again", vec![]).unwrap();
        handle.shutdown();
    }

    #[test]
    fn timings_separate_local_work_from_waiting() {
        let handle = start(EchoRunner::instant());
        handle.submit("hi", vec![]).unwrap();
        let events = collect_until_terminal(&handle, Duration::from_secs(5));
        let Some(Payload::TaskFinished { timings, .. }) = events.last().map(|e| e.payload.clone())
        else {
            panic!("expected a terminal event");
        };
        assert!(
            timings.of(Stage::Generation).is_some(),
            "a turn must report how long it took: {}",
            timings.summary()
        );
        handle.shutdown();
    }

    /// A decision must answer the call it was made for.
    ///
    /// The defect this proves absent: `Approve` used to be forwarded
    /// without checking its call id, into a buffered channel. A stray
    /// keypress while a tool ran was therefore kept and silently used to
    /// approve the *next* request -- an approval bypass reachable by
    /// pressing `y` twice.
    #[test]
    fn a_stray_decision_never_approves_a_later_tool_call() {
        let handle = start(crate::runner::TwoApprovalRunner);
        handle.submit("do two things", vec![]).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut answered_first = false;
        let mut outcome = None;
        let mut approvals_seen = 0;

        while std::time::Instant::now() < deadline && outcome.is_none() {
            for event in handle.drain_blocking(Duration::from_millis(20)) {
                match &event.payload {
                    Payload::ApprovalRequired { call, name, .. } => {
                        approvals_seen += 1;
                        if !answered_first {
                            assert_eq!(name, "first_tool");
                            // Answer the first, then immediately send a
                            // second decision for the same call -- what
                            // a user hammering `y` produces.
                            handle.approve(se_protocol::ApprovalDecision {
                                call: *call,
                                allow: true,
                            });
                            // A second press, landing while the first
                            // tool is still running and nothing is
                            // waiting for a decision.
                            std::thread::sleep(Duration::from_millis(40));
                            handle.approve(se_protocol::ApprovalDecision {
                                call: *call,
                                allow: true,
                            });
                            answered_first = true;
                        } else {
                            // The second request must have genuinely
                            // blocked rather than being answered by the
                            // stray decision above. Deny it.
                            assert_eq!(name, "second_tool");
                            handle.approve(se_protocol::ApprovalDecision {
                                call: *call,
                                allow: false,
                            });
                        }
                    }
                    Payload::TaskFinished { outcome: o, .. } => outcome = Some(o.clone()),
                    _ => {}
                }
            }
        }

        assert_eq!(approvals_seen, 2, "both tools must ask");
        assert_eq!(
            outcome,
            Some(Outcome::Completed {
                final_text: "ran: first_tool".to_string()
            }),
            "the denied second tool must not have run"
        );
        handle.shutdown();
    }

    #[test]
    fn a_panicking_worker_still_produces_a_terminal_event() {
        // I5 has to hold even when the runner is broken, or an interface
        // shows a spinner forever.
        let handle = start(crate::runner::PanickingRunner);
        handle.submit("boom", vec![]).unwrap();
        let events = collect_until_terminal(&handle, Duration::from_secs(5));
        let Some(Payload::TaskFinished { outcome, .. }) = events.last().map(|e| e.payload.clone())
        else {
            panic!("expected a terminal event");
        };
        assert!(matches!(outcome, Outcome::Failed { .. }));
        handle.shutdown();
    }
}

#[cfg(test)]
mod admission {
    use super::*;
    use crate::EchoRunner;

    /// A submission must never vanish.
    ///
    /// The window this covers: the interface clears its "busy" flag when
    /// it sees `TaskFinished`, but the supervisor only released the slot
    /// once the worker *thread* had exited. A submit arriving between
    /// those two moments was accepted by the handle and then silently
    /// dropped by the supervisor, leaving the interface waiting for a
    /// turn that would never report. Run many times, because the gap is
    /// small.
    #[test]
    fn a_submission_right_after_a_finished_task_is_never_dropped() {
        let handle = start(EchoRunner::instant());

        for round in 0..150 {
            // Submit as soon as the handle says it can, which is exactly
            // the moment the previous terminal event was drained.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match handle.submit(&format!("round {round}"), vec![]) {
                    Ok(_) => break,
                    Err(SubmitError::Busy { .. }) => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "round {round}: never became free"
                        );
                        let _ = handle.drain();
                    }
                    Err(e) => panic!("round {round}: {e}"),
                }
            }

            // Every accepted submission must reach a terminal event.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut finished = false;
            while !finished {
                assert!(
                    std::time::Instant::now() < deadline,
                    "round {round}: the accepted submission never finished"
                );
                for event in handle.drain_blocking(Duration::from_millis(20)) {
                    if event.is_terminal() {
                        finished = true;
                    }
                }
            }
        }
        handle.shutdown();
    }
}
