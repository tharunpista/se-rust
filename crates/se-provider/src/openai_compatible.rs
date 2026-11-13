//! One adapter for every service that speaks `/v1/chat/completions`.
//!
//! NVIDIA NIM, OpenAI, Gemini's compatibility endpoint, OpenRouter,
//! DeepSeek, Groq, Together and a local Ollama or vLLM all accept the
//! same request shape and stream the same `data:` frames. They differ
//! only in identity and endpoint, which is exactly what [`Service`]
//! carries, so adding one is a row in [`CATALOGUE`] rather than a file.
//! A service whose dialect genuinely diverges graduates to its own
//! module; the trait boundary makes that a local change.
//!
//! Streaming is implemented against the response body as it arrives, so
//! the first token reaches the caller without waiting for the last.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use crate::credentials;
use crate::{ChatRequest, ModelInfo, ModelProvider, ProviderError, Role, Status, StreamEvent};

/// Time allowed to establish a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Time allowed between reads once a stream is open.
///
/// Deliberately an *idle* bound, not a total one: a long answer is not a
/// hung connection, so capping total generation would cancel healthy
/// work. Silence this long means the connection is dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Time allowed for the metadata call, which should be quick.
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

/// One shared agent, so DNS, TLS and connections are reused across
/// requests instead of being rebuilt per turn (a startup cost the
/// latency budget in docs/ARCHITECTURE.md explicitly calls out).
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(IDLE_TIMEOUT)
            .build()
    })
}

/// A service's identity and endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Service {
    pub id: &'static str,
    pub display_name: &'static str,
    /// Environment variable holding an API key, overriding the store.
    pub key_var: &'static str,
    /// The service's documented OpenAI-compatible API root, including
    /// whatever version segment it uses.
    ///
    /// The version is part of the configured root rather than being
    /// appended by SE, because vendors do not agree on where it goes:
    /// NVIDIA and OpenAI use `/v1`, Google uses `/v1beta/openai`, Groq
    /// uses `/openai/v1`. Appending a fixed `/v1` produced
    /// `/v1/v1/chat/completions` for Ollama and
    /// `/v1beta/openai/v1/...` for Gemini. Empty means the user must
    /// supply one, which `detect` reports as unusable.
    pub default_base_url: &'static str,
    /// Environment variable overriding the base URL. Required for
    /// self-hosted endpoints, and what tests point at a mock server.
    pub base_url_var: &'static str,
    /// Whether this service needs a credential at all.
    ///
    /// A local Ollama serves `/v1/chat/completions` with no auth, so
    /// demanding a key made it unusable without inventing a fake one --
    /// and an `Authorization: Bearer <nonsense>` header sent to a
    /// server that does not want one is worse than sending none.
    pub key_required: bool,
}

/// The services SE registers.
pub const CATALOGUE: &[Service] = &[
    Service {
        id: "nvidia",
        display_name: "NVIDIA NIM",
        key_var: "NVIDIA_API_KEY",
        default_base_url: "https://integrate.api.nvidia.com/v1",
        base_url_var: "NVIDIA_BASE_URL",
        key_required: true,
    },
    Service {
        id: "openai",
        display_name: "OpenAI",
        key_var: "OPENAI_API_KEY",
        default_base_url: "https://api.openai.com/v1",
        base_url_var: "OPENAI_BASE_URL",
        key_required: true,
    },
    Service {
        id: "gemini",
        display_name: "Google Gemini (OpenAI-compatible endpoint)",
        key_var: "GEMINI_API_KEY",
        default_base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        base_url_var: "GEMINI_BASE_URL",
        key_required: true,
    },
    Service {
        id: "openrouter",
        display_name: "OpenRouter",
        key_var: "OPENROUTER_API_KEY",
        default_base_url: "https://openrouter.ai/api/v1",
        base_url_var: "OPENROUTER_BASE_URL",
        key_required: true,
    },
    Service {
        id: "deepseek",
        display_name: "DeepSeek",
        key_var: "DEEPSEEK_API_KEY",
        default_base_url: "https://api.deepseek.com/v1",
        base_url_var: "DEEPSEEK_BASE_URL",
        key_required: true,
    },
    Service {
        id: "groq",
        display_name: "Groq",
        key_var: "GROQ_API_KEY",
        default_base_url: "https://api.groq.com/openai/v1",
        base_url_var: "GROQ_BASE_URL",
        key_required: true,
    },
    Service {
        id: "ollama",
        display_name: "Ollama (local)",
        key_var: "OLLAMA_API_KEY",
        default_base_url: "http://localhost:11434/v1",
        base_url_var: "OLLAMA_BASE_URL",
        key_required: false,
    },
    Service {
        id: "custom",
        display_name: "Custom OpenAI-compatible endpoint",
        key_var: "SE_CUSTOM_API_KEY",
        default_base_url: "",
        base_url_var: "SE_CUSTOM_BASE_URL",
        key_required: true,
    },
];

