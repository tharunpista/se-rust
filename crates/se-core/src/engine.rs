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
use std::sync::mpsc::{channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender};
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
    /// Every worker slot is taken. [`start`] has one slot, so the
    /// interactive session still runs one task at a time: two workers
    /// writing the same files is unsafe until file-conflict safety
    /// (Phase 4 item 7) exists. [`start_pool`] widens the cap for callers
    /// that own that risk, such as the team scheduler. `running` is one
    /// of the occupying tasks.
    Busy { running: TaskId },
    /// The core has shut down.
    Stopped,
    /// The text carries something shaped like an API key; it was not sent or recorded.
    LooksLikeSecret,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitError::Busy { running } => {
                write!(f, "task {running} is still running — interrupt it first")
            }
            SubmitError::Stopped => write!(f, "the core has stopped"),
            SubmitError::LooksLikeSecret => write!(
                f,
                "that looks like an API key, so it was not sent or saved. To connect a provider use /connect <provider>"
            ),
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
    active: Arc<std::sync::Mutex<Vec<TaskId>>>,
    /// The worker-pool size this core was started with.
    max_concurrent: usize,
    /// `None` until [`CoreHandle::attach_session_log`] is called.
    /// Deliberately not set at [`start`] time: most tests, and any
    /// caller that has not decided where sessions live, must see
    /// exactly today's behaviour with no file ever touched.
    session_log: Arc<std::sync::Mutex<Option<crate::session_store::SessionStore>>>,
}

impl CoreHandle {
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Opens `path` as this session's durable event log. Every event
    /// this handle hands to an interface from this point on is also
    /// appended there (see [`CoreHandle::drain`]). Events already
    /// drained before this call are not retroactively logged -- call
    /// this before the first `drain` if nothing may be missed.
    pub fn attach_session_log(&self, path: &std::path::Path) -> std::io::Result<()> {
        let store = crate::session_store::SessionStore::open(path)?;
        let mut guard = self.session_log.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(store);
        Ok(())
    }

    /// Appends `event` to the attached session log, if any. A write
    /// failure is not surfaced to the caller of `drain`: the event
    /// itself was already delivered and losing it would be worse than
    /// losing one line of a log the caller did not ask to be told
    /// about mid-loop. (A future diagnostics surface can read this
    /// back out; nothing today needs to.)
    fn persist(&self, event: &CoreEvent) {
        let mut guard = self.session_log.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(store) = guard.as_mut() {
            let _ = store.append(event);
        }
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
        self.submit_inner(text, attachments, None, None, None)
    }

    /// Like [`CoreHandle::submit`], with an explicit per-turn model
    /// override -- routing precedence level 1 in charter §6.6. No
    /// interface surface sets this yet (there is no `/model` picker and
    /// no specialist/team profile to read), but the seam exists so one
    /// can be wired to it without changing this signature again.
    pub fn submit_with_model(
        &self,
        text: &str,
        attachments: Vec<std::path::PathBuf>,
        model: Option<se_protocol::ModelRef>,
    ) -> Result<TaskId, SubmitError> {
        self.submit_inner(text, attachments, model, None, None)
    }

    /// Routes this turn through the specialist named by `id` (a
    /// catalogue agent id, e.g. `"backend-lead"`).
    ///
    /// Deliberately does no catalogue I/O here: this method only sets
    /// [`UserTurn::specialist_id`] and returns, exactly as fast as
    /// [`CoreHandle::submit`] already is. Resolving `id` against the
    /// real catalogue (and failing with a clear message if it names no
    /// agent) happens on the worker thread, inside whichever `Runner`
    /// is driving the task -- never on this, the caller's, thread. A
    /// caller on the interface thread calling this method must never
    /// block on a filesystem scan, which is the whole reason this
    /// method exists rather than a caller resolving the specialist
    /// itself and setting a richer field.
    pub fn submit_specialist(
        &self,
        id: &str,
        text: &str,
        attachments: Vec<std::path::PathBuf>,
    ) -> Result<TaskId, SubmitError> {
        self.submit_inner(text, attachments, None, Some(id.to_string()), None)
    }

    /// [`CoreHandle::submit_specialist`] with a per-turn model override.
    pub fn submit_specialist_with_model(
        &self,
        id: &str,
        text: &str,
        attachments: Vec<std::path::PathBuf>,
        model: Option<se_protocol::ModelRef>,
    ) -> Result<TaskId, SubmitError> {
        self.submit_inner(text, attachments, model, Some(id.to_string()), None)
    }

