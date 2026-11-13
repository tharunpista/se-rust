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

use se_protocol::{Outcome, Stage, Usage, UsageSource, UserTurn};
use se_provider::{ChatRequest, Message, ProviderError, Registry, Role, StreamEvent};

use crate::sink::TurnSink;
use crate::Runner;

/// How many tokens to allow in a reply, until this is configurable.
const MAX_TOKENS: u32 = 4096;

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
        })
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
        let request = sink.timed(Stage::ContextBuild, || {
            // System instructions, the conversation so far, then this
            // turn. The history is passed in by the core rather than
            // rebuilt here, so what the model sees and what the
            // transcript shows cannot drift apart.
            let mut messages = Vec::with_capacity(history.len() + 2);
            messages.push(Message {
                role: Role::System,
                content: SYSTEM_PROMPT.to_string(),
            });
            messages.extend_from_slice(history);
            messages.push(Message {
                role: Role::User,
                content: turn.text.clone(),
            });
            ChatRequest {
                model: self.model.clone(),
                messages,
                max_tokens: MAX_TOKENS,
            }
        });

        let mut answer = String::new();
        let mut finish_reason: Option<String> = None;
        let mut reported_usage: Option<Usage> = None;
        let mut first_token_at: Option<std::time::Instant> = None;
        let request_started = std::time::Instant::now();
        // Set if the interface goes away mid-stream, so the outcome can
        // say so rather than reporting a truncated answer as complete.
        let mut disconnected = false;

        // Cloned rather than borrowed from `sink`, which is held
        // mutably below to report deltas.
        let cancelled = sink.cancel_flag();
        let result = provider.stream(
            &request,
            &|| cancelled.load(std::sync::atomic::Ordering::SeqCst),
            &mut |event| match event {
                StreamEvent::TextDelta(text) => {
                    if first_token_at.is_none() {
                        first_token_at = Some(std::time::Instant::now());
                    }
                    answer.push_str(&text);
                    if sink.text_delta(&text).is_err() {
                        disconnected = true;
                        return false;
                    }
                    true
                }
                StreamEvent::ReasoningDelta(text) => {
                    if first_token_at.is_none() {
                        first_token_at = Some(std::time::Instant::now());
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
                    reported_usage = Some(Usage {
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
                StreamEvent::Done => true,
            },
        );

        // Recorded before the outcome is decided, so even a failed turn
        // reports how long it waited.
        if let Some(at) = first_token_at {
            sink.record(se_protocol::Span {
                stage: Stage::TimeToFirstToken,
                duration: at.saturating_duration_since(request_started),
            });
        } else {
            // No token ever arrived: the whole wait was time-to-first-
            // token that never came, and saying so is more useful than
            // recording nothing.
            sink.record(se_protocol::Span {
                stage: Stage::TimeToFirstToken,
                duration: request_started.elapsed(),
            });
        }

        // Only ever the provider's own figures.
        if let Some(usage) = reported_usage {
            let _ = sink.usage(usage);
        }

        match result {
            Ok(()) if disconnected => Outcome::Failed {
                detail: "the interface disconnected mid-answer".to_string(),
            },
            Ok(()) => {
                // A provider that completed without ever saying why it
                // stopped is accepted (many send only `[DONE]`), but a
                // recorded reason other than a normal stop is surfaced
                // rather than silently dropped.
                match finish_reason.as_deref() {
                    None | Some("stop") | Some("end_turn") => {
                        Outcome::Completed { final_text: answer }
                    }
                    Some(other) => Outcome::Failed {
                        detail: format!(
                            "the provider stopped early ({other}); the answer may be incomplete"
                        ),
                    },
                }
            }
            Err(ProviderError::Cancelled) => Outcome::Cancelled,
            Err(e) => Outcome::Failed {
                detail: e.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_protocol::Payload;
    use se_provider::{ModelInfo, ModelProvider, Service, Status};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// A provider under the test's control: it streams what it is told
    /// to, counts how many requests it received, and can be made slow.
    struct ScriptedProvider {
        frames: Vec<StreamEvent>,
        per_frame_delay: Duration,
        requests: Arc<AtomicUsize>,
        fail_with: Option<ProviderError>,
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
            Ok(())
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
        let requests = Arc::new(AtomicUsize::new(0));
        let mut registry = Registry::empty();
        registry.register(Box::new(ScriptedProvider {
            frames,
            per_frame_delay: delay,
            requests: requests.clone(),
            fail_with,
        }));
        let _ = SCRIPTED;
        (
            ProviderRunner {
                registry,
                provider_id: "scripted".into(),
                model: "scripted-model".into(),
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
}
