//! What actually executes a turn.
//!
//! [`Runner`] is the seam between the lifecycle (which this crate owns)
//! and the model (which it deliberately knows nothing about yet). The
//! previous implementation had no such seam: the terminal called a
//! concrete agent function, so nothing about the lifecycle could be
//! tested without a provider, a credential and a network, and the parts
//! that could not be tested were exactly the parts that were broken.
//!
//! Phase 1 ships only runners that need nothing external. That is not a
//! placeholder: it is how streaming, cancellation, approval, ordering
//! and backpressure get proven before a provider exists. Phase 2 adds a
//! provider-backed runner and must change no interface code at all.

use se_protocol::{Outcome, Usage, UsageSource, UserTurn};
use se_provider::Message;

use crate::sink::TurnSink;

/// Executes one task.
///
/// `Send + Sync + 'static` because the supervisor shares one runner
/// across task workers. A runner must therefore hold no per-task mutable
/// state; everything task-specific arrives through [`TurnSink`].
pub trait Runner: Send + Sync + 'static {
    /// Names the provider and model this runner will use, for the
    /// `TurnStarted` event. Returned rather than assumed so an interface
    /// can display what actually ran instead of what was selected --
    /// the distinction the blueprint insists on.
    fn describe(&self) -> (String, String);

    /// Runs the task to completion.
    ///
    /// `history` is the conversation before this turn, oldest first,
    /// owned by the core rather than by the runner: a runner holds no
    /// per-task state (one instance serves every task), and a turn that
    /// cannot see the previous one is not a conversation.
    ///
    /// Must return an [`Outcome`] on every path, and should check
    /// [`TurnSink::is_cancelled`] wherever it would otherwise keep the
    /// user waiting.
    fn run(&self, turn: &UserTurn, history: &[Message], sink: &mut TurnSink) -> Outcome;

    /// Called once by a team run before any node starts, so the runner
    /// can enforce file claims and the token budget. Runners with no
    /// files or model calls to govern ignore it.
    fn attach_team(&mut self, _team: crate::team::TeamContext) {}

    /// Sets (or, with `None`, clears) the specialist later unnamed turns
    /// stay with. Runners with no such notion ignore it.
    fn set_specialist(&self, _id: Option<String>) {}

    /// Sets the work mode. Runners that run no tools ignore it.
    fn set_mode(&self, _mode: se_protocol::WorkMode) {}
}

/// A runner with no dependencies, used to exercise the pipeline.
///
/// It streams the user's own text back one word at a time, optionally
/// with a delay per word so that "a turn that takes a while" can be
/// tested deterministically without a network.
pub struct EchoRunner {
    delay: std::time::Duration,
    /// Upper bound on emitted chunks, so a test can ask for a long turn
    /// without composing a long prompt.
    min_chunks: usize,
}

impl EchoRunner {
    /// Streams as fast as the channel allows.
    pub fn instant() -> Self {
        EchoRunner {
            delay: std::time::Duration::ZERO,
            min_chunks: 1,
        }
    }

    /// Streams `chunks` pieces, pausing `delay` between each: a turn
    /// that is slow in a controllable way.
    pub fn slow(delay: std::time::Duration, chunks: usize) -> Self {
        EchoRunner {
            delay,
            min_chunks: chunks.max(1),
        }
    }
}

impl Runner for EchoRunner {
    fn describe(&self) -> (String, String) {
        ("echo".to_string(), "echo-1".to_string())
    }