    /// Submits one node of a team run. `specialist` `None` lets the
    /// runner pick one; the turn is marked as scheduled work.
    pub fn submit_team_node(
        &self,
        node: &str,
        specialist: Option<&str>,
        text: &str,
        model: Option<se_protocol::ModelRef>,
    ) -> Result<TaskId, SubmitError> {
        self.submit_inner(
            text,
            Vec::new(),
            model,
            specialist.map(str::to_string),
            Some(node.to_string()),
        )
    }

    fn submit_inner(
        &self,
        text: &str,
        attachments: Vec<std::path::PathBuf>,
        model: Option<se_protocol::ModelRef>,
        specialist_id: Option<String>,
        team_node: Option<String>,
    ) -> Result<TaskId, SubmitError> {
        // A user's text never carries a key to a model or a session file. Team nodes are
        // authored prompts, not pastes, so a false positive must not fail a run.
        if team_node.is_none() && se_provider::credentials::looks_like_secret(text) {
            return Err(SubmitError::LooksLikeSecret);
        }
        // Allocated here and carried in the operation, so the id the
        // caller receives is the id every event about this task uses.
        let task = TaskId::next();
        {
            // Check and reserve under one lock, so two callers cannot
            // both see the last free slot.
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if active.len() >= self.max_concurrent {
                return Err(SubmitError::Busy { running: active[0] });
            }
            active.push(task);
        }
        let turn = UserTurn {
            session: self.session,
            task,
            text: text.to_string(),
            attachments,
            model,
            specialist_id,
            team_node,
        };
        if self.ops.send(CoreOp::SubmitUserTurn(turn)).is_err() {
            self.release(task);
            return Err(SubmitError::Stopped);
        }
        Ok(task)
    }

