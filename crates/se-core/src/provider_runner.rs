//! The runner that drives a real model provider.
//!
//! This is the Phase 2 substitution the seam was drawn for: it replaces
//! [`EchoRunner`](crate::EchoRunner) and no interface code changes,
//! because an interface only ever sees [`CoreEvent`](se_protocol::CoreEvent)s.
//!
//! What it adds beyond forwarding deltas:
//!
//! * **Time to first token is measured separately** from generation. The
//!   blueprint is explicit that a remote model must never be benchmarked
//!   by timing only the final response, and this is where that number
//!   actually comes from.
//! * **Cancellation reaches the socket.** The provider polls the same
//!   flag between frames, so interrupting stops the connection rather
//!   than politely waiting for a paid-for answer nobody will read.
//! * **Usage is forwarded only if the provider sent it.** Nothing is
//!   estimated; a provider that reports nothing produces
//!   `UsageSource::Unavailable`, which an interface shows as "not
//!   reported".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use se_permission::{
    Approved, ExecutionContext, Gateway, NonceLedger, NullAuditSink, Request as PermRequest,
    Ruleset,
};
use se_protocol::{Outcome, Stage, Usage, UsageSource, UserTurn};
use se_provider::{
    ChatRequest, Message, ProviderError, Registry, Role, StreamEvent, TimeoutKind, ToolCallRecord,
};

use crate::sink::TurnSink;
use crate::tool_loop;
use crate::tools::Granted;
use crate::Runner;

/// `agent_definition_hash` identifies the agent body a token was
/// approved against (`SE_TARGET_ARCHITECTURE.md` §3a). There is no
/// agent/specialist system yet (Phase 3/4, explicitly out of scope
/// here) — `ProviderRunner` itself is the only thing issuing tool
/// calls, so its own identity stands in for an agent definition until
/// one exists. A real multi-agent catalogue entry, not a provider
/// runner, will own this once it exists.
const NATIVE_LOOP_AGENT_ID: &str = "native-tool-loop";
const NATIVE_LOOP_AGENT_DEFINITION_HASH: &str = "native-tool-loop-v1";

/// Hard ceiling on how many model/tool round trips one turn may make,
/// independent of the turn deadline: the deadline bounds wall time, but
/// a provider replying instantly with another tool call every time
/// would otherwise loop forever within that same budget.
const MAX_TOOL_ROUNDS: usize = 20;

/// How many tokens to allow in a reply, until this is configurable.
const MAX_TOKENS: u32 = 4096;

/// Default absolute bound on one turn, independent of the provider's
/// own idle-read timeout.
///
/// The two bounds answer different questions: the provider's idle
/// timeout detects a dead connection; this detects a connection that
/// stays alive and keeps sending *something* far longer than a turn
/// should reasonably take. Without it, a provider that trickles one
/// byte every few seconds forever would never time out at all.
const DEFAULT_TURN_DEADLINE: Duration = Duration::from_secs(600);

/// Instructions sent with every request.
///
/// Deliberately tiny. The blueprint's fast path is explicit that an
/// ordinary message must not drag in a specialist catalogue, a
/// repository tree or every tool description -- that is how a greeting
/// comes to cost ninety seconds. Phase 5 adds specialist instructions,
/// loaded only when a specialist is actually dispatched.
const SYSTEM_PROMPT: &str = "You are SE, an engineering assistant. Be concise and concrete.";

pub struct ProviderRunner {
    registry: Registry,
    provider_id: String,
    model: String,
    turn_deadline: Duration,
    /// The policy a tool call is checked against before a human is
    /// ever asked. Defaults to an empty [`Ruleset`], which (per that
    /// type's own documented default) resolves every call to `Ask`,
    /// never to a silent `Allow` — an unconfigured gateway must not
    /// grant anything on its own.
    ruleset: Ruleset,
    /// Single-use nonces for every token this runner mints, across the
    /// whole turn (and, since `Runner::run` takes `&self` and one
    /// runner is shared across task workers, across every turn it ever
    /// runs). `NonceLedger`'s own interior `Mutex` is what makes this
    /// safe to share.
    ledger: NonceLedger,
}

impl ProviderRunner {
    /// Fails fast if the provider is unknown or unconfigured, so a turn
    /// is never started against something that cannot run it.
    pub fn new(provider_id: &str, model: &str) -> Result<Self, ProviderError> {
        let registry = Registry::new();
        let provider = registry.get(provider_id).ok_or_else(|| {
            ProviderError::NotConfigured(format!("`{provider_id}` is not a known provider"))
        })?;
        let status = provider.detect();
        if !status.configured {
            return Err(ProviderError::NotConfigured(format!(
                "`{provider_id}` is not configured: {}",
                status.detail
            )));
        }
        Ok(ProviderRunner {
            registry,
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            turn_deadline: DEFAULT_TURN_DEADLINE,
            ruleset: Ruleset::default(),
            ledger: NonceLedger::new(),
        })
    }

    /// Overrides the whole-turn deadline. Rejects a non-positive value
    /// at construction time rather than racing against an already-
    /// expired deadline on the first poll.
    pub fn with_turn_deadline(mut self, deadline: Duration) -> Result<Self, ProviderError> {
        if deadline.is_zero() {
            return Err(ProviderError::NotConfigured(
                "a turn deadline must be greater than zero".to_string(),
            ));
        }
        self.turn_deadline = deadline;
        Ok(self)
    }

    /// Supplies the policy tool calls are checked against. Not required:
    /// the default empty ruleset asks a human for everything, which is
    /// the safe starting point this crate does not relax on its own.
    pub fn with_ruleset(mut self, ruleset: Ruleset) -> Self {
        self.ruleset = ruleset;
        self
    }

    fn tool_definitions(&self) -> Vec<se_provider::ToolDefinition> {
        tool_loop::tool_definitions()
    }
}