pub fn registered() -> Vec<Box<dyn ModelProvider>> {
    CATALOGUE
        .iter()
        .map(|s| Box::new(OpenAiCompatible::new(*s)) as Box<dyn ModelProvider>)
        .collect()
}

pub struct OpenAiCompatible {
    service: Service,
    /// A fixed key, for tests only, so they never touch real storage.
    /// `None` means "read the credential live on every call" -- see the
    /// note in `credentials` about why nothing is cached here.
    fixed_key: Option<String>,
    /// A fixed endpoint, for tests only. Set so a test never depends on
    /// a process-wide environment variable: several tests run in
    /// parallel against their own mock servers, and sharing one variable
    /// made them connect to each other's (observed, not theorised --
    /// four tests failed this way).
    fixed_base_url: Option<String>,
}

impl OpenAiCompatible {
    pub fn new(service: Service) -> Self {
        OpenAiCompatible {
            service,
            fixed_key: None,
            fixed_base_url: None,
        }
    }

    /// For tests: an explicit key, never consulting the credential store.
    pub fn with_fixed_key(service: Service, key: &str) -> Self {
        OpenAiCompatible {
            service,
            fixed_key: Some(key.to_string()),
            fixed_base_url: None,
        }
    }

    /// For tests: an explicit key and endpoint, touching neither the
    /// credential store nor the environment.
    pub fn with_fixed(service: Service, key: &str, base_url: &str) -> Self {
        OpenAiCompatible {
            service,
            fixed_key: Some(key.to_string()),
            fixed_base_url: Some(base_url.trim_end_matches('/').to_string()),
        }
    }

    fn api_key(&self) -> Option<String> {
        match &self.fixed_key {
            Some(k) => Some(k.clone()),
            None => credentials::load(self.service.id, self.service.key_var),
        }
    }

    /// Read live, so pointing a service at a different endpoint does not
    /// require restarting.
    fn base_url(&self) -> Option<String> {
        if let Some(url) = &self.fixed_base_url {
            return Some(url.clone());
        }
        if let Ok(url) = std::env::var(self.service.base_url_var) {
            if !url.is_empty() {
                return Some(url.trim_end_matches('/').to_string());
            }
        }
        if self.service.default_base_url.is_empty() {
            return None;
        }
        Some(self.service.default_base_url.to_string())
    }

    fn endpoint(&self, path: &str) -> Result<String, ProviderError> {
        let base = self.base_url().ok_or_else(|| {
            ProviderError::NotConfigured(format!(
                "{} has no endpoint — set {}",
                self.service.id, self.service.base_url_var
            ))
        })?;
        Ok(format!("{base}{path}"))
    }

    /// The credential, if this service needs one.
    ///
    /// `Ok(None)` is a valid, fully-configured state for a service with
    /// `key_required: false` (a local Ollama). Callers must then send no
    /// `Authorization` header at all.
    fn key_or_error(&self) -> Result<Option<String>, ProviderError> {
        match self.api_key() {
            Some(key) => Ok(Some(key)),
            None if !self.service.key_required => Ok(None),
            None => Err(ProviderError::NotConfigured(format!(
                "no credential for {} — run `se connect {}`, or set {}",
                self.service.id, self.service.id, self.service.key_var
            ))),
        }
    }
}