    fn release(&self, task: TaskId) {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        active.retain(|t| *t != task);
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

    /// Sets, or with `None` clears, the conversation's specialist.
    pub fn set_specialist(&self, id: Option<&str>) {
        let _ = self.ops.send(CoreOp::SetSpecialist(id.map(str::to_string)));
    }

    /// Sets how tool calls are handled: manual, auto or plan.
    pub fn set_mode(&self, mode: se_protocol::WorkMode) {
        let _ = self.ops.send(CoreOp::SetMode(mode));
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
                self.release(event.task);
            }
            self.persist(&event);
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
                    self.release(first.task);
                }
                self.persist(&first);
                let mut out = vec![first];
                out.extend(self.drain());
                out
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => Vec::new(),
        }
    }

    /// Whether a task is currently running, from the interface's view.
    pub fn is_busy(&self) -> bool {
        !self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// The session transcript: completed user/assistant exchanges only.
///
/// Shared on purpose: it is the conversation the user sees, not task
/// state. What E8 calls "per-task history" -- a task's own in-flight
/// messages, tool calls and results -- never lives here; it stays local
/// to the runner's loop. A worker snapshots this before its model call
/// and appends its exchange as one locked push of both messages, so
/// concurrent tasks can never interleave half an exchange.
/// `None` for an isolated pool: each task starts from nothing and
/// leaves nothing behind (see [`start_isolated_pool`]).
type History = Option<Arc<Conversation>>;

/// The shared transcript, plus where it is saved when the session is a
/// resumable conversation (see [`start_conversation`]).
struct Conversation {
    messages: std::sync::Mutex<Vec<Message>>,
    path: Option<std::path::PathBuf>,
}

impl Conversation {
    /// Saves the transcript whole: written to a sibling temp file, then
    /// renamed over the old one, so a crash mid-write never leaves a
    /// half-written conversation behind.
    fn save(&self, messages: &[Message]) {
        let Some(path) = &self.path else { return };
        let Ok(json) = serde_json::to_string(messages) else {
            return;
        };
        let json = se_provider::credentials::redact_secrets(&json);
        let tmp = path.with_extension("json.tmp");
        // shortcut: a failed save is dropped (the turn already happened and
        // the in-memory transcript is intact); surface it if users hit it.
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Reads a saved conversation; a missing file is a new, empty one.
pub fn load_conversation(path: &std::path::Path) -> std::io::Result<Vec<Message>> {
    match std::fs::read(path) {
        // Redacted on the way in too: a file saved before the guard existed (or edited by
        // hand) must never resend a key as `prior` history.
        Ok(bytes) => serde_json::from_str(&se_provider::credentials::redact_secrets(
            &String::from_utf8_lossy(&bytes),
        ))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// A task the supervisor is tracking.
struct ActiveTask {
    id: TaskId,
    cancelled: Arc<AtomicBool>,
    approvals: Sender<(ToolCallId, bool)>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Set by the worker just before it emits `TaskFinished`. Admission
    /// counts only unfinished tasks: once the interface has drained the
    /// terminal event it may submit again, possibly before this thread
    /// has exited and been reaped, and that must not be refused.
    finished: Arc<AtomicBool>,
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
    start_pool(runner, 1)
}

/// Starts a core that runs up to `max_concurrent` tasks at once, each on
/// its own worker thread, with interrupts and approvals routed by task id
/// (E8). Panics if `max_concurrent` is zero, which could never run
/// anything.
///
/// shortcut: one global cap only. Per-provider/account semaphores need
/// the runner to choose a provider per turn (Phase 2a item 6), and today
/// a core has exactly one provider, so a per-provider cap would equal
/// this one. Add them when the runner is registry-backed.
pub fn start_pool<R: Runner>(runner: R, max_concurrent: usize) -> CoreHandle {
    start_with(runner, max_concurrent, Some(conversation(Vec::new(), None)))
}

/// Like [`start`], but the conversation is loaded from `path` (if it
/// exists) and saved back to it after every completed turn, so a later
/// `se --resume` continues it -- on any provider -- without the user
/// re-explaining anything.
pub fn start_conversation<R: Runner>(
    runner: R,
    path: &std::path::Path,
) -> std::io::Result<CoreHandle> {
    let prior = load_conversation(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(start_with(
        runner,
        1,
        Some(conversation(prior, Some(path.to_path_buf()))),
    ))
}

fn conversation(messages: Vec<Message>, path: Option<std::path::PathBuf>) -> Arc<Conversation> {
    Arc::new(Conversation {
        messages: std::sync::Mutex::new(messages),
        path,
    })
}

/// Like [`start_pool`], but no task sees or extends a shared transcript.
/// For scheduled team work: one task's finished output must never reach
/// another task as conversation it appears to have had (Phase 4 item 1).
pub fn start_isolated_pool<R: Runner>(runner: R, max_concurrent: usize) -> CoreHandle {
    start_with(runner, max_concurrent, None)
}

fn start_with<R: Runner>(runner: R, max_concurrent: usize, history: History) -> CoreHandle {
    assert!(max_concurrent > 0, "a worker pool needs at least one slot");
    let (op_tx, op_rx) = sync_channel(OP_QUEUE);
    let (event_tx, event_rx) = sync_channel(EVENT_QUEUE);
    let session = SessionId::next();
    let active = Arc::new(std::sync::Mutex::new(Vec::new()));

    let runner = Arc::new(runner);
    let seq = Arc::new(SeqCounter::new());
    let supervisor_events = event_tx.clone();

    std::thread::Builder::new()
        .name("se-supervisor".to_string())
        .spawn(move || {
            supervise(
                op_rx,
                supervisor_events,
                runner,
                seq,
                session,
                history,
                max_concurrent,
            )
        })
        .expect("spawning the supervisor thread");

    CoreHandle {
        ops: op_tx,
        events: event_rx,
        session,
        active,
        max_concurrent,
        session_log: Arc::new(std::sync::Mutex::new(None)),
    }
}

/// Wakes a worker parked on an approval so it re-checks cancellation.
fn cancel(task: &ActiveTask) {
    task.cancelled.store(true, Ordering::SeqCst);
    // `UNPARK` matches no real call, so the worker loops once, sees the
    // flag and unwinds.
    let _ = task.approvals.send((UNPARK, false));
}

fn supervise<R: Runner>(
    ops: Receiver<CoreOp>,
    events: SyncSender<CoreEvent>,
    runner: Arc<R>,
    seq: Arc<SeqCounter>,
    session: SessionId,
    history: History,
    max_concurrent: usize,
) {
    let mut running: Vec<ActiveTask> = Vec::new();

    loop {
        // A timeout rather than a blocking receive so finished workers
        // are reaped promptly even when the user is idle.
        let op = match ops.recv_timeout(Duration::from_millis(50)) {
            Ok(op) => Some(op),
            Err(RecvTimeoutError::Timeout) => None,
            // Every interface handle is gone: nothing can arrive again.
            Err(RecvTimeoutError::Disconnected) => break,
        };

        running.retain_mut(|task| {
            if !task.worker.as_ref().is_some_and(|w| w.is_finished()) {
                return true;
            }
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
            false
        });

        let Some(op) = op else { continue };

        match op {
            CoreOp::SubmitUserTurn(turn) => {
                let occupied: Vec<&ActiveTask> = running
                    .iter()
                    .filter(|t| !t.finished.load(Ordering::SeqCst))
                    .collect();
                if occupied.len() >= max_concurrent {
                    let busy = occupied[0];
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
                                    busy.id
                                ),
                            },
                            timings: se_protocol::Timings::default(),
                        },
                    });
                    continue;
                }
                let id = turn.task;
                let cancelled = Arc::new(AtomicBool::new(false));
                // Unbounded on purpose (E9, row 4.9): a 1-slot channel with
                // `try_send` silently dropped a real decision whenever a stray
                // duplicate still occupied the slot, hanging the task. Decisions
                // arrive at human speed and the worker discards mismatches, so
                // the queue cannot grow meaningfully.
                let (approval_tx, approval_rx) = channel();
                let finished = Arc::new(AtomicBool::new(false));
                let spawned = spawn_worker(
                    id,
                    session,
                    turn,
                    runner.clone(),
                    seq.clone(),
                    events.clone(),
                    cancelled.clone(),
                    approval_rx,
                    history.clone(),
                    finished.clone(),
                );
                // Out of threads or memory: fail this one task, with the
                // OS's reason, and keep the core serving the others.
                let worker = match spawned {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = events.send(CoreEvent {
                            seq: seq.next(),
                            session,
                            task: id,
                            turn: None,
                            payload: Payload::TaskFinished {
                                outcome: Outcome::Failed {
                                    detail: format!("could not start a worker thread: {e}"),
                                },
                                timings: se_protocol::Timings::default(),
                            },
                        });
                        continue;
                    }
                };
                running.push(ActiveTask {
                    id,
                    cancelled,
                    approvals: approval_tx,
                    worker: Some(worker),
                    finished,
                });
            }
            // E8: both route by task id. A stale interrupt or decision
            // for a task that is gone matches nothing and is dropped; it
            // can never land on whichever task happens to be running.
            CoreOp::Interrupt { task: target } => {
                if let Some(task) = running.iter().find(|t| t.id == target) {
                    cancel(task);
                }
            }
            CoreOp::Approve(ApprovalDecision { task, call, allow }) => {
                if let Some(task) = running.iter().find(|t| t.id == task) {
                    // The worker still compares the call id, so a stale
                    // decision for an earlier call in the same task is
                    // rejected there.
                    let _ = task.approvals.send((call, allow));
                }
            }
            CoreOp::SetSpecialist(id) => runner.set_specialist(id),
            CoreOp::SetMode(mode) => runner.set_mode(mode),
            CoreOp::Shutdown => {
                // Signal every task before joining any, so they unwind
                // in parallel rather than one after another.
                running.iter().for_each(cancel);
                for task in &mut running {
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
    finished: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    #[cfg(test)]
    if turn.text == FAIL_SPAWN_TEXT {
        return Err(std::io::Error::other("test: no threads left"));
    }
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
            let prior: Vec<Message> = history
                .as_ref()
                .map(|h| h.messages.lock().unwrap_or_else(|e| e.into_inner()).clone())
                .unwrap_or_default();

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
            if let (Outcome::Completed { final_text }, Some(history)) = (&outcome, &history) {
                let mut conversation = history.messages.lock().unwrap_or_else(|e| e.into_inner());
                // A compaction is applied only if the transcript still
                // starts with exactly what this turn summarized.
                if let Some((covers, with)) = sink.compaction.take() {
                    if conversation.get(..covers) == prior.get(..covers) {
                        conversation.splice(..covers, with);
                    }
                }
                conversation.push(Message::text(Role::User, turn.text.clone()));
                conversation.push(Message::text(Role::Assistant, final_text.clone()));
                history.save(&conversation);
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

            finished.store(true, Ordering::SeqCst);
            let _ = sink.emit(Payload::TaskFinished { outcome, timings });
        })
}

/// A request with exactly this text fails to spawn, to test that path
/// without exhausting the machine's threads (keyed on the task, not a
/// global flag, so parallel tests cannot take each other's failure).
#[cfg(test)]
const FAIL_SPAWN_TEXT: &str = "se-test: fail this worker spawn";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EchoRunner;

    /// A worker that cannot be spawned fails its own task with the OS
    /// reason; the core survives and runs the next task normally.
    #[test]
    fn a_failed_worker_spawn_fails_only_that_task() {
        let core = start(EchoRunner::instant());
        let first = core.submit(FAIL_SPAWN_TEXT, vec![]).unwrap();
        let outcome_of = |task: TaskId| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                for e in core.drain_blocking(Duration::from_millis(20)) {
                    if let (true, Payload::TaskFinished { outcome, .. }) =
                        (e.task == task, e.payload)
                    {
                        return outcome;
                    }
                }
            }
            panic!("task never finished");
        };
        match outcome_of(first) {
            Outcome::Failed { detail } => assert!(detail.contains("worker thread"), "{detail}"),
            other => panic!("expected a failure, got {other:?}"),
        }
        let second = core.submit("two", vec![]).unwrap();
        assert!(matches!(outcome_of(second), Outcome::Completed { .. }));
        core.shutdown();
    }

    /// Ignores cancellation entirely and always reports success, after
    /// a short delay so a test has time to call `interrupt` first. The
    /// one thing this type exists to prove: the override at the
    /// `sink.is_cancelled()` check below does not depend on the runner
    /// cooperating. A runner that never looks at `is_cancelled` --
    /// buggy, or simply not written yet -- still cannot make a
    /// cancelled task report as completed.
    struct IgnoresCancellationRunner;

    impl Runner for IgnoresCancellationRunner {
        fn describe(&self) -> (String, String) {
            ("test".to_string(), "ignores-cancellation".to_string())
        }
        fn run(&self, _turn: &UserTurn, _history: &[Message], _sink: &mut TurnSink) -> Outcome {
            std::thread::sleep(Duration::from_millis(60));
            Outcome::Completed {
                final_text: "the runner never checked is_cancelled".to_string(),
            }
        }
    }

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

    /// Echoes back whatever model override it was given, as the
    /// completed text, so a test can assert on it without a real
    /// provider.
    struct RecordsModelRunner;

    impl Runner for RecordsModelRunner {
        fn describe(&self) -> (String, String) {
            ("test".to_string(), "records-model".to_string())
        }
        fn run(&self, turn: &UserTurn, _history: &[Message], _sink: &mut TurnSink) -> Outcome {
            let seen = match &turn.model {
                Some(m) => format!("{}/{}", m.provider, m.model),
                None => "none".to_string(),
            };
            Outcome::Completed { final_text: seen }
        }
    }

    #[test]
    fn submit_with_model_carries_the_override_onto_the_turn_the_runner_sees() {
        let handle = crate::start(RecordsModelRunner);
        let model = se_protocol::ModelRef {
            provider: "anthropic".to_string(),
            model: "sonnet".to_string(),
        };
        handle.submit_with_model("hi", vec![], Some(model)).unwrap();
        let events = collect_until_terminal(&handle, Duration::from_secs(5));
        let outcome = events
            .iter()
            .find_map(|e| match &e.payload {
                Payload::TaskFinished { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .expect("a terminal event must carry an outcome");
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "anthropic/sonnet".to_string()
            }
        );
        handle.shutdown();
    }

    #[test]
    fn plain_submit_carries_no_model_override() {
        let handle = crate::start(RecordsModelRunner);
        handle.submit("hi", vec![]).unwrap();
        let events = collect_until_terminal(&handle, Duration::from_secs(5));
        let outcome = events
            .iter()
            .find_map(|e| match &e.payload {
                Payload::TaskFinished { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .expect("a terminal event must carry an outcome");
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "none".to_string()
            }
        );
        handle.shutdown();
    }

    #[test]
    fn a_cancelled_task_reports_cancelled_even_if_the_runner_ignores_cancellation() {
        let handle = crate::start(IgnoresCancellationRunner);
        let task = handle.submit("hi", vec![]).unwrap();
        std::thread::sleep(Duration::from_millis(15));
        handle.interrupt(task);
        let events = collect_until_terminal(&handle, Duration::from_secs(5));
        let outcome = events
            .iter()
            .find_map(|e| match &e.payload {
                Payload::TaskFinished { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .expect("a terminal event must carry an outcome");
        assert_eq!(
            outcome,
            Outcome::Cancelled,
            "the override must win over whatever the runner itself returned"
        );
        handle.shutdown();
    }

    #[test]
    fn a_killed_mid_turn_session_reads_interrupted_not_completed_on_resume() {
        // The Phase 2a gate, end to end through the real CoreHandle:
        // start a slow turn, drain (which is what persists -- nothing
        // is logged until something reads it, exactly as a real TUI
        // loop does), then simulate the process dying by dropping the
        // handle before the turn ever finishes. A fresh load of the
        // same log must show the task as interrupted, never completed,
        // and nothing must have been silently replayed.
        let path = std::env::temp_dir().join(format!(
            "se-engine-resume-test-{}.jsonl",
            std::process::id()
        ));
        let _cleanup = scopeguard(path.clone());

        let handle = crate::start(EchoRunner::slow(Duration::from_millis(200), 50));
        handle.attach_session_log(&path).unwrap();
        let task = handle
            .submit("a turn that will not finish", vec![])
            .unwrap();
        // Pull at least the TaskStarted event so it is actually
        // persisted, then "crash" -- drop the handle without ever
        // seeing a terminal event.
        let _ = handle.drain_blocking(Duration::from_secs(2));
        drop(handle);

        let (events, errors) = crate::session_store::load(&path);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!events.is_empty(), "the started turn must have been logged");
        let result = crate::session_store::reconcile(&events);
        let (logged_task, state) = result
            .into_iter()
            .find(|(t, _)| *t == task)
            .expect("the task must appear in its own session log");
        assert_eq!(logged_task, task);
        assert_eq!(
            state,
            crate::session_store::TaskState::Interrupted,
            "a process death mid-turn must never read back as completed"
        );
    }

    /// Removes the test's log file even if an assertion above panics.
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn scopeguard(path: std::path::PathBuf) -> Cleanup {
        Cleanup(path)
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

    /// A conversation saved before the guard (the incident's file) loads redacted.
    #[test]
    fn a_saved_conversation_holding_a_key_loads_redacted() {
        let path = std::env::temp_dir().join(format!("se-redact-{}.json", std::process::id()));
        let _cleanup = scopeguard(path.clone());
        std::fs::write(
            &path,
            r#"[{"role":"User","content":"key:\nsk-abcdefghij0123456789ABCDEFGHIJ"}]"#,
        )
        .unwrap();
        let loaded = load_conversation(&path).unwrap();
        assert_eq!(loaded[0].content, "key:\n[REDACTED]");
    }

    /// The incident: a pasted key went to a model and into the session file.
    #[test]
    fn a_pasted_api_key_is_refused_and_never_reaches_the_runner() {
        let handle = start(EchoRunner::instant());
        let key = "sk-abcdefghij0123456789ABCDEFGHIJ0123456789abcdefghij0";
        assert!(matches!(
            handle.submit(key, vec![]),
            Err(SubmitError::LooksLikeSecret)
        ));
        assert!(matches!(
            handle.submit_with_model(&format!("connect this {key}"), vec![], None),
            Err(SubmitError::LooksLikeSecret)
        ));
        std::thread::sleep(Duration::from_millis(50));
        assert!(handle.drain().is_empty(), "nothing started, nothing recorded");
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
    fn the_approved_call_id_is_the_same_id_the_tool_actually_runs_and_completes_under() {
        // E7: `request_approval` used to hand back a plain bool, and
        // `tool_requested` minted its own id for what the sink had
        // already approved -- so the id a human approved and the id
        // the tool actually ran under were two different numbers with
        // nothing connecting them. This drives a real ApprovalRunner
        // through a real CoreHandle and asserts all three events name
        // the same call.
        let handle = start(crate::runner::ApprovalRunner { tool: "run_shell" });
        handle.submit("please run it", vec![]).unwrap();

        let mut approval_required_call = None;
        let mut tool_requested_call = None;
        let mut tool_completed_call = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        'outer: while std::time::Instant::now() < deadline {
            for event in handle.drain_blocking(Duration::from_millis(20)) {
                match &event.payload {
                    Payload::ApprovalRequired { call, .. } => {
                        approval_required_call = Some(*call);
                        handle.approve(se_protocol::ApprovalDecision {
                            task: event.task,
                            call: *call,
                            allow: true,
                        });
                    }
                    Payload::ToolRequested { call, .. } => tool_requested_call = Some(*call),
                    Payload::ToolCompleted { call, .. } => tool_completed_call = Some(*call),
                    Payload::TaskFinished { .. } => break 'outer,
                    _ => {}
                }
            }
        }
        handle.shutdown();

        let approved = approval_required_call.expect("must have asked for approval");
        assert_eq!(
            tool_requested_call,
            Some(approved),
            "the tool must announce itself under the exact id that was approved"
        );
        assert_eq!(
            tool_completed_call,
            Some(approved),
            "the tool must complete under the exact id that was approved"
        );
    }

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
                                task: event.task,
                                call: *call,
                                allow: true,
                            });
                            // A second press, landing while the first
                            // tool is still running and nothing is
                            // waiting for a decision.
                            std::thread::sleep(Duration::from_millis(40));
                            handle.approve(se_protocol::ApprovalDecision {
                                task: event.task,
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
                                task: event.task,
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
    fn a_stale_interrupt_for_a_finished_task_never_cancels_the_next_one() {
        // E8: `Interrupt` ignoring the task id it was asked to cancel.
        // A reserved interrupt that finally arrives after its task
        // already finished must not reach whatever task is current by
        // the time it is processed.
        let handle = start(crate::runner::EchoRunner::instant());
        let first = handle.submit("first", vec![]).unwrap();
        let _ = collect_until_terminal(&handle, Duration::from_secs(5));

        let second = handle.submit("second", vec![]).unwrap();
        // A stale interrupt naming the already-finished first task.
        handle.interrupt(first);
        let events = collect_until_terminal(&handle, Duration::from_secs(5));
        let Some(Payload::TaskFinished { outcome, .. }) = events.last().map(|e| e.payload.clone())
        else {
            panic!("expected a terminal event");
        };
        assert_ne!(
            outcome,
            Outcome::Cancelled,
            "a stale interrupt for a different task must not cancel {second:?}'s own task"
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
                    if let Payload::TaskFinished { outcome, .. } = &event.payload {
                        // Accepted means it runs: a worker that had
                        // already reported must not still occupy the slot.
                        assert!(
                            matches!(outcome, Outcome::Completed { .. }),
                            "round {round}: an accepted submission was refused: {outcome:?}"
                        );
                        finished = true;
                    }
                }
            }
        }
        handle.shutdown();
    }

    /// Drains until `done` says stop, failing loudly on timeout.
    fn drain_until(
        handle: &CoreHandle,
        limit: Duration,
        mut done: impl FnMut(&CoreEvent) -> bool,
    ) -> Vec<CoreEvent> {
        let deadline = std::time::Instant::now() + limit;
        let mut all = Vec::new();
        while std::time::Instant::now() < deadline {
            for event in handle.drain_blocking(Duration::from_millis(20)) {
                let stop = done(&event);
                all.push(event);
                if stop {
                    return all;
                }
            }
        }
        panic!("condition not reached within {limit:?}; got {all:?}");
    }

    /// Verification row 4.8 (E8). Two tasks are both parked on approval.
    /// Approving A must run only A's call; B must still be waiting, not
    /// approved by A's decision and not executed.
    #[test]
    fn approving_one_task_never_runs_another_tasks_waiting_call() {
        let handle = start_pool(crate::runner::ApprovalRunner { tool: "run_shell" }, 2);
        let a = handle.submit("a", vec![]).unwrap();
        let b = handle.submit("b", vec![]).unwrap();

        let mut waiting = std::collections::HashMap::new();
        drain_until(&handle, Duration::from_secs(5), |e| {
            if let Payload::ApprovalRequired { call, .. } = &e.payload {
                waiting.insert(e.task, *call);
            }
            waiting.len() == 2
        });

        handle.approve(ApprovalDecision {
            task: a,
            call: waiting[&a],
            allow: true,
        });
        let after = drain_until(&handle, Duration::from_secs(5), |e| {
            e.task == a && e.is_terminal()
        });

        // The negative case is the whole test.
        assert!(
            after.iter().all(|e| e.task != b),
            "task B must emit nothing while still waiting; got {after:?}"
        );
        assert!(
            after
                .iter()
                .any(|e| e.task == a && matches!(e.payload, Payload::ToolCompleted { .. })),
            "task A's approved call must have run"
        );
        assert!(handle.is_busy(), "B still occupies a slot");

        handle.approve(ApprovalDecision {
            task: b,
            call: waiting[&b],
            allow: false,
        });
        let rest = drain_until(&handle, Duration::from_secs(5), |e| {
            e.task == b && e.is_terminal()
        });
        assert!(
            !rest
                .iter()
                .any(|e| matches!(e.payload, Payload::ToolCompleted { .. })),
            "B was denied, so its call must not have run"
        );
        handle.shutdown();
    }

    /// E8, interrupt half: interrupting A leaves B running to completion.
    #[test]
    fn interrupting_one_task_leaves_the_other_running() {
        let handle = start_pool(EchoRunner::slow(Duration::from_millis(20), 10), 2);
        let a = handle.submit("aaaa", vec![]).unwrap();
        let b = handle.submit("bbbb", vec![]).unwrap();
        handle.interrupt(a);

        let mut outcomes = std::collections::HashMap::new();
        drain_until(&handle, Duration::from_secs(5), |e| {
            if let Payload::TaskFinished { outcome, .. } = &e.payload {
                outcomes.insert(e.task, outcome.clone());
            }
            outcomes.len() == 2
        });
        assert_eq!(outcomes[&a], Outcome::Cancelled);
        assert!(
            matches!(outcomes[&b], Outcome::Completed { .. }),
            "B must not be cancelled by A's interrupt; got {:?}",
            outcomes[&b]
        );
        handle.shutdown();
    }

    /// Verification row 4.1: overlap proven from event order, not
    /// wall-clock. B starts before A finishes.
    #[test]
    fn two_tasks_in_a_pool_overlap() {
        let handle = start_pool(EchoRunner::slow(Duration::from_millis(20), 10), 2);
        let a = handle.submit("aaaa", vec![]).unwrap();
        let b = handle.submit("bbbb", vec![]).unwrap();

        let mut finished = 0;
        let events = drain_until(&handle, Duration::from_secs(5), |e| {
            if e.is_terminal() {
                finished += 1;
            }
            finished == 2
        });
        let pos = |task, started: bool| {
            events
                .iter()
                .position(|e| {
                    e.task == task
                        && if started {
                            matches!(e.payload, Payload::TaskStarted { .. })
                        } else {
                            e.is_terminal()
                        }
                })
                .unwrap()
        };
        assert!(
            pos(b, true) < pos(a, false),
            "B must start before A finishes"
        );
        assert!(
            pos(a, true) < pos(b, false),
            "A must start before B finishes"
        );
        handle.shutdown();
    }

    #[test]
    fn a_full_pool_refuses_the_next_submission() {
        let handle = start_pool(EchoRunner::slow(Duration::from_millis(50), 50), 2);
        let a = handle.submit("a", vec![]).unwrap();
        handle.submit("b", vec![]).unwrap();
        assert_eq!(
            handle.submit("c", vec![]),
            Err(SubmitError::Busy { running: a })
        );
        handle.shutdown();
    }

    /// Announces a call, then holds off waiting until the test opens the
    /// gate, so the test controls exactly when the worker parks.
    struct GatedApprovalRunner {
        gate: std::sync::Mutex<Receiver<()>>,
    }

    impl Runner for GatedApprovalRunner {
        fn describe(&self) -> (String, String) {
            ("test".to_string(), "gated".to_string())
        }
        fn run(&self, _turn: &UserTurn, _history: &[Message], sink: &mut TurnSink) -> Outcome {
            let Some(call) = sink.announce_approval("gated_tool", "x") else {
                return Outcome::Failed {
                    detail: "disconnected".into(),
                };
            };
            let _ = self.gate.lock().unwrap().recv();
            let allowed = sink.await_decision(call, "gated_tool").is_some();
            Outcome::Completed {
                final_text: format!("allowed={allowed}"),
            }
        }
    }

    /// Verification row 4.9 (E9), forced deterministically rather than
    /// by looping: three stray decisions and then the real one are all
    /// delivered *before* the worker parks. The old 1-slot channel kept
    /// the first stray and silently dropped the rest, the real answer
    /// included, so the worker waited forever.
    #[test]
    fn a_reply_delivered_before_the_worker_parks_is_still_observed() {
        let (open_gate, gate) = sync_channel(1);
        let handle = start(GatedApprovalRunner {
            gate: std::sync::Mutex::new(gate),
        });
        let task = handle.submit("go", vec![]).unwrap();

        let mut call = None;
        drain_until(&handle, Duration::from_secs(5), |e| {
            if let Payload::ApprovalRequired { call: c, .. } = &e.payload {
                call = Some(*c);
            }
            call.is_some()
        });
        let call = call.unwrap();

        for _ in 0..3 {
            handle.approve(ApprovalDecision {
                task,
                call: ToolCallId::from_raw(u64::MAX),
                allow: true,
            });
        }
        handle.approve(ApprovalDecision {
            task,
            call,
            allow: true,
        });
        // Lets the supervisor forward all four before the worker parks.
        // If it were ever slower, this test loses detection power but
        // can never fail falsely: the correct code passes either way.
        std::thread::sleep(Duration::from_millis(100));
        open_gate.send(()).unwrap();

        let events = drain_until(&handle, Duration::from_secs(5), |e| e.is_terminal());
        let outcome = events.iter().find_map(|e| match &e.payload {
            Payload::TaskFinished { outcome, .. } => Some(outcome.clone()),
            _ => None,
        });
        assert_eq!(
            outcome,
            Some(Outcome::Completed {
                final_text: "allowed=true".into()
            })
        );
        handle.shutdown();
    }
}