/// How a fired whole-turn deadline and a possible cancellation resolve
/// against each other, as a decision over three booleans with nothing
/// else in scope.
///
/// A fired deadline and a real user cancellation reach `stream()` as
/// the same closure returning `true` (a provider has one cancellation
/// mechanism, not two), so they are indistinguishable from inside the
/// call. `cancelled` is the tie-breaker precisely because user
/// cancellation must always win over a deadline that happened to fire
/// in the same instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeadlinePrecedence {
    /// The deadline did not fire, or the user also cancelled and that
    /// takes precedence -- the caller's ordinary `Err`/`Ok` handling
    /// applies.
    NotApplicable,
    Failed,
    Incomplete,
}

fn deadline_precedence(
    deadline_fired: bool,
    cancelled: bool,
    answer_is_empty: bool,
) -> DeadlinePrecedence {
    if !deadline_fired || cancelled {
        return DeadlinePrecedence::NotApplicable;
    }
    if answer_is_empty {
        DeadlinePrecedence::Failed
    } else {
        DeadlinePrecedence::Incomplete
    }
}

impl Runner for ProviderRunner {
    fn describe(&self) -> (String, String) {
        (self.provider_id.clone(), self.model.clone())
    }

    fn run(&self, turn: &UserTurn, history: &[Message], sink: &mut TurnSink) -> Outcome {
        let Some(provider) = self.registry.get(&self.provider_id) else {
            return Outcome::Failed {
                detail: format!("`{}` is no longer registered", self.provider_id),
            };
        };
        if sink.start_turn(&self.provider_id, &self.model).is_err() {
            return Outcome::Failed {
                detail: "the interface disconnected".to_string(),
            };
        }

        // Context assembly is measured even though it is currently
        // trivial, so that it stays visible when it stops being trivial.
        // `messages` grows across round trips (an assistant tool-calls
        // turn, then one tool-result turn per call) as the loop below
        // runs; what the model sees this turn and what gets reported
        // back as the final answer never includes those intermediate
        // turns in cross-turn `history` -- only `engine.rs` deciding to
        // persist `Outcome::Completed`'s `final_text` does that.
        let mut messages: Vec<Message> = sink.timed(Stage::ContextBuild, || {
            let mut messages = Vec::with_capacity(history.len() + 2);
            messages.push(Message::text(Role::System, SYSTEM_PROMPT));
            messages.extend_from_slice(history);
            messages.push(Message::text(Role::User, turn.text.clone()));
            messages
        });

        let mut answer = String::new();
        let mut first_token_at: Option<Instant> = None;
        // Measured from the very first provider call of the turn, never
        // reset on a later round trip -- the brief's requirement that
        // time-to-first-token is not re-measured per round trip.
        let request_started = Instant::now();
        let mut total_usage: Option<Usage> = None;
        // One deadline for the whole turn, computed once; every round
        // trip below shares the same `turn_started` and `turn_deadline`,
        // so the bound is on the loop, not reset per model call.
        let turn_started = Instant::now();
        let turn_deadline = self.turn_deadline;
        let deadline_fired = Arc::new(AtomicBool::new(false));

        let mut round = 0usize;
        let outcome = loop {
            round += 1;
            if sink.is_cancelled() {
                break Outcome::Cancelled;
            }
            if round > MAX_TOOL_ROUNDS {
                break Outcome::Incomplete {
                    partial_text: answer,
                    reason: "the turn made too many tool-call round trips".to_string(),
                };
            }

            let request = ChatRequest {
                model: self.model.clone(),
                messages: messages.clone(),
                max_tokens: MAX_TOKENS,
                tools: self.tool_definitions(),
            };

            let mut round_answer = String::new();
            let mut finish_reason: Option<String> = None;
            let mut round_usage: Option<Usage> = None;
            let mut disconnected = false;
            let mut tool_calls: Vec<ToolCallRecord> = Vec::new();

            // Cloned rather than borrowed from `sink`, which is held
            // mutably below to report deltas.
            let cancelled = sink.cancel_flag();
            // Distinguishes "the deadline fired" from "the user
            // cancelled" after the fact: both reach the provider
            // through the same closure returning `true` (a provider has
            // one cancellation mechanism, not two), so this flag is the
            // only way to tell them apart once `stream` has returned.
            let deadline_fired_writer = deadline_fired.clone();
            let should_stop = move || {
                if cancelled.load(Ordering::SeqCst) {
                    return true;
                }
                if turn_started.elapsed() >= turn_deadline {
                    deadline_fired_writer.store(true, Ordering::SeqCst);
                    return true;
                }
                false
            };
            let result = provider.stream(&request, &should_stop, &mut |event| match event {
                StreamEvent::TextDelta(text) => {
                    if first_token_at.is_none() {
                        first_token_at = Some(Instant::now());
                    }
                    round_answer.push_str(&text);
                    if sink.text_delta(&text).is_err() {
                        disconnected = true;
                        return false;
                    }
                    true
                }
                StreamEvent::ReasoningDelta(text) => {
                    if first_token_at.is_none() {
                        first_token_at = Some(Instant::now());
                    }
                    if sink.reasoning_delta(&text).is_err() {
                        disconnected = true;
                        return false;
                    }
                    true
                }
                StreamEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                } => {
                    round_usage = Some(Usage {
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                        source: UsageSource::ProviderReported,
                    });
                    true
                }
                // A mid-stream provider error becomes a transport-level
                // failure via the `Err` the provider returns; nothing is
                // appended to `answer`, so a failed turn cannot present
                // a partial reply as the answer.
                StreamEvent::Error(_) => false,
                // Recorded for the outcome, not shown as content.
                StreamEvent::FinishReason(reason) => {
                    finish_reason = Some(reason);
                    true
                }
                StreamEvent::ToolCallRequested {
                    id,
                    name,
                    arguments,
                } => {
                    tool_calls.push(ToolCallRecord {
                        id,
                        name,
                        arguments,
                    });
                    true
                }
                StreamEvent::Done => true,
            });

            answer.push_str(&round_answer);
            if let Some(usage) = round_usage {
                total_usage = Some(match total_usage {
                    None => usage,
                    Some(prev) => merge_usage(prev, usage),
                });
            }

            // The tie-break is a pure function over three booleans (see
            // `deadline_precedence` below) specifically so every one of
            // its input combinations is a named row in a test, rather
            // than a property inferred from reading this `if`.
            match deadline_precedence(
                deadline_fired.load(Ordering::SeqCst),
                sink.is_cancelled(),
                answer.is_empty(),
            ) {
                DeadlinePrecedence::NotApplicable => {}
                DeadlinePrecedence::Failed => {
                    break Outcome::Failed {
                        detail: ProviderError::Timeout(TimeoutKind::WholeTurn).to_string(),
                    };
                }
                DeadlinePrecedence::Incomplete => {
                    break Outcome::Incomplete {
                        partial_text: answer,
                        reason: ProviderError::Timeout(TimeoutKind::WholeTurn).to_string(),
                    };
                }
            }

            match result {
                Ok(()) if disconnected => {
                    // A real answer may already have streamed before the
                    // interface disappeared; preserve it as Incomplete
                    // rather than discarding it as a bare failure.
                    break if answer.is_empty() {
                        Outcome::Failed {
                            detail: "the interface disconnected mid-answer".to_string(),
                        }
                    } else {
                        Outcome::Incomplete {
                            partial_text: answer,
                            reason: "the interface disconnected mid-answer".to_string(),
                        }
                    };
                }
                Ok(())
                    if !tool_calls.is_empty() && finish_reason.as_deref() == Some("tool_calls") =>
                {
                    // The model asked to run tools: carry its own
                    // tool-calls message, then exactly one tool-result
                    // message per call (including a denied or errored
                    // one -- the model must see why, not have the turn
                    // silently stop), then loop back with a fresh
                    // request rather than returning.
                    messages.push(Message::assistant_tool_calls(tool_calls.clone()));
                    let mut interrupted = false;
                    for call in &tool_calls {
                        if sink.is_cancelled() {
                            interrupted = true;
                            break;
                        }
                        let result_text = self.handle_tool_call(sink, call);
                        messages.push(Message::tool_result(call.id.clone(), result_text));
                    }
                    if interrupted {
                        break Outcome::Cancelled;
                    }
                    continue;
                }
                Ok(()) => {
                    // A provider that completed without ever saying why
                    // it stopped is accepted (many send only `[DONE]`);
                    // `length` and `content_filter` never reach here --
                    // the provider reports those as a Truncated/Policy
                    // `Err` so they are handled below, with the partial
                    // text preserved.
                    break match finish_reason.as_deref() {
                        None | Some("stop") | Some("end_turn") => {
                            Outcome::Completed { final_text: answer }
                        }
                        Some(other) => Outcome::Incomplete {
                            partial_text: answer,
                            reason: format!("the provider stopped early ({other})"),
                        },
                    };
                }
                Err(ProviderError::Cancelled) => break Outcome::Cancelled,
                // Truncation and any other mid-stream failure both
                // preserve whatever already streamed, as Incomplete
                // rather than Failed -- the distinction the outcome type
                // exists to make. Only a failure with nothing to
                // preserve (e.g. Auth rejected before the first byte) is
                // a bare Failed.
                Err(e) if !answer.is_empty() => {
                    break Outcome::Incomplete {
                        partial_text: answer,
                        reason: e.to_string(),
                    };
                }
                Err(e) => {
                    break Outcome::Failed {
                        detail: e.to_string(),
                    };
                }
            }
        };