/// Parses an OpenAI-shaped `{"data":[{"id":...}]}` model list.
/// Anything the endpoint does not report stays `None`.
pub fn parse_model_list(body: &serde_json::Value) -> Vec<ModelInfo> {
    body.get("data")
        .and_then(|d| d.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(|v| v.as_str())?;
                    Some(ModelInfo {
                        id: id.to_string(),
                        display_name: m
                            .get("display_name")
                            .or_else(|| m.get("name"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        // These are absent from the OpenAI model list.
                        // Reported as unknown rather than guessed.
                        context_window: m
                            .get("context_length")
                            .or_else(|| m.get("context_window"))
                            .and_then(|v| v.as_u64()),
                        supports_tools: None,
                        supports_vision: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Interprets one SSE `data:` payload, returning **every** event it
/// carries.
///
/// Returns a `Vec` rather than an `Option<StreamEvent>` because a single
/// frame genuinely can carry more than one fact, and the previous
/// one-event-per-frame signature silently dropped the extras. Two cases
/// were confirmed by reading the old code against real frame shapes:
///
/// * a frame with `usage` **and** a content delta returned only the
///   text, so the token accounting vanished (the old comment claimed
///   the delta was "handled below on the next call" -- there is no next
///   call; this function is invoked once per line);
/// * a frame with `reasoning_content` **and** `content` returned only
///   the reasoning, so the actual answer text vanished.
///
/// An empty `Vec` means the frame carried nothing a caller needs
/// (keep-alives, role-only openers, unparseable lines).
pub fn parse_stream_frame(payload: &str) -> Vec<StreamEvent> {
    let payload = payload.trim();
    if payload.is_empty() {
        return Vec::new();
    }
    // The documented end-of-stream sentinel.
    if payload == "[DONE]" {
        return vec![StreamEvent::Done];
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        // A malformed line is skipped rather than killing the stream,
        // but it is never mistaken for content.
        return Vec::new();
    };

    let mut events = Vec::new();

    // An error delivered *inside* a 200 response stream. Checked first:
    // if a provider reports a failure mid-stream, nothing else in the
    // frame should be treated as a usable answer.
    if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
        let detail = error
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        events.push(StreamEvent::Error(detail));
        return events;
    }

    // Usage, wherever the vendor put it. Groq nests streaming usage
    // under `x_groq`; the top level is the OpenAI-documented position.
    let usage_value = value
        .get("usage")
        .filter(|u| !u.is_null())
        .or_else(|| value.get("x_groq").and_then(|g| g.get("usage")));
    if let Some(usage) = usage_value {
        let get = |k: &str| usage.get(k).and_then(|v| v.as_u64());
        let cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64());
        events.push(StreamEvent::Usage {
            input_tokens: get("prompt_tokens"),
            output_tokens: get("completion_tokens"),
            cached_input_tokens: cached,
        });
    }

    // Every choice, not just the first: a frame may carry several.
    let choices = value.get("choices").and_then(|c| c.as_array());
    for choice in choices.into_iter().flatten() {
        if let Some(delta) = choice.get("delta") {
            // Reasoning models expose thinking under a separate key. It
            // is emitted alongside, never instead of, the answer.
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                events.push(StreamEvent::ReasoningDelta(reasoning.to_string()));
            }
            if let Some(text) = delta
                .get("content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                events.push(StreamEvent::TextDelta(text.to_string()));
            }
        }
        // Why the provider stopped. `length` means truncated, which the
        // caller must not report as a finished answer.
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            events.push(StreamEvent::FinishReason(reason.to_string()));
        }
    }

    events
}

impl ModelProvider for OpenAiCompatible {
    fn id(&self) -> &str {
        self.service.id
    }

    fn display_name(&self) -> &str {
        self.service.display_name
    }

    fn env_var(&self) -> &str {
        self.service.key_var
    }

    fn detect(&self) -> Status {
        let source = if self.fixed_key.is_some() {
            crate::Source::Environment
        } else {
            credentials::source(self.service.id, self.service.key_var)
        };

        if self.base_url().is_none() {
            return Status {
                configured: false,
                detail: format!(
                    "no endpoint — set {} to the service's OpenAI-compatible base URL",
                    self.service.base_url_var
                ),
                source,
            };
        }
        if !source.is_configured() {
            if !self.service.key_required {
                // No credential is the correct, complete configuration
                // for this service. Still not a reachability claim.
                return Status {
                    configured: true,
                    detail: "no credential needed — not yet validated".to_string(),
                    source,
                };
            }
            return Status {
                configured: false,
                detail: format!(
                    "no credential — run `se connect {}`, or set {}",
                    self.service.id, self.service.key_var
                ),
                source,
            };
        }
        Status {
            configured: true,
            // Deliberately not "ready" or "connected": nothing has been
            // sent, so nothing is known about reachability.
            detail: format!("credential from {} — not yet validated", source.label()),
            source,
        }
    }

    fn stream(
        &self,
        request: &ChatRequest,
        cancelled: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(StreamEvent) -> bool,
    ) -> Result<(), ProviderError> {
        let key = self.key_or_error()?;
        let url = self.endpoint("/chat/completions")?;

        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": match m.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                    },
                    "content": m.content,
                })
            })
            .collect();

        let body = serde_json::json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "stream": true,
            // Ask for usage in the stream. Providers that ignore this
            // simply send none, and SE reports none rather than
            // estimating.
            "stream_options": {"include_usage": true},
        });

        if cancelled() {
            return Err(ProviderError::Cancelled);
        }

        let mut request = agent()
            .post(&url)
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream");
        if let Some(key) = &key {
            request = request.set("Authorization", &format!("Bearer {key}"));
        }
        let response = request.send_json(body);

        let response = match response {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let detail = r.into_string().unwrap_or_default();
                return Err(ProviderError::Protocol(format!(
                    "{} returned HTTP {code}: {}",
                    self.service.id,
                    first_line(&detail, 300)
                )));
            }
            Err(ureq::Error::Transport(t)) => return Err(ProviderError::Transport(t.to_string())),
        };

        let mut reader = BufReader::new(response.into_reader());
        let mut line = String::new();
        // Whether the provider ever said why it stopped. A stream that
        // just ends is not the same as one that finished, and treating
        // the two alike meant a connection dropped mid-answer was
        // reported as a complete reply.
        let mut saw_terminal_signal = false;
        loop {
            if cancelled() {
                // Dropping the reader closes the connection, so the
                // provider stops generating rather than being paid to
                // finish something nobody will read.
                return Err(ProviderError::Cancelled);
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    // End of body. Only a success if the provider
                    // actually signalled completion; otherwise the
                    // answer we have is a fragment and must say so.
                    return if saw_terminal_signal {
                        Ok(())
                    } else {
                        Err(ProviderError::Protocol(format!(
                            "{} closed the stream without completing it; the answer may be incomplete",
                            self.service.id
                        )))
                    };
                }
                Ok(_) => {}
                Err(e) => return Err(ProviderError::Transport(e.to_string())),
            }

            let Some(payload) = line.strip_prefix("data:") else {
                // Comments, event names and blank separators.
                continue;
            };
            for event in parse_stream_frame(payload) {
                match event {
                    StreamEvent::Done => {
                        on_event(StreamEvent::Done);
                        return Ok(());
                    }
                    // A provider error inside a 200 stream is a failed
                    // turn, not a finished one.
                    StreamEvent::Error(detail) => {
                        return Err(ProviderError::Protocol(format!(
                            "{} reported an error mid-stream: {}",
                            self.service.id,
                            first_line(&detail, 300)
                        )));
                    }
                    StreamEvent::FinishReason(reason) => {
                        saw_terminal_signal = true;
                        // Truncation is not completion. Reported as an
                        // error so the caller cannot present a cut-off
                        // answer as the whole one.
                        if reason == "length" {
                            on_event(StreamEvent::FinishReason(reason));
                            return Err(ProviderError::Protocol(format!(
                                "{} stopped because the output limit was reached; the answer is truncated",
                                self.service.id
                            )));
                        }
                        if !on_event(StreamEvent::FinishReason(reason)) {
                            return Err(ProviderError::Cancelled);
                        }
                    }
                    event => {
                        if !on_event(event) {
                            return Err(ProviderError::Cancelled);
                        }
                    }
                }
            }
        }
    }

    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let key = self.key_or_error()?;
        let url = self.endpoint("/models")?;
        let mut request = agent().get(&url).timeout(METADATA_TIMEOUT);
        if let Some(key) = &key {
            request = request.set("Authorization", &format!("Bearer {key}"));
        }
        let response = request.call();

        let body: serde_json::Value = match response {
            Ok(r) => r.into_json().map_err(|e| {
                ProviderError::Protocol(format!("the model list was not JSON: {e}"))
            })?,
            Err(ureq::Error::Status(code, r)) => {
                let detail = r.into_string().unwrap_or_default();
                return Err(ProviderError::Protocol(format!(
                    "{} returned HTTP {code} for the model list: {}",
                    self.service.id,
                    first_line(&detail, 300)
                )));
            }
            Err(ureq::Error::Transport(t)) => return Err(ProviderError::Transport(t.to_string())),
        };
        Ok(parse_model_list(&body))
    }
}