    fn run(&self, turn: &UserTurn, _history: &[Message], sink: &mut TurnSink) -> Outcome {
        let (provider, model) = self.describe();
        if sink.start_turn(&provider, &model).is_err() {
            return Outcome::Failed {
                detail: "the interface disconnected".to_string(),
            };
        }

        // Built so that concatenating every delta yields exactly the
        // final text, with no trailing separator. Invariant I2 in
        // miniature: the streamed transcript and the reported answer are
        // the same bytes, so a view can never show something subtly
        // different from what the task says it produced.
        let mut words: Vec<String> = turn.text.split_whitespace().map(str::to_string).collect();
        if words.is_empty() {
            words.push("(nothing to echo)".to_string());
        }
        while words.len() < self.min_chunks {
            words.push(words.len().to_string());
        }
        let last = words.len() - 1;
        let chunks: Vec<String> = words
            .into_iter()
            .enumerate()
            .map(|(i, w)| if i == last { w } else { format!("{w} ") })
            .collect();

        let mut echoed = String::new();
        for chunk in chunks {
            // Checked before each chunk, so an interrupt takes effect
            // within one delay rather than at the end of the turn.
            if sink.is_cancelled() {
                return Outcome::Cancelled;
            }
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            if sink.text_delta(&chunk).is_err() {
                return Outcome::Failed {
                    detail: "the interface disconnected".to_string(),
                };
            }
            echoed.push_str(&chunk);
        }

        // Locally measured, and labelled as such: this runner has no
        // provider to report real figures, and inventing them is exactly
        // the dishonesty the protocol's `UsageSource` exists to prevent.
        let _ = sink.usage(Usage {
            input_tokens: Some(turn.text.split_whitespace().count() as u64),
            output_tokens: Some(echoed.split_whitespace().count() as u64),
            cached_input_tokens: None,
            source: UsageSource::LocallyMeasured,
        });

        // Not trimmed: the answer is precisely what was streamed.
        Outcome::Completed { final_text: echoed }
    }
}

/// Requests approval before echoing, to exercise the approval round
/// trip end to end.
///
/// Exists to drive the sink's approval machinery in tests, not as a
/// real tool runner -- there is no tool system to run yet, and no
/// `se` binary code path references this type. `#[cfg(test)]`-gating
/// was tried and reverted: it broke se-tui's own tests, which exercise
/// this cross-crate the same way `EchoRunner` already is, and that
/// existing convention is worth more than the marginal gate.
pub struct ApprovalRunner {
    pub tool: &'static str,
}

impl Runner for ApprovalRunner {
    fn describe(&self) -> (String, String) {
        ("echo".to_string(), "echo-approval".to_string())
    }

    fn run(&self, turn: &UserTurn, _history: &[Message], sink: &mut TurnSink) -> Outcome {
        let (provider, model) = self.describe();
        if sink.start_turn(&provider, &model).is_err() {
            return Outcome::Failed {
                detail: "the interface disconnected".to_string(),
            };
        }
        let Some(call) = sink.request_approval(self.tool, &turn.text) else {
            // A refusal is a normal outcome, not an error: the user said
            // no and the transcript should say exactly that.
            return Outcome::Completed {
                final_text: format!("{} was not approved, so nothing ran", self.tool),
            };
        };
        // The same id approval was granted under, not a new one: this
        // is the E7 fix in use, not just in the sink's own definition.
        if sink.tool_requested(call, self.tool, &turn.text).is_err() {
            return Outcome::Failed {
                detail: "the interface disconnected".to_string(),
            };
        }
        let _ = sink.tool_completed(call, self.tool, true, "approved and ran");
        Outcome::Completed {
            final_text: format!("{} ran", self.tool),
        }
    }
}

/// Requests approval for two different tools in sequence, so that a
/// decision meant for one can be shown not to answer the other.
pub struct TwoApprovalRunner;

impl Runner for TwoApprovalRunner {
    fn describe(&self) -> (String, String) {
        ("echo".to_string(), "echo-two-approvals".to_string())
    }

    fn run(&self, _turn: &UserTurn, _history: &[Message], sink: &mut TurnSink) -> Outcome {
        let (provider, model) = self.describe();
        if sink.start_turn(&provider, &model).is_err() {
            return Outcome::Failed {
                detail: "the interface disconnected".to_string(),
            };
        }
        let mut ran = Vec::new();
        for tool in ["first_tool", "second_tool"] {
            if sink.request_approval(tool, "please decide").is_some() {
                // A real tool takes time to run. The pause matters: it
                // is the window in which a stray decision can arrive
                // with nothing waiting to consume it.
                std::thread::sleep(std::time::Duration::from_millis(120));
                ran.push(tool);
            }
        }
        Outcome::Completed {
            final_text: format!("ran: {}", ran.join(",")),
        }
    }
}