        // Recorded once, after every round trip this turn made --
        // whichever one actually produced the first token (or none
        // did), not re-measured per round.
        if let Some(at) = first_token_at {
            sink.record(se_protocol::Span {
                stage: Stage::TimeToFirstToken,
                duration: at.saturating_duration_since(request_started),
            });
        } else {
            sink.record(se_protocol::Span {
                stage: Stage::TimeToFirstToken,
                duration: request_started.elapsed(),
            });
        }
        // Only ever the provider's own figures, summed across every
        // round trip that reported any.
        if let Some(usage) = total_usage {
            let _ = sink.usage(usage);
        }

        outcome
    }
}

/// Sums two reported usage figures field by field. A field absent on
/// both sides stays absent; present on only one side is carried through
/// unchanged -- summing is never invented across a gap the provider
/// itself left silent.
fn merge_usage(a: Usage, b: Usage) -> Usage {
    fn add(x: Option<u64>, y: Option<u64>) -> Option<u64> {
        match (x, y) {
            (None, None) => None,
            (Some(x), None) => Some(x),
            (None, Some(y)) => Some(y),
            (Some(x), Some(y)) => Some(x + y),
        }
    }
    Usage {
        input_tokens: add(a.input_tokens, b.input_tokens),
        output_tokens: add(a.output_tokens, b.output_tokens),
        cached_input_tokens: add(a.cached_input_tokens, b.cached_input_tokens),
        source: UsageSource::ProviderReported,
    }
}