/// Trims a provider's error body to something displayable, on one line.
fn first_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    trimmed.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use std::io::Write;
    use std::net::TcpListener;

    pub(super) const TEST_SERVICE: Service = Service {
        id: "mock",
        display_name: "Mock",
        key_var: "SE_TEST_MOCK_KEY_UNSET",
        default_base_url: "",
        base_url_var: "SE_TEST_MOCK_BASE_URL",
        key_required: true,
    };

    /// Serves one HTTP response and records the request body it read.
    fn serve(
        status_line: &'static str,
        content_type: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = requests.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0usize;
                let mut head = String::new();
                loop {
                    let mut l = String::new();
                    if reader.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = l.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    head.push_str(&l);
                }
                let mut payload = vec![0u8; length];
                use std::io::Read;
                let _ = reader.read_exact(&mut payload);
                sink.lock()
                    .unwrap()
                    .push(format!("{head}{}", String::from_utf8_lossy(&payload)));

                let response = format!(
                    "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}"), requests)
    }

    fn provider(base_url: &str) -> OpenAiCompatible {
        OpenAiCompatible::with_fixed(TEST_SERVICE, "test-key-not-real", base_url)
    }

    fn collect(p: &OpenAiCompatible, request: &ChatRequest) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        p.stream(request, &|| false, &mut |e| {
            events.push(e);
            true
        })
        .unwrap();
        events
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "mock-model".into(),
            messages: vec![Message {
                role: Role::User,
                content: "hi".into(),
            }],
            max_tokens: 64,
        }
    }

    #[test]
    fn a_streamed_response_arrives_as_separate_deltas() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _req) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = provider(&base);
        let events = collect(&p, &request());

        let text: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, vec!["Hel", "lo", " there"]);
        assert_eq!(events.last(), Some(&StreamEvent::Done));
    }

    #[test]
    fn the_request_asks_for_streaming_and_carries_the_selected_model() {
        const BODY: &str = "data: [DONE]\n\n";
        let (base, requests) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = provider(&base);
        let mut req = request();
        req.model = "deepseek-ai/deepseek-v3".into();
        collect(&p, &req);

        let seen = requests.lock().unwrap().join("\n");
        assert!(seen.contains("\"stream\":true"), "{seen}");
        assert!(
            seen.contains("deepseek-ai/deepseek-v3"),
            "the selected model must reach the wire: {seen}"
        );
        // And the credential travels only as a bearer header.
        assert!(seen.contains("Authorization: Bearer test-key-not-real"));
    }

    #[test]
    fn provider_reported_usage_is_passed_through_untouched() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":8}}}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _r) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = provider(&base);
        let events = collect(&p, &request());

        let usage = events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                } => Some((*input_tokens, *output_tokens, *cached_input_tokens)),
                _ => None,
            })
            .expect("usage frame parsed");
        assert_eq!(usage, (Some(11), Some(3), Some(8)));
    }

    #[test]
    fn reasoning_is_reported_separately_from_the_answer() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _r) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = provider(&base);
        let events = collect(&p, &request());
        assert!(events.contains(&StreamEvent::ReasoningDelta("thinking".into())));
        assert!(events.contains(&StreamEvent::TextDelta("answer".into())));
    }

    #[test]
    fn a_callback_that_stops_ends_the_stream_promptly() {
        // How a disconnected interface ends a turn: the first delta is
        // taken and then the caller declines the rest.
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _r) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = provider(&base);
        let mut seen = 0;
        let result = p.stream(&request(), &|| false, &mut |_e| {
            seen += 1;
            false
        });
        assert_eq!(result, Err(ProviderError::Cancelled));
        assert_eq!(seen, 1, "the stream must stop at the first refusal");
    }

    #[test]
    fn cancellation_before_the_request_never_touches_the_network() {
        let p = provider("http://127.0.0.1:1");
        let result = p.stream(&request(), &|| true, &mut |_| true);
        assert_eq!(result, Err(ProviderError::Cancelled));
    }

    #[test]
    fn an_http_error_is_reported_with_the_providers_own_explanation() {
        const BODY: &str = r#"{"error":{"message":"invalid api key"}}"#;
        let (base, _r) = serve("HTTP/1.1 401 Unauthorized", "application/json", BODY);
        let p = provider(&base);
        let err = p.stream(&request(), &|| false, &mut |_| true).unwrap_err();
        match err {
            ProviderError::Protocol(detail) => {
                assert!(detail.contains("401"), "{detail}");
                assert!(detail.contains("invalid api key"), "{detail}");
            }
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[test]
    fn an_unreachable_endpoint_is_a_transport_error_not_a_protocol_one() {
        // Port 1 is reserved and nothing listens there.
        let p = provider("http://127.0.0.1:1");
        let err = p.stream(&request(), &|| false, &mut |_| true).unwrap_err();
        assert!(matches!(err, ProviderError::Transport(_)), "got {err:?}");
    }

    #[test]
    fn without_a_credential_nothing_is_sent() {
        // A service with an endpoint but no key: the failure must happen
        // before any connection is attempted.
        let p = OpenAiCompatible {
            service: TEST_SERVICE,
            fixed_key: None,
            fixed_base_url: Some("http://127.0.0.1:1".to_string()),
        };
        let err = p.stream(&request(), &|| false, &mut |_| true).unwrap_err();
        match err {
            ProviderError::NotConfigured(d) => assert!(d.contains("se connect"), "{d}"),
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[test]
    fn a_service_with_no_endpoint_reports_unusable_before_being_called() {
        // `TEST_SERVICE` has no default base URL, and this provider has
        // no fixed one either.
        let p = OpenAiCompatible::with_fixed_key(TEST_SERVICE, "k");
        let status = p.detect();
        assert!(!status.configured);
        assert!(
            status.detail.contains(TEST_SERVICE.base_url_var),
            "{}",
            status.detail
        );
    }

    #[test]
    fn a_credential_present_is_reported_as_unvalidated_not_as_connected() {
        let p = provider("http://127.0.0.1:1");
        let status = p.detect();
        assert!(status.configured);
        assert!(
            status.detail.contains("not yet validated"),
            "having a key is not the same as being reachable: {}",
            status.detail
        );
        // And the status text never carries the key.
        assert!(!status.detail.contains("test-key-not-real"));
    }

    #[test]
    fn models_are_listed_from_what_the_provider_actually_reports() {
        const BODY: &str = r#"{"object":"list","data":[
            {"id":"meta/llama-3.3-70b-instruct"},
            {"id":"deepseek-ai/deepseek-v3","context_length":131072}
        ]}"#;
        let (base, _r) = serve("HTTP/1.1 200 OK", "application/json", BODY);
        let p = provider(&base);
        let models = p.validate().unwrap();

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "meta/llama-3.3-70b-instruct");
        // Unreported capabilities stay unknown rather than being filled
        // in from a local guess.
        assert_eq!(models[0].context_window, None);
        assert_eq!(models[0].supports_tools, None);
        assert_eq!(models[1].context_window, Some(131072));
    }

    #[test]
    fn frame_parsing_handles_the_shapes_providers_actually_send() {
        assert_eq!(parse_stream_frame(" [DONE]"), vec![StreamEvent::Done]);
        // Keep-alive and empty frames carry nothing.
        assert!(parse_stream_frame("").is_empty());
        assert!(parse_stream_frame("{}").is_empty());
        // An empty content delta is not a delta.
        assert!(parse_stream_frame(r#"{"choices":[{"delta":{"content":""}}]}"#).is_empty());
        // A role-only opening frame.
        assert!(parse_stream_frame(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#).is_empty());
        // Malformed JSON is skipped rather than killing the stream.
        assert!(parse_stream_frame("{not json").is_empty());
    }

    #[test]
    fn every_catalogue_entry_is_distinct_and_consistently_named() {
        let mut ids = std::collections::BTreeSet::new();
        for s in CATALOGUE {
            assert!(ids.insert(s.id), "duplicate service id {}", s.id);
            assert!(!s.display_name.is_empty());
            assert!(s.key_var.ends_with("_API_KEY"), "{}", s.key_var);
            assert!(s.base_url_var.ends_with("_BASE_URL"), "{}", s.base_url_var);
        }
        assert!(ids.contains("nvidia"));
        assert!(ids.contains("custom"));
    }

    /// Every endpoint must resolve to exactly one `chat/completions`
    /// with no duplicated version segment.
    ///
    /// Regression for a real defect: a fixed `/v1` was appended to every
    /// base, which gave Ollama `/v1/v1/chat/completions` and Gemini
    /// `/v1beta/openai/v1/chat/completions`.
    #[test]
    fn no_catalogue_endpoint_duplicates_its_version_segment() {
        for service in CATALOGUE {
            if service.default_base_url.is_empty() {
                continue;
            }
            let p = OpenAiCompatible::new(*service);
            let chat = p.endpoint("/chat/completions").unwrap();
            assert!(
                !chat.contains("/v1/v1/"),
                "{}: duplicated version segment in {chat}",
                service.id
            );
            assert_eq!(
                chat.matches("/chat/completions").count(),
                1,
                "{}: {chat}",
                service.id
            );
            // And the root is a plausible absolute URL.
            assert!(
                chat.starts_with("http://") || chat.starts_with("https://"),
                "{}: {chat}",
                service.id
            );
        }
    }

    #[test]
    fn the_documented_endpoints_are_what_each_vendor_publishes() {
        let url = |id: &str| {
            let s = CATALOGUE.iter().find(|s| s.id == id).unwrap();
            OpenAiCompatible::new(*s)
                .endpoint("/chat/completions")
                .unwrap()
        };
        assert_eq!(
            url("nvidia"),
            "https://integrate.api.nvidia.com/v1/chat/completions"
        );
        assert_eq!(url("openai"), "https://api.openai.com/v1/chat/completions");
        assert_eq!(
            url("gemini"),
            "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        );
        assert_eq!(
            url("groq"),
            "https://api.groq.com/openai/v1/chat/completions"
        );
        assert_eq!(url("ollama"), "http://localhost:11434/v1/chat/completions");
    }

    /// Regression for a confirmed honesty defect: a single frame can
    /// carry usage *and* content, and the old one-event-per-frame
    /// signature returned only the text, so the token accounting was
    /// silently discarded.
    #[test]
    fn a_frame_carrying_usage_and_content_yields_both() {
        let events = parse_stream_frame(
            r#"{"choices":[{"delta":{"content":"hi"}}],"usage":{"prompt_tokens":7,"completion_tokens":2}}"#,
        );
        assert!(
            events.contains(&StreamEvent::TextDelta("hi".into())),
            "{events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                StreamEvent::Usage {
                    input_tokens: Some(7),
                    ..
                }
            )),
            "usage must not be dropped: {events:?}"
        );
    }

    /// The same defect in its other form: reasoning and answer text in
    /// one frame returned only the reasoning, losing the answer.
    #[test]
    fn a_frame_carrying_reasoning_and_content_yields_both() {
        let events = parse_stream_frame(
            r#"{"choices":[{"delta":{"reasoning_content":"thinking","content":"answer"}}]}"#,
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningDelta("thinking".into()),
                StreamEvent::TextDelta("answer".into()),
            ]
        );
    }

    #[test]
    fn groq_style_nested_usage_is_found() {
        let events = parse_stream_frame(
            r#"{"choices":[],"x_groq":{"usage":{"prompt_tokens":3,"completion_tokens":4}}}"#,
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                StreamEvent::Usage {
                    output_tokens: Some(4),
                    ..
                }
            )),
            "{events:?}"
        );
    }

    #[test]
    fn an_error_frame_inside_a_200_stream_is_reported_not_dropped() {
        let events = parse_stream_frame(r#"{"error":{"message":"rate limited"}}"#);
        assert_eq!(events, vec![StreamEvent::Error("rate limited".into())]);
    }

    #[test]
    fn a_finish_reason_is_carried_so_truncation_can_be_detected() {
        let events = parse_stream_frame(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#);
        assert_eq!(events, vec![StreamEvent::FinishReason("length".into())]);
    }

    /// A stream that just stops, with no `[DONE]` and no finish reason,
    /// must not be reported as a finished answer.
    #[test]
    fn a_stream_that_ends_without_completing_is_an_error_not_a_success() {
        const BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}

";
        let (base, _r) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = provider(&base);
        let mut seen = String::new();
        let result = p.stream(&request(), &|| false, &mut |e| {
            if let StreamEvent::TextDelta(t) = e {
                seen.push_str(&t);
            }
            true
        });
        assert_eq!(seen, "partial", "the fragment is still delivered");
        match result {
            Err(ProviderError::Protocol(d)) => {
                assert!(d.contains("without completing"), "{d}")
            }
            other => panic!("a truncated stream must not succeed, got {other:?}"),
        }
    }

    #[test]
    fn a_service_needing_no_credential_is_configured_and_sends_no_auth_header() {
        // Ollama serves /v1 with no auth. Demanding a key made it
        // unusable, and sending a fake bearer token is worse than none.
        const KEYLESS: Service = Service {
            id: "keyless",
            display_name: "Keyless",
            key_var: "SE_TEST_KEYLESS_KEY_UNSET",
            default_base_url: "",
            base_url_var: "SE_TEST_KEYLESS_BASE",
            key_required: false,
        };
        const BODY: &str = "data: [DONE]

";
        let (base, requests) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = OpenAiCompatible {
            service: KEYLESS,
            fixed_key: None,
            fixed_base_url: Some(base),
        };

        let status = p.detect();
        assert!(status.configured, "{}", status.detail);
        assert!(
            status.detail.contains("no credential needed"),
            "{}",
            status.detail
        );

        p.stream(&request(), &|| false, &mut |_| true).unwrap();
        let seen = requests.lock().unwrap().join(
            "
",
        );
        assert!(
            !seen.to_lowercase().contains("authorization"),
            "a keyless service must receive no Authorization header: {seen}"
        );
    }

    /// Ollama in the real catalogue must be keyless; everything else
    /// must still require a credential.
    #[test]
    fn only_ollama_is_keyless_in_the_catalogue() {
        for s in CATALOGUE {
            if s.id == "ollama" {
                assert!(!s.key_required, "ollama needs no key");
            } else {
                assert!(s.key_required, "{} must require a credential", s.id);
            }
        }
    }

    #[test]
    fn a_providers_error_body_is_trimmed_to_one_displayable_line() {
        assert_eq!(first_line("a\nb\nc", 50), "a b c");
        let long = "x".repeat(500);
        let cut = first_line(&long, 10);
        assert_eq!(cut.chars().count(), 11);
        assert!(cut.ends_with('…'));
    }
}