/// Panics on purpose, to prove a broken runner still yields a terminal
/// event rather than a view stuck on "running" (I5).
#[cfg(test)]
pub(crate) struct PanickingRunner;

#[cfg(test)]
impl Runner for PanickingRunner {
    fn describe(&self) -> (String, String) {
        ("broken".to_string(), "broken-1".to_string())
    }

    fn run(&self, _turn: &UserTurn, _history: &[Message], _sink: &mut TurnSink) -> Outcome {
        panic!("this runner is deliberately broken");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_protocol::{Payload, SessionId};
    use std::time::Duration;

    fn run_to_completion<R: Runner>(runner: R, text: &str) -> (Outcome, Vec<Payload>) {
        let handle = crate::start(runner);
        handle.submit(text, vec![]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut payloads = Vec::new();
        while std::time::Instant::now() < deadline {
            for event in handle.drain_blocking(Duration::from_millis(20)) {
                let terminal = event.is_terminal();
                let payload = event.payload.clone();
                payloads.push(payload.clone());
                if terminal {
                    handle.shutdown();
                    let Payload::TaskFinished { outcome, .. } = payload else {
                        unreachable!("terminal is TaskFinished");
                    };
                    return (outcome, payloads);
                }
            }
        }
        panic!("runner did not finish");
    }

    #[test]
    fn the_echo_runner_streams_the_request_back_in_pieces() {
        let (outcome, payloads) = run_to_completion(EchoRunner::instant(), "one two three");
        assert_eq!(
            outcome,
            Outcome::Completed {
                final_text: "one two three".to_string()
            }
        );
        let deltas: Vec<String> = payloads
            .iter()
            .filter_map(|p| match p {
                Payload::Delta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas.len(), 3, "one delta per word: {deltas:?}");
        assert_eq!(deltas.concat(), "one two three");
    }

    /// Invariant I2, asserted directly: what a view accumulates from the
    /// deltas must be byte-identical to the answer the task reports.
    /// Found by a failing interface test -- the deltas concatenated to a
    /// trailing space while the outcome was trimmed, so the transcript
    /// and the result disagreed.
    #[test]
    fn the_streamed_deltas_concatenate_to_exactly_the_final_text() {
        for request in ["one", "one two three", "  spaced   out  ", "   "] {
            let (outcome, payloads) = run_to_completion(EchoRunner::instant(), request);
            let streamed: String = payloads
                .iter()
                .filter_map(|p| match p {
                    Payload::Delta { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let Outcome::Completed { final_text } = outcome else {
                panic!("expected completion for {request:?}");
            };
            assert_eq!(
                streamed, final_text,
                "transcript and reported answer must match for {request:?}"
            );
        }
    }

    #[test]
    fn a_padded_turn_still_streams_to_exactly_its_final_text() {
        // The `slow` runner pads to a chunk count; the padding must not
        // break the delta/answer equality either.
        let (outcome, payloads) =
            run_to_completion(EchoRunner::slow(std::time::Duration::ZERO, 6), "two words");
        let streamed: String = payloads
            .iter()
            .filter_map(|p| match p {
                Payload::Delta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let Outcome::Completed { final_text } = outcome else {
            panic!("expected completion");
        };
        assert_eq!(streamed, final_text);
        assert!(streamed.starts_with("two words"), "{streamed:?}");
    }

    #[test]
    fn an_empty_request_still_produces_a_turn_rather_than_silence() {
        let (outcome, _p) = run_to_completion(EchoRunner::instant(), "   ");
        assert!(matches!(outcome, Outcome::Completed { .. }));
    }

    #[test]
    fn locally_measured_usage_is_never_labelled_as_provider_reported() {
        let (_outcome, payloads) = run_to_completion(EchoRunner::instant(), "a b");
        let usage = payloads
            .iter()
            .find_map(|p| match p {
                Payload::UsageUpdated(u) => Some(*u),
                _ => None,
            })
            .expect("usage reported");
        assert_eq!(usage.source, UsageSource::LocallyMeasured);
        assert_eq!(usage.input_tokens, Some(2));
        // Nothing is claimed about caching, because nothing knows.
        assert_eq!(usage.cached_input_tokens, None);
    }

    #[test]
    fn a_turn_declares_the_provider_and_model_that_actually_ran() {
        let (_outcome, payloads) = run_to_completion(EchoRunner::instant(), "hi");
        let started = payloads
            .iter()
            .find_map(|p| match p {
                Payload::TurnStarted { provider, model } => Some((provider.clone(), model.clone())),
                _ => None,
            })
            .expect("a turn must say what ran it");
        assert_eq!(started, ("echo".to_string(), "echo-1".to_string()));
    }

    #[test]
    fn an_unapproved_tool_does_not_run_and_the_outcome_says_so() {
        let handle = crate::start(ApprovalRunner { tool: "run_shell" });
        handle.submit("rm -rf /", vec![]).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut outcome = None;
        let mut saw_request = false;
        let mut tool_ran = false;
        while std::time::Instant::now() < deadline && outcome.is_none() {
            for event in handle.drain_blocking(Duration::from_millis(20)) {
                match &event.payload {
                    Payload::ApprovalRequired { call, .. } => {
                        saw_request = true;
                        // Say no, the way an interface would.
                        handle.approve(se_protocol::ApprovalDecision {
                            task: event.task,
                            call: *call,
                            allow: false,
                        });
                    }
                    Payload::ToolCompleted { .. } => tool_ran = true,
                    Payload::TaskFinished { outcome: o, .. } => outcome = Some(o.clone()),
                    _ => {}
                }
            }
        }
        assert!(saw_request, "approval must be requested");
        assert!(!tool_ran, "a denied tool must never execute");
        assert_eq!(
            outcome,
            Some(Outcome::Completed {
                final_text: "run_shell was not approved, so nothing ran".to_string()
            })
        );
        handle.shutdown();
    }

    #[test]
    fn an_approved_tool_runs() {
        let handle = crate::start(ApprovalRunner { tool: "read_file" });
        handle.submit("src/main.rs", vec![]).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut tool_ran = false;
        let mut outcome = None;
        while std::time::Instant::now() < deadline && outcome.is_none() {
            for event in handle.drain_blocking(Duration::from_millis(20)) {
                match &event.payload {
                    Payload::ApprovalRequired { call, .. } => {
                        handle.approve(se_protocol::ApprovalDecision {
                            task: event.task,
                            call: *call,
                            allow: true,
                        });
                    }
                    Payload::ToolCompleted { ok, .. } => tool_ran = *ok,
                    Payload::TaskFinished { outcome: o, .. } => outcome = Some(o.clone()),
                    _ => {}
                }
            }
        }
        assert!(tool_ran, "an approved tool must execute");
        assert_eq!(
            outcome,
            Some(Outcome::Completed {
                final_text: "read_file ran".to_string()
            })
        );
        handle.shutdown();
    }

    #[test]
    fn a_runner_that_is_cancelled_mid_stream_reports_cancelled() {
        let handle = crate::start(EchoRunner::slow(Duration::from_millis(20), 200));
        let task = handle.submit("long", vec![]).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        handle.interrupt(task);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            for event in handle.drain_blocking(Duration::from_millis(20)) {
                if let Payload::TaskFinished { outcome, .. } = &event.payload {
                    assert_eq!(*outcome, Outcome::Cancelled);
                    handle.shutdown();
                    return;
                }
            }
        }
        panic!("a cancelled turn must still finish");
    }

    #[test]
    fn a_session_id_is_stable_across_tasks() {
        let handle = crate::start(EchoRunner::instant());
        let session: SessionId = handle.session();
        handle.submit("one", vec![]).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let events = handle.drain_blocking(Duration::from_millis(20));
            if events.iter().any(|e| e.is_terminal()) {
                assert!(events.iter().all(|e| e.session == session));
                handle.shutdown();
                return;
            }
        }
        panic!("task did not finish");
    }
}