impl ProviderRunner {
    /// Runs one tool call end to end: ruleset check, human approval if
    /// the ruleset defers, execution, and the sink events that make it
    /// visible. Returns the text to feed back to the model as that
    /// call's `Role::Tool` result -- a denial or an error included,
    /// since the model must see why a call did not run rather than have
    /// the turn silently stop.
    ///
    /// The approval mapping, chosen here because `se-permission`
    /// deliberately leaves it to the caller:
    /// * `Decision::Deny` is checked *before* any human is asked, and
    ///   is final -- a ruleset denial can never be overridden by a
    ///   human's "yes", because the human is never shown the request at
    ///   all.
    /// * `Decision::Allow` (a ruleset that pre-approves this exact
    ///   call) also never asks a human; the token it already minted is
    ///   used directly.
    /// * `Decision::Ask` is the only case that reaches
    ///   `TurnSink::request_approval`. A human's "yes" is then minted
    ///   into a token via `Gateway::approve` (the documented "approved
    ///   out of band" path); "no", a cancellation, or a disconnected
    ///   interface (`request_approval` returning `None`, which does not
    ///   distinguish the three) is always treated as a refusal -- a
    ///   withheld decision is never permission.
    fn handle_tool_call(&self, sink: &mut TurnSink, call: &ToolCallRecord) -> String {
        let parsed = match tool_loop::parse_tool_call(&call.name, &call.arguments) {
            Ok(p) => p,
            Err(reason) => {
                return format!("tool call rejected before it ran: {reason}");
            }
        };
        let resource = match tool_loop::resolve_identity(&parsed) {
            Ok(r) => r,
            Err(e) => {
                return format!("tool call rejected before it ran: {e}");
            }
        };

        let req = PermRequest {
            agent_id: NATIVE_LOOP_AGENT_ID.to_string(),
            agent_definition_hash: NATIVE_LOOP_AGENT_DEFINITION_HASH.to_string(),
            session: sink.session(),
            task: sink.task(),
            tool_name: call.name.clone(),
            tool_schema_version: 1,
            resource: resource.clone(),
            resource_scope: parsed.resource_scope.clone(),
            cwd: parsed.cwd.clone(),
            env_subset: Vec::new(),
            shown_to_human: parsed.display.clone(),
        };
        let gateway = Gateway::new(&self.ruleset, &self.ledger, &NullAuditSink);

        let (id, approved): (se_protocol::ToolCallId, Box<Approved>) = match gateway.decide(&req) {
            se_permission::Decision::Deny { reason } => {
                let id = se_protocol::ToolCallId::next();
                let _ = sink.tool_requested(id, &call.name, &parsed.display);
                let _ = sink.tool_completed(
                    id,
                    &call.name,
                    false,
                    &format!("denied by policy: {reason}"),
                );
                return format!("denied by policy: {reason}");
            }
            se_permission::Decision::Allow(token) => {
                let id = se_protocol::ToolCallId::next();
                if sink
                    .tool_requested(id, &call.name, &parsed.display)
                    .is_err()
                {
                    return "the interface disconnected before this call could run".to_string();
                }
                (id, token)
            }
            se_permission::Decision::Ask => {
                let Some(id) = sink.request_approval(&call.name, &parsed.display) else {
                    return "denied: not approved".to_string();
                };
                if sink
                    .tool_requested(id, &call.name, &parsed.display)
                    .is_err()
                {
                    return "the interface disconnected before this call could run".to_string();
                }
                if sink.is_cancelled() {
                    let _ = sink.tool_completed(id, &call.name, false, "cancelled");
                    return "cancelled before this call ran".to_string();
                }
                (id, gateway.approve(&req))
            }
        };

        let ctx = ExecutionContext {
            agent_id: req.agent_id.clone(),
            agent_definition_hash: req.agent_definition_hash.clone(),
            session: req.session,
            task: req.task,
            tool_name: req.tool_name.clone(),
            tool_schema_version: req.tool_schema_version,
            resource,
            cwd: req.cwd.clone(),
            env_subset: req.env_subset.clone(),
        };
        let granted = match Granted::new(*approved, ctx, &self.ledger) {
            Ok(g) => g,
            Err(e) => {
                let detail = format!("token verification failed at execution time: {e:?}");
                let _ = sink.tool_completed(id, &call.name, false, &detail);
                return detail;
            }
        };

        let outcome = tool_loop::execute(&granted, parsed.action);
        match outcome {
            Ok(preview) => {
                let _ = sink.tool_completed(id, &call.name, true, &preview);
                preview
            }
            Err(detail) => {
                let _ = sink.tool_completed(id, &call.name, false, &detail);
                format!("tool error: {detail}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_protocol::Payload;
    use se_provider::{ModelInfo, ModelProvider, Service, Status};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Every one of the eight (deadline, cancelled, answer-empty)
    /// combinations, named -- the gap the Phase 0 security review
    /// found: the precedence logic existed, but no test had ever set
    /// both a deadline and a cancellation flag at once.
    #[test]
    fn deadline_precedence_truth_table() {
        let cases = [
            // (deadline_fired, cancelled, answer_is_empty, expected)
            (false, false, false, DeadlinePrecedence::NotApplicable),
            (false, false, true, DeadlinePrecedence::NotApplicable),
            (false, true, false, DeadlinePrecedence::NotApplicable),
            (false, true, true, DeadlinePrecedence::NotApplicable),
            // The deadline fired but cancellation still wins, with an
            // empty or a partial answer -- the exact race the review
            // flagged as having zero coverage.
            (true, true, false, DeadlinePrecedence::NotApplicable),
            (true, true, true, DeadlinePrecedence::NotApplicable),
            (true, false, true, DeadlinePrecedence::Failed),
            (true, false, false, DeadlinePrecedence::Incomplete),
        ];
        for (deadline_fired, cancelled, answer_is_empty, expected) in cases {
            assert_eq!(
                deadline_precedence(deadline_fired, cancelled, answer_is_empty),
                expected,
                "deadline_fired={deadline_fired} cancelled={cancelled} answer_is_empty={answer_is_empty}"
            );
        }
    }
    use std::sync::Arc;
    use std::time::Duration;

    /// A provider under the test's control: it streams what it is told
    /// to, counts how many requests it received, and can be made slow.
    struct ScriptedProvider {
        frames: Vec<StreamEvent>,
        per_frame_delay: Duration,
        requests: Arc<AtomicUsize>,
        /// Fails before streaming anything -- for "nothing happened"
        /// cases (e.g. Auth rejected up front).
        fail_with: Option<ProviderError>,
        /// Streams every frame in `frames`, *then* fails -- for "a real
        /// answer started but did not finish" cases.
        fail_after: Option<ProviderError>,
        /// Ignores `frames` and streams forever (one delta every
        /// `per_frame_delay`) until cancelled -- for deadline tests,
        /// where the point is that nothing *else* ever stops it.
        never_stops: bool,
    }

    impl ModelProvider for ScriptedProvider {
        fn id(&self) -> &str {
            "scripted"
        }
        fn display_name(&self) -> &str {
            "Scripted"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_SCRIPTED_KEY"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "scripted".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            _request: &ChatRequest,
            cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            if let Some(e) = &self.fail_with {
                return Err(e.clone());
            }
            if self.never_stops {
                let mut i = 0usize;
                loop {
                    if cancelled() {
                        return Err(ProviderError::Cancelled);
                    }
                    if !self.per_frame_delay.is_zero() {
                        std::thread::sleep(self.per_frame_delay);
                    }
                    // Rechecked after the wait, as a real provider does
                    // before putting more bytes on the wire.
                    if cancelled() {
                        return Err(ProviderError::Cancelled);
                    }
                    if !on_event(StreamEvent::TextDelta(format!("{i} "))) {
                        return Err(ProviderError::Cancelled);
                    }
                    i += 1;
                }
            }
            for frame in &self.frames {
                if cancelled() {
                    return Err(ProviderError::Cancelled);
                }
                if !self.per_frame_delay.is_zero() {
                    std::thread::sleep(self.per_frame_delay);
                }
                if !on_event(frame.clone()) {
                    return Err(ProviderError::Cancelled);
                }
            }
            match &self.fail_after {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    const SCRIPTED: Service = Service {
        id: "scripted",
        display_name: "Scripted",
        key_var: "SE_TEST_SCRIPTED_KEY",
        default_base_url: "http://127.0.0.1:1",
        base_url_var: "SE_TEST_SCRIPTED_BASE_URL",
        key_required: true,
    };

    fn runner_with(
        frames: Vec<StreamEvent>,
        delay: Duration,
        fail_with: Option<ProviderError>,
    ) -> (ProviderRunner, Arc<AtomicUsize>) {
        scripted_runner(frames, delay, fail_with, None, false)
    }

    /// Streams `frames` and then fails -- "a real answer started but did
    /// not finish", which is what distinguishes Incomplete from Failed.
    fn runner_failing_after(
        frames: Vec<StreamEvent>,
        fail_after: ProviderError,
    ) -> (ProviderRunner, Arc<AtomicUsize>) {
        scripted_runner(frames, Duration::ZERO, None, Some(fail_after), false)
    }

    /// Streams forever until something stops it -- so a deadline test is
    /// measuring the deadline, not the end of a script.
    fn runner_that_never_stops(delay: Duration) -> (ProviderRunner, Arc<AtomicUsize>) {
        scripted_runner(Vec::new(), delay, None, None, true)
    }

    fn scripted_runner(
        frames: Vec<StreamEvent>,
        delay: Duration,
        fail_with: Option<ProviderError>,
        fail_after: Option<ProviderError>,
        never_stops: bool,
    ) -> (ProviderRunner, Arc<AtomicUsize>) {
        let requests = Arc::new(AtomicUsize::new(0));
        let mut registry = Registry::empty();
        registry.register(Box::new(ScriptedProvider {
            frames,
            per_frame_delay: delay,
            requests: requests.clone(),
            fail_with,
            fail_after,
            never_stops,
        }));
        let _ = SCRIPTED;
        (
            ProviderRunner {
                registry,
                provider_id: "scripted".into(),
                model: "scripted-model".into(),
                turn_deadline: DEFAULT_TURN_DEADLINE,
                ruleset: Ruleset::default(),
                ledger: NonceLedger::new(),
            },
            requests,
        )
    }

    fn run(runner: ProviderRunner, text: &str) -> (Outcome, Vec<Payload>) {
        let core = crate::start(runner);
        core.submit(text, vec![]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut payloads = Vec::new();
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                let terminal = event.is_terminal();
                payloads.push(event.payload.clone());
                if terminal {
                    core.shutdown();
                    let Payload::TaskFinished { outcome, .. } = event.payload else {
                        unreachable!()
                    };
                    return (outcome, payloads);
                }
            }
        }
        panic!("the turn never finished");
    }

    /// Like [`run`], but auto-answers every approval request with
    /// `allow` as it arrives, for tests about the tool loop rather than
    /// about approval itself.
    fn run_auto_approving(
        runner: ProviderRunner,
        text: &str,
        allow: bool,
    ) -> (Outcome, Vec<Payload>) {
        let core = crate::start(runner);
        core.submit(text, vec![]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut payloads = Vec::new();
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                if let Payload::ApprovalRequired { call, .. } = &event.payload {
                    core.approve(se_protocol::ApprovalDecision { call: *call, allow });
                }
                let terminal = event.is_terminal();
                payloads.push(event.payload.clone());
                if terminal {
                    core.shutdown();
                    let Payload::TaskFinished { outcome, .. } = event.payload else {
                        unreachable!()
                    };
                    return (outcome, payloads);
                }
            }
        }
        panic!("the turn never finished");
    }

    /// A provider that requests one tool call on its first request
    /// (one that the five real tools recognise: `write`), then answers
    /// plainly on every later request -- so the loop's second round
    /// trip is exercised with a real model-shaped request/response,
    /// not a stub.
    struct ToolCallingProvider {
        requests_seen: Arc<std::sync::Mutex<Vec<ChatRequest>>>,
        tool_name: String,
        arguments: String,
    }

    impl ModelProvider for ToolCallingProvider {
        fn id(&self) -> &str {
            "toolcalling"
        }
        fn display_name(&self) -> &str {
            "ToolCalling"
        }
        fn env_var(&self) -> &str {
            "SE_TEST_TOOLCALLING_KEY"
        }
        fn detect(&self) -> Status {
            Status {
                configured: true,
                detail: "toolcalling".into(),
                source: se_provider::Source::Environment,
            }
        }
        fn stream(
            &self,
            request: &ChatRequest,
            _cancelled: &dyn Fn() -> bool,
            on_event: &mut dyn FnMut(StreamEvent) -> bool,
        ) -> Result<(), ProviderError> {
            let round = {
                let mut seen = self.requests_seen.lock().unwrap();
                seen.push(request.clone());
                seen.len()
            };
            if round == 1 {
                on_event(StreamEvent::ToolCallRequested {
                    id: "call_1".into(),
                    name: self.tool_name.clone(),
                    arguments: self.arguments.clone(),
                });
                on_event(StreamEvent::FinishReason("tool_calls".into()));
            } else {
                on_event(StreamEvent::TextDelta("acknowledged".into()));
                on_event(StreamEvent::FinishReason("stop".into()));
            }
            on_event(StreamEvent::Done);
            Ok(())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn tool_calling_runner(
        tool_name: &str,
        arguments: String,
        ruleset: Ruleset,
    ) -> (ProviderRunner, Arc<std::sync::Mutex<Vec<ChatRequest>>>) {
        let requests_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut registry = Registry::empty();
        registry.register(Box::new(ToolCallingProvider {
            requests_seen: requests_seen.clone(),
            tool_name: tool_name.to_string(),
            arguments,
        }));
        let runner = ProviderRunner {
            registry,
            provider_id: "toolcalling".into(),
            model: "toolcalling-model".into(),
            turn_deadline: DEFAULT_TURN_DEADLINE,
            ruleset,
            ledger: NonceLedger::new(),
        };
        (runner, requests_seen)
    }

    /// End to end through a real `CoreHandle`: a tool call the ruleset
    /// defers (`Ask`) and a human approves actually runs the real
    /// `write` tool, and the second round trip carries the tool's
    /// result back to the model as a `Role::Tool` message.
    #[test]
    fn an_approved_tool_call_runs_the_real_tool_and_feeds_the_result_back() {
        let dir =
            std::env::temp_dir().join(format!("se-core-loop-approved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("out.txt");
        let args =
            serde_json::json!({"path": file.to_string_lossy(), "content": "hello from the model"})
                .to_string();

        let (runner, requests_seen) = tool_calling_runner(
            "write",
            args,
            Ruleset::from_rules(vec![se_permission::Rule::allow_all()]),
        );
        let (outcome, _payloads) = run_auto_approving(runner, "please write the file", true);

        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "hello from the model",
            "the real write tool must have run"
        );
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "acknowledged".into()
            }
        );
        let seen = requests_seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one round trip per model/tool exchange");
        let second = &seen[1];
        assert!(
            second
                .messages
                .iter()
                .any(|m| m.role == Role::Assistant && !m.tool_calls.is_empty()),
            "the assistant's own tool-calls message must be replayed: {:?}",
            second.messages
        );
        assert!(
            second.messages.iter().any(|m| m.role == Role::Tool
                && m.tool_call_id.as_deref() == Some("call_1")
                && m.content.contains("wrote file")),
            "the tool's result must be fed back by the same call id: {:?}",
            second.messages
        );
    }

    /// A denial must not execute the tool, and the model must be told
    /// why -- the turn continues, it is not silently abandoned.
    #[test]
    fn a_denied_tool_call_never_runs_and_the_model_is_told_it_was_denied() {
        let dir = std::env::temp_dir().join(format!("se-core-loop-denied-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("should-not-exist.txt");
        let args =
            serde_json::json!({"path": file.to_string_lossy(), "content": "nope"}).to_string();

        // The default `Ruleset` resolves to `Ask` for everything, so
        // this call actually reaches the human approval gate rather
        // than being auto-allowed by policy before it gets there.
        let (runner, requests_seen) = tool_calling_runner("write", args, Ruleset::default());
        let (outcome, _payloads) = run_auto_approving(runner, "please write the file", false);

        assert!(
            !file.exists(),
            "a denied write must never touch the filesystem"
        );
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "acknowledged".into()
            },
            "the turn continues after a denial rather than stopping"
        );
        let seen = requests_seen.lock().unwrap();
        let second = &seen[1];
        assert!(
            second
                .messages
                .iter()
                .any(|m| m.role == Role::Tool && m.content.to_lowercase().contains("denied")),
            "the model must be told the call was denied: {:?}",
            second.messages
        );
    }

    /// Deny wins: a ruleset that denies this tool is never overridden
    /// by a human saying yes, because the human is never asked at all.
    #[test]
    fn a_ruleset_deny_overrides_a_human_who_always_says_yes() {
        let dir =
            std::env::temp_dir().join(format!("se-core-loop-ruleset-deny-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("should-not-exist.txt");
        let args =
            serde_json::json!({"path": file.to_string_lossy(), "content": "nope"}).to_string();

        let (runner, _requests_seen) = tool_calling_runner(
            "write",
            args,
            Ruleset::from_rules(vec![se_permission::Rule::deny_tool("write")]),
        );
        // `allow: true` on every approval -- the point is that this
        // never gets asked, so always saying yes must not matter.
        let (outcome, payloads) = run_auto_approving(runner, "please write the file", true);

        assert!(
            !file.exists(),
            "a ruleset denial must never touch the filesystem"
        );
        assert!(
            !payloads
                .iter()
                .any(|p| matches!(p, Payload::ApprovalRequired { .. })),
            "a ruleset deny must be final before a human is ever asked: {payloads:?}"
        );
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "acknowledged".into()
            }
        );
    }

    #[test]
    fn a_greeting_causes_exactly_one_model_request() {
        // The core request gate: no planner pre-flight, no classifier,
        // no specialist swarm. One turn, one request.
        let (runner, requests) = runner_with(
            vec![StreamEvent::TextDelta("Hello".into()), StreamEvent::Done],
            Duration::ZERO,
            None,
        );
        let (outcome, _p) = run(runner, "hi");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "a greeting must cost exactly one model request"
        );
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "Hello".into()
            }
        );
    }

    #[test]
    fn streamed_deltas_reach_the_interface_and_build_the_answer() {
        let (runner, _r) = runner_with(
            vec![
                StreamEvent::TextDelta("one ".into()),
                StreamEvent::TextDelta("two ".into()),
                StreamEvent::TextDelta("three".into()),
                StreamEvent::Done,
            ],
            Duration::ZERO,
            None,
        );
        let (outcome, payloads) = run(runner, "count");
        let streamed: String = payloads
            .iter()
            .filter_map(|p| match p {
                Payload::Delta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "one two three");
        // Same invariant as the echo runner: the transcript and the
        // reported answer are the same bytes.
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "one two three".into()
            }
        );
    }

    #[test]
    fn reasoning_is_streamed_as_reasoning_not_as_the_answer() {
        let (runner, _r) = runner_with(
            vec![
                StreamEvent::ReasoningDelta("let me think".into()),
                StreamEvent::TextDelta("42".into()),
                StreamEvent::Done,
            ],
            Duration::ZERO,
            None,
        );
        let (outcome, payloads) = run(runner, "the answer?");
        let kinds: Vec<se_protocol::DeltaKind> = payloads
            .iter()
            .filter_map(|p| match p {
                Payload::Delta { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                se_protocol::DeltaKind::Reasoning,
                se_protocol::DeltaKind::Text
            ]
        );
        // Thinking must not end up in the answer.
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "42".into()
            }
        );
    }

    #[test]
    fn provider_reported_usage_is_forwarded_and_labelled_as_theirs() {
        let (runner, _r) = runner_with(
            vec![
                StreamEvent::TextDelta("x".into()),
                StreamEvent::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(1),
                    cached_input_tokens: Some(4),
                },
                StreamEvent::Done,
            ],
            Duration::ZERO,
            None,
        );
        let (_outcome, payloads) = run(runner, "hi");
        let usage = payloads
            .iter()
            .find_map(|p| match p {
                Payload::UsageUpdated(u) => Some(*u),
                _ => None,
            })
            .expect("usage forwarded");
        assert_eq!(usage.source, UsageSource::ProviderReported);
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(usage.cached_input_tokens, Some(4));
    }