#[cfg(test)]
mod streaming_timing {
    use super::tests::TEST_SERVICE;
    use super::*;
    use crate::Message;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Instant;

    /// Serves SSE frames with a real pause between them, over a raw
    /// socket so no HTTP library's buffering is in question.
    fn serve_slowly(frames: Vec<&'static str>, gap: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Drain the request head so the client is not blocked on a
            // write while we start responding.
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = l.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0u8; length];
            use std::io::Read;
            let _ = reader.read_exact(&mut body);

            // Chunked, so the body can legitimately end without a
            // length and each frame can leave immediately.
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
            for frame in frames {
                let payload = format!("data: {frame}\n\n");
                let _ = stream.write_all(format!("{:x}\r\n", payload.len()).as_bytes());
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.write_all(b"\r\n");
                let _ = stream.flush();
                std::thread::sleep(gap);
            }
            let _ = stream.write_all(b"0\r\n\r\n");
            let _ = stream.flush();
        });
        format!("http://{addr}")
    }

    /// The Phase 2 gate: a delta must reach the caller when the provider
    /// sends it, not when the response ends.
    ///
    /// Asserted on arrival times rather than on the final string, because
    /// a fully buffered implementation produces an identical string and
    /// would pass any content-only test. Observed failing exactly that
    /// way during a manual end-to-end run, which is why this exists.
    #[test]
    fn deltas_arrive_as_the_provider_sends_them_not_all_at_the_end() {
        const GAP: Duration = Duration::from_millis(150);
        let base = serve_slowly(
            vec![
                r#"{"choices":[{"delta":{"content":"one"}}]}"#,
                r#"{"choices":[{"delta":{"content":"two"}}]}"#,
                r#"{"choices":[{"delta":{"content":"three"}}]}"#,
                "[DONE]",
            ],
            GAP,
        );
        let p = OpenAiCompatible::with_fixed(TEST_SERVICE, "k", &base);
        let request = ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                role: Role::User,
                content: "go".into(),
            }],
            max_tokens: 32,
        };

        let started = Instant::now();
        let mut arrivals = Vec::new();
        p.stream(&request, &|| false, &mut |event| {
            if let StreamEvent::TextDelta(_) = event {
                arrivals.push(started.elapsed());
            }
            true
        })
        .unwrap();

        assert_eq!(arrivals.len(), 3, "expected three deltas");
        // The first must not have waited for the rest: the whole
        // response takes ~450ms, so a buffered implementation delivers
        // everything at once near the end.
        assert!(
            arrivals[0] < GAP * 2,
            "first delta arrived at {:?}, which means the stream was buffered",
            arrivals[0]
        );
        // And they must be spread out, not bunched.
        let spread = arrivals[2] - arrivals[0];
        assert!(
            spread >= GAP,
            "deltas arrived {spread:?} apart; they should be ~{:?} apart",
            GAP * 2
        );
    }
}