    #[test]
    fn a_provider_that_reports_no_usage_produces_no_invented_figures() {
        let (runner, _r) = runner_with(
            vec![StreamEvent::TextDelta("x".into()), StreamEvent::Done],
            Duration::ZERO,
            None,
        );
        let (_outcome, payloads) = run(runner, "hi");
        let any_usage = payloads
            .iter()
            .any(|p| matches!(p, Payload::UsageUpdated(_)));
        assert!(
            !any_usage,
            "silence from the provider must stay silence, not become a zero"
        );
    }

    #[test]
    fn time_to_first_token_is_measured_separately_from_generation() {
        let (runner, _r) = runner_with(
            vec![
                StreamEvent::TextDelta("slow".into()),
                StreamEvent::TextDelta(" answer".into()),
                StreamEvent::Done,
            ],
            Duration::from_millis(40),
            None,
        );
        let (_outcome, payloads) = run(runner, "hi");
        let Some(Payload::TaskFinished { timings, .. }) = payloads.last() else {
            panic!("expected a terminal event");
        };
        let ttft = timings
            .of(Stage::TimeToFirstToken)
            .expect("ttft must be recorded");
        assert!(
            ttft >= Duration::from_millis(30),
            "ttft should reflect the real wait: {ttft:?}"
        );
        // And it is not the whole turn: two frames were delayed, so
        // generation is longer than the first-token wait.
        let generation = timings.of(Stage::Generation).expect("generation recorded");
        assert!(
            generation > ttft,
            "generation {generation:?} must exceed ttft {ttft:?}"
        );
    }

    #[test]
    fn interrupting_a_stream_reports_cancelled_not_a_partial_answer() {
        let (runner, _r) = runner_with(
            (0..200)
                .map(|i| StreamEvent::TextDelta(format!("{i} ")))
                .collect(),
            Duration::from_millis(10),
            None,
        );
        let core = crate::start(runner);
        let task = core.submit("a long answer", vec![]).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        core.interrupt(task);

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            for event in core.drain_blocking(Duration::from_millis(20)) {
                if let Payload::TaskFinished { outcome, .. } = &event.payload {
                    assert_eq!(*outcome, Outcome::Cancelled);
                    core.shutdown();
                    return;
                }
            }
        }
        panic!("an interrupted stream must still finish");
    }

    #[test]
    fn a_provider_error_is_reported_verbatim_rather_than_as_an_empty_answer() {
        let (runner, _r) = runner_with(
            Vec::new(),
            Duration::ZERO,
            Some(ProviderError::Protocol(
                "nvidia returned HTTP 401: invalid api key".into(),
            )),
        );
        let (outcome, _p) = run(runner, "hi");
        match outcome {
            Outcome::Failed { detail } => {
                assert!(detail.contains("401"), "{detail}");
                assert!(detail.contains("invalid api key"), "{detail}");
            }
            other => panic!("expected a reported failure, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_provider_is_refused_before_a_turn_starts() {
        // `ProviderRunner` holds boxed trait objects and is not
        // `Debug`, so the result is matched rather than unwrapped.
        match ProviderRunner::new("not-a-provider", "m") {
            Err(ProviderError::NotConfigured(d)) => {
                assert!(d.contains("not-a-provider"), "{d}")
            }
            Err(other) => panic!("expected NotConfigured, got {other:?}"),
            Ok(_) => panic!("an unknown provider must be refused"),
        }
    }

    #[test]
    fn an_unconfigured_provider_is_refused_with_the_reason() {
        // `custom` has no default endpoint and no credential in a test
        // environment, so it must refuse rather than start a turn that
        // cannot possibly run.
        std::env::remove_var("SE_CUSTOM_API_KEY");
        std::env::remove_var("SE_CUSTOM_BASE_URL");
        match ProviderRunner::new("custom", "m") {
            Err(ProviderError::NotConfigured(d)) => assert!(d.contains("custom"), "{d}"),
            Err(other) => panic!("expected NotConfigured, got {other:?}"),
            Ok(_) => panic!("an unconfigured provider must be refused"),
        }
    }

    /// A conversation, not a sequence of amnesiac requests.
    ///
    /// The defect this covers: the runner used to send only the system
    /// prompt and the current message, so the second turn could not see
    /// the first -- while the phase table claimed a "real session".
    #[test]
    fn a_second_turn_sends_the_first_exchange_back_to_the_model() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let recorder = seen.clone();

        struct Recorder {
            seen: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        }
        impl ModelProvider for Recorder {
            fn id(&self) -> &str {
                "recorder"
            }
            fn display_name(&self) -> &str {
                "Recorder"
            }
            fn env_var(&self) -> &str {
                "SE_TEST_RECORDER_KEY"
            }
            fn detect(&self) -> Status {
                Status {
                    configured: true,
                    detail: "recorder".into(),
                    source: se_provider::Source::Environment,
                }
            }
            fn stream(
                &self,
                request: &ChatRequest,
                _cancelled: &dyn Fn() -> bool,
                on_event: &mut dyn FnMut(StreamEvent) -> bool,
            ) -> Result<(), ProviderError> {
                self.seen.lock().unwrap().push(
                    request
                        .messages
                        .iter()
                        .map(|m| format!("{:?}:{}", m.role, m.content))
                        .collect(),
                );
                on_event(StreamEvent::TextDelta("ok".into()));
                on_event(StreamEvent::Done);
                Ok(())
            }
            fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
                Ok(Vec::new())
            }
        }

        let mut registry = Registry::empty();
        registry.register(Box::new(Recorder { seen: recorder }));
        let runner = ProviderRunner {
            registry,
            provider_id: "recorder".into(),
            model: "m".into(),
            turn_deadline: DEFAULT_TURN_DEADLINE,
            ruleset: Ruleset::default(),
            ledger: NonceLedger::new(),
        };

        let core = crate::start(runner);
        for request in ["first question", "second question"] {
            core.submit(request, vec![]).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut done = false;
            while !done && std::time::Instant::now() < deadline {
                for event in core.drain_blocking(Duration::from_millis(20)) {
                    if event.is_terminal() {
                        done = true;
                    }
                }
            }
            assert!(done, "{request} did not finish");
        }
        core.shutdown();

        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "two turns, two requests");
        // The first turn carries system + the question only.
        assert_eq!(requests[0].len(), 2, "{:?}", requests[0]);
        // The second carries system + the whole first exchange + the
        // new question.
        assert_eq!(requests[1].len(), 4, "{:?}", requests[1]);
        assert!(
            requests[1][1].contains("first question"),
            "{:?}",
            requests[1]
        );
        assert!(
            requests[1][2].starts_with("Assistant:") && requests[1][2].contains("ok"),
            "the model's own previous answer must be included: {:?}",
            requests[1]
        );
        assert!(
            requests[1][3].contains("second question"),
            "{:?}",
            requests[1]
        );
    }

    #[test]
    fn the_turn_declares_the_provider_and_model_that_actually_ran() {
        let (runner, _r) = runner_with(
            vec![StreamEvent::TextDelta("x".into()), StreamEvent::Done],
            Duration::ZERO,
            None,
        );
        let (_outcome, payloads) = run(runner, "hi");
        let started = payloads
            .iter()
            .find_map(|p| match p {
                Payload::TurnStarted { provider, model } => Some((provider.clone(), model.clone())),
                _ => None,
            })
            .expect("the turn must say what ran it");
        assert_eq!(
            started,
            ("scripted".to_string(), "scripted-model".to_string())
        );
    }

    #[test]
    fn a_turn_that_outruns_its_deadline_is_incomplete_not_completed() {
        let (runner, _r) = runner_that_never_stops(Duration::from_millis(5));
        let runner = runner
            .with_turn_deadline(Duration::from_millis(80))
            .expect("a positive deadline is accepted");
        let (outcome, _p) = run(runner, "stream forever");
        match outcome {
            Outcome::Incomplete {
                partial_text,
                reason,
            } => {
                assert!(
                    !partial_text.is_empty(),
                    "the text that did arrive must be preserved"
                );
                assert!(reason.contains("turn"), "reason was {reason:?}");
            }
            other => panic!("a deadline must not report success: {other:?}"),
        }
    }

    #[test]
    fn a_deadline_that_fires_before_any_token_is_a_failure_not_a_partial_answer() {
        // One frame per 50ms against a 1ms deadline: the deadline is
        // seen on the first poll, before anything is streamed.
        let (runner, _r) = runner_that_never_stops(Duration::from_millis(50));
        let runner = runner
            .with_turn_deadline(Duration::from_millis(1))
            .expect("a positive deadline is accepted");
        let (outcome, _p) = run(runner, "stream forever");
        match outcome {
            Outcome::Failed { detail } => assert!(detail.contains("turn"), "detail was {detail:?}"),
            other => panic!("an empty timed-out turn is a failure: {other:?}"),
        }
    }

    // A timing-based integration test for the exact simultaneous race
    // (deadline_fired and is_cancelled both true at the outcome
    // decision) was attempted and removed: the window between
    // `stream()` returning and the decision reading `is_cancelled()` is
    // one synchronous code path, not a sleep-measurable interval, and
    // every attempt to force it via `thread::sleep` either missed the
    // window (cancellation too early, correctly reported as Cancelled
    // by the ordinary `Err(Cancelled)` match arm) or arrived after the
    // task had already finished from the deadline alone, which only
    // proves that interrupting a finished task is a no-op -- a true
    // but uninteresting property. `deadline_precedence_truth_table`
    // above is the complete, deterministic proof of this property.

    #[test]
    fn a_zero_deadline_is_refused_at_construction() {
        let (runner, _r) = runner_with(vec![StreamEvent::Done], Duration::ZERO, None);
        assert!(runner.with_turn_deadline(Duration::ZERO).is_err());
    }

    #[test]
    fn an_answer_that_started_and_then_failed_is_incomplete_not_failed() {
        let (runner, _r) = runner_failing_after(
            vec![StreamEvent::TextDelta("half an ans".into())],
            ProviderError::Transport("the connection dropped".into()),
        );
        let (outcome, _p) = run(runner, "hi");
        match outcome {
            Outcome::Incomplete {
                partial_text,
                reason,
            } => {
                assert_eq!(partial_text, "half an ans");
                assert!(reason.contains("connection dropped"), "reason {reason:?}");
            }
            other => panic!("a partial answer must survive the failure: {other:?}"),
        }
    }

    #[test]
    fn a_failure_before_any_token_stays_a_failure() {
        let (runner, _r) = runner_failing_after(
            Vec::new(),
            ProviderError::Transport("the connection dropped".into()),
        );
        let (outcome, _p) = run(runner, "hi");
        match outcome {
            Outcome::Failed { detail } => {
                assert!(detail.contains("connection dropped"), "detail {detail:?}")
            }
            other => panic!("nothing arrived, so this is a failure: {other:?}"),
        }
    }
}
