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
use crate::{
    ChatRequest, ModelInfo, ModelProvider, ProviderError, Role, Status, StreamEvent, TimeoutKind,
};

/// Time allowed to establish a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Default time allowed between reads once a stream is open.
///
/// Deliberately an *idle* bound, not a total one: a long answer is not a
/// hung connection, so capping total generation would cancel healthy
/// work. Silence this long means the connection is dead.
///
/// Configurable via `SE_PROVIDER_IDLE_TIMEOUT_SECS`, read once when the
/// shared agent is first built. A fixed 120s was too coarse a single
/// answer for every deployment -- a developer debugging a slow local
/// model and a user on a flaky connection both legitimately want a
/// different bound, and recompiling to change a constant is not a real
/// option.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Time allowed for the metadata call, which should be quick.
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);

fn idle_timeout() -> Duration {
    std::env::var("SE_PROVIDER_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_IDLE_TIMEOUT)
}

/// One shared agent, so DNS, TLS and connections are reused across
/// requests instead of being rebuilt per turn (a startup cost the
/// latency budget in docs/ARCHITECTURE.md explicitly calls out).
///
/// Read once, at first use -- not a hot-reload setting. A test that
/// needs a different idle bound must set the environment variable
/// before anything in the process has touched this function.
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(idle_timeout())
            // A 302 from a provider can carry Authorization to another
            // host. Never follow a redirect on an authenticated request.
            // The provider's own canonical endpoint is the only URL that
            // ever receives a credential.
            .redirects(0)
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
    /// Which request/stream format the service speaks.
    pub wire: Wire,
}

/// The two wire formats SE speaks. Everything else about a service --
/// origin pinning, no redirects, redaction, timeouts -- is shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// `/chat/completions`, `Authorization: Bearer`.
    OpenAi,
    /// Anthropic's `/messages`, `x-api-key` + `anthropic-version`.
    Anthropic,
}

/// The services SE registers.
pub const CATALOGUE: &[Service] = &[
    Service {
        id: "anthropic",
        display_name: "Anthropic (Claude)",
        key_var: "ANTHROPIC_API_KEY",
        default_base_url: "https://api.anthropic.com/v1",
        base_url_var: "ANTHROPIC_BASE_URL",
        key_required: true,
        wire: Wire::Anthropic,
    },
    Service {
        id: "nvidia",
        display_name: "NVIDIA NIM",
        key_var: "NVIDIA_API_KEY",
        default_base_url: "https://integrate.api.nvidia.com/v1",
        base_url_var: "NVIDIA_BASE_URL",
        key_required: true,
        wire: Wire::OpenAi,
    },
    Service {
        id: "openai",
        display_name: "OpenAI",
        key_var: "OPENAI_API_KEY",
        default_base_url: "https://api.openai.com/v1",
        base_url_var: "OPENAI_BASE_URL",
        key_required: true,
        wire: Wire::OpenAi,
    },
    Service {
        id: "gemini",
        display_name: "Google Gemini (OpenAI-compatible endpoint)",
        key_var: "GEMINI_API_KEY",
        default_base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        base_url_var: "GEMINI_BASE_URL",
        key_required: true,
        wire: Wire::OpenAi,
    },
    Service {
        id: "openrouter",
        display_name: "OpenRouter",
        key_var: "OPENROUTER_API_KEY",
        default_base_url: "https://openrouter.ai/api/v1",
        base_url_var: "OPENROUTER_BASE_URL",
        key_required: true,
        wire: Wire::OpenAi,
    },
    Service {
        id: "deepseek",
        display_name: "DeepSeek",
        key_var: "DEEPSEEK_API_KEY",
        default_base_url: "https://api.deepseek.com/v1",
        base_url_var: "DEEPSEEK_BASE_URL",
        key_required: true,
        wire: Wire::OpenAi,
    },
    Service {
        id: "groq",
        display_name: "Groq",
        key_var: "GROQ_API_KEY",
        default_base_url: "https://api.groq.com/openai/v1",
        base_url_var: "GROQ_BASE_URL",
        key_required: true,
        wire: Wire::OpenAi,
    },
    Service {
        id: "ollama",
        display_name: "Ollama (local)",
        key_var: "OLLAMA_API_KEY",
        default_base_url: "http://localhost:11434/v1",
        base_url_var: "OLLAMA_BASE_URL",
        key_required: false,
        wire: Wire::OpenAi,
    },
    Service {
        id: "apinex",
        display_name: "Apinex",
        key_var: "APINEX_API_KEY",
        // From Apinex's own docs (`BASE_URL = "https://api.apinex.bond/v1"`), checked 2026-11-18.
        default_base_url: "https://api.apinex.bond/v1",
        base_url_var: "APINEX_BASE_URL",
        key_required: true,
        wire: Wire::OpenAi,
    },
    Service {
        id: "custom",
        display_name: "Custom OpenAI-compatible endpoint",
        key_var: "SE_CUSTOM_API_KEY",
        default_base_url: "",
        base_url_var: "SE_CUSTOM_BASE_URL",
        key_required: true,
        wire: Wire::OpenAi,
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
    /// Where model-access verdicts are cached. `None` (the test and
    /// key-check constructors) probes without remembering anything.
    access_file: Option<std::path::PathBuf>,
}

impl OpenAiCompatible {
    pub fn new(service: Service) -> Self {
        OpenAiCompatible {
            service,
            fixed_key: None,
            fixed_base_url: None,
            access_file: crate::model_access::default_path(),
        }
    }

    /// For tests: cache model-access verdicts in `path` instead of nowhere.
    pub fn with_access_file(mut self, path: std::path::PathBuf) -> Self {
        self.access_file = Some(path);
        self
    }

    /// For tests: an explicit key, never consulting the credential store.
    pub fn with_fixed_key(service: Service, key: &str) -> Self {
        OpenAiCompatible {
            service,
            fixed_key: Some(key.to_string()),
            fixed_base_url: None,
            access_file: None,
        }
    }

    /// An explicit key and endpoint, touching neither the credential store nor
    /// the environment: for tests, and for checking a candidate key before saving it.
    pub fn with_fixed(service: Service, key: &str, base_url: &str) -> Self {
        OpenAiCompatible {
            service,
            fixed_key: Some(key.to_string()),
            fixed_base_url: Some(base_url.trim_end_matches('/').to_string()),
            access_file: None,
        }
    }

    /// The credential, as a value usable in an HTTP header.
    ///
    /// Returns `Err` if a credential exists but cannot be placed in a
    /// header (non-ASCII, empty after sanitising). Reporting that
    /// explicitly rather than "no credential" is what lets the user fix
    /// a paste-corrupted key without reading the keyring.
    fn api_key(&self) -> Result<Option<credentials::HeaderSafeKey>, ProviderError> {
        let raw = match &self.fixed_key {
            Some(k) => Some(k.clone()),
            None => credentials::load(self.service.id, self.service.key_var),
        };
        match raw {
            None => Ok(None),
            Some(r) => credentials::HeaderSafeKey::from_raw(&r)
                .map(Some)
                .map_err(|why| ProviderError::NotConfigured(format!("{}: {why}", self.service.id))),
        }
    }

    /// The API root requests go to. Read live, so pointing a service at a
    /// different endpoint does not require restarting.
    pub fn base_url(&self) -> Option<String> {
        if let Some(url) = &self.fixed_base_url {
            return Some(url.clone());
        }
        if let Ok(url) = std::env::var(self.service.base_url_var) {
            if !url.is_empty() {
                return Some(url.trim_end_matches('/').to_string());
            }
        }
        if let Some(url) = credentials::stored_base_url(self.service.id) {
            return Some(url);
        }
        if self.service.default_base_url.is_empty() {
            return None;
        }
        Some(self.service.default_base_url.to_string())
    }

    fn endpoint(&self, path: &str) -> Result<String, ProviderError> {
        let base = self.base_url().ok_or_else(|| {
            ProviderError::NotConfigured(format!(
                "{0} has no endpoint — run `se connect {0} <api-url>` (or /connect {0} <api-url>)",
                self.service.id
            ))
        })?;
        // Every service, keyed or not, must resolve to TLS or a
        // loopback address. For a service that sends a credential, an
        // attacker-set `*_BASE_URL=http://attacker/v1` would otherwise
        // exfiltrate the stored key with no prompt. For a *keyless*
        // service -- Ollama's shape -- the same check still matters:
        // nothing is exfiltrated, but every prompt, file excerpt and
        // agent instruction sent to it would travel in cleartext to
        // whatever host the attacker named (E6b). Loopback is exempted
        // so local proxies and the raw-socket mock servers used in
        // tests continue to work.
        if !is_safe_origin(&base) {
            return Err(ProviderError::NotConfigured(format!(
                "{} requires https:// (or a loopback http:// address); refusing to send a request to an insecure or untrusted URL",
                self.service.id
            )));
        }
        Ok(format!("{base}{path}"))
    }

    /// The credential, if this service needs one.
    ///
    /// Short-circuits on `key_required: false` *before* `api_key()` is
    /// consulted at all. That is the fix for a verified credential-
    /// disclosure defect: Ollama used to receive `Authorization: Bearer
    /// <whatever>` if a stray `OLLAMA_API_KEY` happened to be set, and
    /// sending a nonsense bearer to `http://localhost:11434` is strictly
    /// worse than sending none -- a local process credential leak.
    ///
    /// `Ok(None)` is a valid, fully-configured state. Callers must then
    /// send no `Authorization` header at all.
    fn key_or_error(&self) -> Result<Option<credentials::HeaderSafeKey>, ProviderError> {
        if !self.service.key_required {
            return Ok(None);
        }
        match self.api_key()? {
            Some(k) => Ok(Some(k)),
            None => Err(ProviderError::NotConfigured(format!(
                "no credential for {} — run `se connect {}`, or set {}",
                self.service.id, self.service.id, self.service.key_var
            ))),
        }
    }

    /// Refuses to proceed if this credential is pinned to a different
    /// origin than `url` resolves to (E2). Called with the exact URL a
    /// request is about to be sent to, after `key_or_error` -- a
    /// keyless service has nothing pinned and this is always `Ok`.
    /// Attaches the credential the way this service's wire format wants.
    fn authorize(
        &self,
        request: ureq::Request,
        key: Option<&credentials::HeaderSafeKey>,
    ) -> ureq::Request {
        match (self.service.wire, key) {
            (Wire::Anthropic, Some(key)) => request
                .set("x-api-key", key.as_header_value())
                .set("anthropic-version", crate::anthropic::API_VERSION),
            (Wire::Anthropic, None) => {
                request.set("anthropic-version", crate::anthropic::API_VERSION)
            }
            (Wire::OpenAi, Some(key)) => request.set("Authorization", &key.as_bearer()),
            (Wire::OpenAi, None) => request,
        }
    }

    /// Whether listed models are probed: keyed OpenAI-wire services only. A keyless
    /// local server (Ollama) would load every model just to answer.
    fn probes_models(&self) -> bool {
        self.service.wire == Wire::OpenAi && self.service.key_required
    }

    /// The cache file and this credential's entry in it, if verdicts are remembered.
    fn access_entry(
        &self,
        key: Option<&credentials::HeaderSafeKey>,
    ) -> Option<(std::path::PathBuf, String)> {
        let path = self.access_file.clone()?;
        let entry = crate::model_access::entry_key(
            self.service.id,
            key?.as_header_value(),
            &self.base_url()?,
        );
        Some((path, entry))
    }

    /// One minimal call: the model, one tiny message, `max_tokens: 1`, no stream.
    /// The flag is true when the answer was a 429 (the account, not the model, is limited).
    fn probe(
        &self,
        url: &str,
        key: Option<&credentials::HeaderSafeKey>,
        model: &str,
    ) -> (crate::model_access::Verdict, bool) {
        use crate::model_access::Verdict;
        let request = agent()
            .post(url)
            .timeout(PROBE_TIMEOUT)
            .set("Content-Type", "application/json");
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
            "stream": false,
        });
        match self.authorize(request, key).send_json(body) {
            Ok(_) => (Verdict::Ok, false),
            Err(ureq::Error::Status(code, r)) => (
                probe_verdict(self.service.id, code, &r.into_string().unwrap_or_default()),
                code == 429,
            ),
            Err(ureq::Error::Transport(t)) => {
                (Verdict::Unchecked(first_line(&t.to_string(), 160)), false)
            }
        }
    }

    /// After a real chat turn failed: if the failure says this credential cannot use
    /// `model`, remember that and say so on one extra line. Otherwise `err` unchanged.
    fn learn_from_failure(
        &self,
        model: &str,
        code: u16,
        body: &str,
        key: Option<&credentials::HeaderSafeKey>,
        err: ProviderError,
    ) -> ProviderError {
        // 400/422 in a real turn usually mean a bad parameter, and 403 a blocked request,
        // not a model this key cannot use -- unless the body says it is a billing refusal.
        let ambiguous = matches!(code, 400 | 403 | 422) && !body.contains("billing_error");
        if !self.probes_models() || ambiguous {
            return err;
        }
        let verdict = probe_verdict(self.service.id, code, body);
        if !verdict.hides() {
            return err;
        }
        if let Some((path, entry)) = self.access_entry(key) {
            crate::model_access::mark(&path, &entry, model, verdict.clone());
        }
        ProviderError::Protocol(format!(
            "{err}\n{}/{model} is now hidden from /models: {}",
            self.service.id,
            verdict.why()
        ))
    }

    fn check_pinned_origin(&self, url: &str) -> Result<(), ProviderError> {
        if self.fixed_key.is_some() {
            // Test-only construction, never touches the keyring.
            return Ok(());
        }
        let Some(origin) = credentials::origin_of(url) else {
            return Ok(());
        };
        credentials::check_origin(self.service.id, &origin).map_err(ProviderError::NotConfigured)
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
///
/// Tool-call arguments arrive as JSON fragments spread across many
/// frames, keyed by `index` — this single-frame function has nowhere
/// to keep that state, so it never emits a partial `ToolCallRequested`.
/// Use [`parse_stream_frame_with_tools`] with a [`ToolCallAccumulator`]
/// that outlives the whole response to see them.
pub fn parse_stream_frame(payload: &str) -> Vec<StreamEvent> {
    let mut acc = ToolCallAccumulator::new();
    parse_stream_frame_with_tools(payload, &mut acc)
}

/// Per-call-index accumulator for streamed `tool_calls` deltas.
///
/// OpenAI's wire format sends `id` and `function.name` only on the
/// first delta for a given index, then only `function.arguments`
/// fragments on every later one for that index — this is where those
/// fragments are joined back into complete JSON, keyed by index, never
/// exposed to a caller until [`Self::flush`] is called.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    // `(index, id, name, arguments)`, in first-seen order.
    partial: Vec<(u64, String, String, String)>,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    fn apply(&mut self, tool_calls: &[serde_json::Value]) {
        for tc in tool_calls {
            let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
            let id = tc.get("id").and_then(|v| v.as_str());
            let function = tc.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str());
            let args_fragment = function
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match self.partial.iter_mut().find(|(i, ..)| *i == index) {
                Some((_, slot_id, slot_name, slot_args)) => {
                    if let Some(id) = id {
                        *slot_id = id.to_string();
                    }
                    if let Some(name) = name {
                        *slot_name = name.to_string();
                    }
                    slot_args.push_str(args_fragment);
                }
                None => self.partial.push((
                    index,
                    id.unwrap_or_default().to_string(),
                    name.unwrap_or_default().to_string(),
                    args_fragment.to_string(),
                )),
            }
        }
    }

    /// Emits one [`StreamEvent::ToolCallRequested`] per call accumulated
    /// so far, in the order their index first appeared, and clears the
    /// accumulator. Called once the stream's terminal signal for the
    /// batch of calls (`finish_reason == "tool_calls"`) is seen — the
    /// same "this frame is the last one for this thing" signal this
    /// provider already uses for the plain-text case, not a separate
    /// invention.
    fn flush(&mut self) -> Vec<StreamEvent> {
        self.partial
            .drain(..)
            .map(|(_, id, name, arguments)| StreamEvent::ToolCallRequested {
                id,
                name,
                arguments,
            })
            .collect()
    }
}

/// Same as [`parse_stream_frame`], but tool-call argument fragments are
/// accumulated into `acc` across every call sharing it, and a complete
/// [`StreamEvent::ToolCallRequested`] is only emitted once that call's
/// `finish_reason == "tool_calls"` frame arrives.
pub fn parse_stream_frame_with_tools(
    payload: &str,
    acc: &mut ToolCallAccumulator,
) -> Vec<StreamEvent> {
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
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                acc.apply(tool_calls);
            }
        }
        // Why the provider stopped. `length` means truncated, which the
        // caller must not report as a finished answer.
        if let Some(reason) = choice
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            if reason == "tool_calls" {
                events.extend(acc.flush());
            }
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
        let source = if !self.service.key_required {
            // A keyless service never reads this env var or the
            // keyring (see `key_or_error`), so reporting "Environment"
            // just because it happens to be set would contradict the
            // fix in key_or_error: the UI must not claim a credential
            // is in use when the request path provably ignores it.
            crate::Source::None
        } else if self.fixed_key.is_some() {
            crate::Source::Environment
        } else {
            credentials::source(self.service.id, self.service.key_var)
        };

        if self.base_url().is_none() {
            return Status {
                configured: false,
                detail: format!(
                    "no endpoint — run `se connect {} <api-url>` with its OpenAI-compatible base URL",
                    self.service.id
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

    fn current_origin(&self) -> Option<String> {
        credentials::origin_of(self.base_url()?.as_str())
    }

    fn stream(
        &self,
        request: &ChatRequest,
        cancelled: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(StreamEvent) -> bool,
    ) -> Result<(), ProviderError> {
        let key = self.key_or_error()?;
        let anthropic = self.service.wire == Wire::Anthropic;
        let url = self.endpoint(if anthropic {
            "/messages"
        } else {
            "/chat/completions"
        })?;
        self.check_pinned_origin(&url)?;

        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };
                // An assistant message that made tool calls carries no
                // `content` on the wire, only `tool_calls`; a tool
                // result carries `tool_call_id` and its content as the
                // call's answer. Both shapes are mutually exclusive
                // with the plain `{role, content}` turn.
                if !m.tool_calls.is_empty() {
                    let calls: Vec<serde_json::Value> = m
                        .tool_calls
                        .iter()
                        .map(|c| {
                            serde_json::json!({
                                "id": c.id,
                                "type": "function",
                                "function": {"name": c.name, "arguments": c.arguments},
                            })
                        })
                        .collect();
                    serde_json::json!({"role": role, "content": null, "tool_calls": calls})
                } else if let Some(call_id) = &m.tool_call_id {
                    serde_json::json!({
                        "role": role,
                        "tool_call_id": call_id,
                        "content": m.content,
                    })
                } else {
                    serde_json::json!({"role": role, "content": m.content})
                }
            })
            .collect();

        let mut body = serde_json::json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "stream": true,
            // Ask for usage in the stream. Providers that ignore this
            // simply send none, and SE reports none rather than
            // estimating.
            "stream_options": {"include_usage": true},
        });
        // Omitted rather than sent as `[]`, so a no-tools request's
        // body is unchanged from before tool support existed and every
        // existing body-shape test stays valid.
        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        },
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
        }

        if anthropic {
            body = crate::anthropic::request_body(request);
        }

        if cancelled() {
            return Err(ProviderError::Cancelled);
        }

        let model = request.model.clone();
        let request = agent()
            .post(&url)
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream");
        let request = self.authorize(request, key.as_ref());
        let response = request.send_json(body);

        let response = match response {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let retry_after = r.header("Retry-After").and_then(parse_retry_after);
                let body = r.into_string().unwrap_or_default();
                let err = classify_http_body(self.service.id, code, retry_after, &body, "");
                return Err(self.learn_from_failure(&model, code, &body, key.as_ref(), err));
            }
            Err(ureq::Error::Transport(t)) if is_timeout(&t) => {
                return Err(ProviderError::Timeout(TimeoutKind::Idle));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(ProviderError::Transport(first_line(&t.to_string(), 300)))
            }
        };

        let mut reader = BufReader::new(response.into_reader());
        let mut line = String::new();
        // Whether the provider ever said why it stopped. A stream that
        // just ends is not the same as one that finished, and treating
        // the two alike meant a connection dropped mid-answer was
        // reported as a complete reply.
        let mut saw_terminal_signal = false;
        // Outlives every frame in this one response, so argument
        // fragments for the same tool-call index join up correctly no
        // matter how many lines they are split across.
        let mut tool_call_accumulator = ToolCallAccumulator::new();
        let mut anthropic_state = crate::anthropic::StreamState::default();
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
                        // A real answer may already have streamed; this
                        // is a Truncated error (not Protocol) precisely
                        // so the caller can preserve the partial text as
                        // Outcome::Incomplete rather than discarding it.
                        Err(ProviderError::Truncated(format!(
                            "{} closed the stream without completing it; the answer may be incomplete",
                            self.service.id
                        )))
                    };
                }
                Ok(_) => {}
                Err(e) if is_timeout_io(&e) => {
                    return Err(ProviderError::Timeout(TimeoutKind::Idle))
                }
                Err(e) => return Err(ProviderError::Transport(e.to_string())),
            }

            let Some(payload) = line.strip_prefix("data:") else {
                // Comments, event names and blank separators.
                continue;
            };
            let events = if anthropic {
                crate::anthropic::parse_frame(payload, &mut anthropic_state)
            } else {
                parse_stream_frame_with_tools(payload, &mut tool_call_accumulator)
            };
            for event in events {
                match event {
                    StreamEvent::Done => {
                        on_event(StreamEvent::Done);
                        return Ok(());
                    }
                    // A provider error inside a 200 stream is a failed
                    // turn, not a finished one. Classified the same way
                    // an HTTP-level error body would be, within the
                    // limits of `looks_like_a_policy_refusal`.
                    StreamEvent::Error(detail) => {
                        let detail = format!(
                            "{} reported an error mid-stream: {}",
                            self.service.id,
                            first_line(&detail, 300)
                        );
                        return Err(if looks_like_a_policy_refusal(&detail) {
                            ProviderError::Policy(detail)
                        } else {
                            ProviderError::Protocol(detail)
                        });
                    }
                    StreamEvent::FinishReason(reason) => {
                        saw_terminal_signal = true;
                        if reason == "content_filter" {
                            on_event(StreamEvent::FinishReason(reason.clone()));
                            return Err(ProviderError::Policy(format!(
                                "{} refused to continue on content-policy grounds",
                                self.service.id
                            )));
                        }
                        // Truncation is not completion. A Truncated
                        // error (not Protocol) so the caller preserves
                        // what streamed as Outcome::Incomplete rather
                        // than presenting a cut-off answer as the whole
                        // one, or discarding it as a bare failure.
                        if reason == "length" {
                            on_event(StreamEvent::FinishReason(reason));
                            return Err(ProviderError::Truncated(format!(
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
        self.check_pinned_origin(&url)?;
        let request = agent().get(&url).timeout(METADATA_TIMEOUT);
        let response = self.authorize(request, key.as_ref()).call();

        let body: serde_json::Value = match response {
            Ok(r) => r.into_json().map_err(|e| {
                ProviderError::Protocol(format!("the model list was not JSON: {e}"))
            })?,
            Err(ureq::Error::Status(code, r)) => {
                return Err(classify_http_error(
                    self.service.id,
                    code,
                    r,
                    "for the model list",
                ));
            }
            Err(ureq::Error::Transport(t)) if is_timeout(&t) => {
                return Err(ProviderError::Timeout(TimeoutKind::Idle));
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(ProviderError::Transport(first_line(&t.to_string(), 300)))
            }
        };
        Ok(parse_model_list(&body))
    }

    fn usable_models(&self, refresh: bool) -> Result<crate::UsableModels, ProviderError> {
        use crate::model_access::{self, Verdict};
        let listed = self.list_models()?;
        if !self.probes_models() {
            return Ok(crate::UsableModels {
                models: listed,
                ..Default::default()
            });
        }
        let key = self.key_or_error()?;
        let url = self.endpoint("/chat/completions")?;
        self.check_pinned_origin(&url)?;
        let cached = self.access_entry(key.as_ref());
        let mut entry = cached
            .as_ref()
            .and_then(|(path, k)| model_access::load(path, k, model_access::TTL))
            .unwrap_or_else(model_access::Entry::fresh);
        if refresh {
            entry.checked_at = model_access::Entry::fresh().checked_at;
        }

        let todo: Vec<&str> = listed
            .iter()
            .map(|m| m.id.as_str())
            .filter(|id| refresh || entry.needs_probe(id))
            .collect();
        // Bounded pool: `PROBE_CONCURRENCY` workers pull the next model off a shared index.
        // A 429 limits the whole account (Apinex: 15 requests a minute), so once one arrives
        // the rest are left unprobed -- shown as unchecked and tried on the next listing.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let limited = std::sync::atomic::AtomicBool::new(false);
        let results = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..PROBE_CONCURRENCY.min(todo.len()) {
                scope.spawn(|| loop {
                    if limited.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(id) = todo.get(i) else { break };
                    let (v, rate_limited) = self.probe(&url, key.as_ref(), id);
                    if rate_limited {
                        limited.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    results
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((id.to_string(), v));
                });
            }
        });
        for (id, v) in results.into_inner().unwrap_or_else(|e| e.into_inner()) {
            if let Verdict::BadKey(detail) = v {
                return Err(ProviderError::Auth(detail));
            }
            // An inconclusive re-check (429, 5xx) keeps the verdict already known.
            if v.settled() || entry.needs_probe(&id) {
                entry.models.insert(id, v);
            }
        }
        if let Some((path, k)) = &cached {
            if !todo.is_empty() {
                model_access::store(path, k, &entry);
            }
        }

        let mut out = crate::UsableModels::default();
        for m in listed {
            match entry.models.get(&m.id) {
                Some(v) if v.hides() => out.hidden.push((m.id, v.clone())),
                Some(Verdict::Unchecked(_)) | None => {
                    out.unchecked.push(m.id.clone());
                    out.models.push(m);
                }
                Some(_) => out.models.push(m),
            }
        }
        Ok(out)
    }
}

/// How many probes run at once, and how long one may take.
const PROBE_CONCURRENCY: usize = 8;
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Classifies one failed probe (or failed chat call) into a verdict.
/// Reuses [`classify_http_body`] so the reason is redacted and trimmed the same way.
fn probe_verdict(service_id: &str, code: u16, body: &str) -> crate::model_access::Verdict {
    use crate::model_access::Verdict;
    let err = classify_http_body(service_id, code, None, body, "for a test call");
    // A firewall or proxy page is not the API's verdict on this model.
    if body.trim_start().starts_with('<') {
        return Verdict::Unchecked(err.to_string());
    }
    // The provider's own words, when its error body has them.
    let reason = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .or_else(|| v.get("detail"))
                .and_then(|m| m.as_str())
                .map(|m| first_line(m, 160))
        })
        .unwrap_or_else(|| format!("HTTP {code}"));
    match code {
        401 => Verdict::BadKey(err.to_string()),
        402 | 403 => Verdict::Paid(reason),
        _ if body.contains("billing_error") => Verdict::Paid(reason),
        // Spec said "available", but a 429 limits the account, not the model: Apinex rate-limits
        // before it checks billing, so a 429 there said nothing about a paid-only model.
        429 => Verdict::Unchecked(format!("rate limited: {reason}")),
        400 | 404 | 410 | 422 => Verdict::NotChat(reason),
        _ => Verdict::Unchecked(err.to_string()),
    }
}

/// Trims a provider's error body to something displayable, on one line.
/// Whether this URL may receive a credential.
///
/// `https://` is safe, unless the authority carries userinfo. `http://`
/// is accepted only for a loopback host with no userinfo, so a local
/// Ollama and the raw-socket mock servers used in tests work -- but not
/// a hostile `http://attacker.example.com/v1` that an attacker-set
/// `*_BASE_URL` could point at.
///
/// A URL with embedded credentials (`https://user:key@host/...`) is
/// refused outright, over either scheme, rather than stripped down to
/// its host and allowed through. The earlier version only stripped
/// userinfo on the `http://` branch and never looked for it on
/// `https://` at all, so `https://user:KEY@attacker.example.com/v1`
/// passed this check and the credential in the URL was sent. Rejecting
/// is also what keeps a userinfo-bearing URL out of
/// [`crate::credentials::origin_of`]'s output before it is ever pinned
/// or echoed in an error message -- this function runs before
/// `check_pinned_origin`, in [`OpenAiCompatible::endpoint`], so a
/// userinfo URL never reaches that code at all.
/// The API root from whatever address the user pasted: people copy a full endpoint from
/// docs (`…/v1/models`, `…/v1/chat/completions`), and SE appends those paths itself, which
/// produced `…/v1/models/models` (a real 404 seen from Apinex).
pub fn api_root(url: &str) -> String {
    const ENDPOINTS: [&str; 6] = [
        "/chat/completions",
        "/completions",
        "/models",
        "/messages",
        "/responses",
        "/embeddings",
    ];
    let mut root = url.trim().trim_end_matches('/');
    for e in ENDPOINTS {
        if let Some(r) = root.strip_suffix(e) {
            root = r.trim_end_matches('/');
            break;
        }
    }
    root.to_string()
}

pub fn is_safe_origin(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    // Authority = everything up to the first path separator.
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    match scheme {
        "https" => true,
        "http" => {
            // Bracketed IPv6 ("[::1]:port") must not split on the
            // inner colons.
            let host = if let Some(bracketed) = authority.strip_prefix('[') {
                bracketed.split(']').next().unwrap_or("")
            } else {
                authority.split(':').next().unwrap_or("")
            };
            matches!(host, "localhost" | "127.0.0.1" | "::1")
        }
        _ => false,
    }
}

/// Whether an error body carries one of the markers vendors use for a
/// policy/safety refusal.
///
/// Honestly limited: there is no shared vendor standard for this, so
/// the match is on the handful of strings observed in practice
/// (OpenAI/Groq's `content_filter`, Gemini's `SAFETY`). A vendor using
/// different wording is classified as `BadRequest`/`Protocol` instead of
/// `Policy` -- wrong severity, but never the dangerous direction: it is
/// never silently retried or failed over either way, since both
/// variants are equally non-retriable. Extending this table is a
/// one-line, reviewable change; it is not a security boundary.
fn looks_like_a_policy_refusal(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("content_filter") || lower.contains("safety") || lower.contains("content_policy")
}

/// Classifies an HTTP error status into a typed [`ProviderError`],
/// reading `Retry-After` before the body is consumed.
///
/// A match on the status code and a declared policy marker -- never on
/// free-text wording beyond that one documented exception -- so a
/// future retry/failover decision can be written against the type
/// rather than re-parsing vendor prose.
fn classify_http_error(
    service_id: &str,
    code: u16,
    response: ureq::Response,
    context: &str,
) -> ProviderError {
    let retry_after = response.header("Retry-After").and_then(parse_retry_after);
    let body = response.into_string().unwrap_or_default();
    classify_http_body(service_id, code, retry_after, &body, context)
}

/// [`classify_http_error`] once the body has been read.
fn classify_http_body(
    service_id: &str,
    code: u16,
    retry_after: Option<Duration>,
    body: &str,
    context: &str,
) -> ProviderError {
    // A web page instead of an API error is a firewall or the wrong address (e.g. Cloudflare's
    // "Sorry, you have been blocked" is a 403), never a verdict on the key. Only for 4xx other
    // than 429: an HTML 429/5xx page is still a rate limit or outage and must stay retriable.
    let page_title = body.trim_start().starts_with('<').then(|| {
        body.split_once("<title>")
            .and_then(|(_, t)| t.split_once("</title>"))
            .map(|(t, _)| first_line(t.trim(), 120))
            .unwrap_or_else(|| "a web page".to_string())
    });
    if let Some(title) = &page_title {
        if (400..500).contains(&code) && code != 429 {
            return ProviderError::Protocol(format!(
                "{service_id} returned HTTP {code} {context} as a web page, not an API answer: {title}"
            ));
        }
    }
    let detail = format!(
        "{service_id} returned HTTP {code} {context}: {}",
        page_title.unwrap_or_else(|| first_line(body, 300))
    );
    match code {
        401 | 403 => ProviderError::Auth(detail),
        429 => ProviderError::RateLimited {
            retry_after,
            detail,
        },
        400 if looks_like_a_policy_refusal(body) => ProviderError::Policy(detail),
        400 | 404 | 422 => ProviderError::BadRequest(detail),
        500..=599 => ProviderError::Server {
            status: code,
            detail,
        },
        _ => ProviderError::Protocol(detail),
    }
}

/// Parses `Retry-After` as a number of seconds.
///
/// Deliberately does not handle the HTTP-date form (`Retry-After: Wed,
/// 13 Nov 2026 12:00:00 GMT`). Adding a date-parsing dependency for one
/// header was not worth it at this scope; a provider using the date
/// form is treated as not having given a retry hint (`None`), which is
/// the honest fallback already used for a missing header, not a wrong
/// answer -- just a less specific one.
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Whether a `ureq::Transport` represents a timeout SE itself bounded
/// (connect or idle-read), as opposed to DNS failure, TLS failure or a
/// reset connection -- which stay `ProviderError::Transport`.
fn is_timeout(t: &ureq::Transport) -> bool {
    matches!(t.kind(), ureq::ErrorKind::Io) && t.to_string().to_ascii_lowercase().contains("time")
}

/// Same distinction for the `std::io::Error` surfaced by `read_line`
/// once a stream is already open (ureq maps a read timeout to
/// `io::ErrorKind::WouldBlock` or `TimedOut` depending on platform).
fn is_timeout_io(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// Credential shapes several providers echo back inside their own error
/// payloads. Prefix matching only -- a generic "long random-looking
/// word" rule would redact model ids and request ids as well.
const CREDENTIAL_PREFIXES: [&str; 5] = ["sk-", "sk_", "gsk_", "nvapi-", "AIza"];

/// Replaces every credential-shaped run inside one word. A prefix counts
/// only at a non-alphanumeric boundary -- error bodies are JSON, so a key
/// usually follows `"` or `:` rather than a space, while `task-runner`
/// and `risk-free` contain `sk-` after a letter and must survive.
fn redact_credentials(word: &str) -> String {
    let bytes = word.as_bytes();
    let mut out = String::with_capacity(word.len());
    let mut i = 0;
    while i < word.len() {
        let at = &word[i..];
        let at_a_boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if at_a_boundary && CREDENTIAL_PREFIXES.iter().any(|p| at.starts_with(p)) {
            out.push_str("[redacted]");
            i += at
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(at.len());
            continue;
        }
        let c = at.chars().next().expect("inside the string");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A provider's error body is text this process does not control, and it
/// reaches both the interface and any log: control characters (ANSI
/// escapes among them) are flattened, and anything shaped like a
/// credential is replaced before the body is carried anywhere.
fn first_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut words: Vec<String> = Vec::new();
    let mut after_bearer = false;
    for word in flat.split_whitespace() {
        // Trailing punctuation only, and `ends_with`: the scheme name
        // can arrive as `Bearer`, `Bearer:`, or glued to a JSON key as
        // `{"authorization":"Bearer`.
        let bare = word.trim_end_matches(|c: char| !c.is_alphanumeric());
        words.push(if after_bearer {
            "[redacted]".to_string()
        } else {
            redact_credentials(word)
        });
        after_bearer = bare.to_ascii_lowercase().ends_with("bearer");
    }
    let joined = words.join(" ");
    if joined.chars().count() <= max {
        return joined;
    }
    joined.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolCallRecord, ToolDefinition};

    #[test]
    fn a_pasted_endpoint_is_cut_back_to_the_api_root() {
        let root = "https://api.apinex.bond/v1";
        for pasted in [
            "https://api.apinex.bond/v1",
            " https://api.apinex.bond/v1/ ",
            "https://api.apinex.bond/v1/models",
            "https://api.apinex.bond/v1/models/",
            "https://api.apinex.bond/v1/chat/completions",
            "https://api.apinex.bond/v1/messages",
        ] {
            assert_eq!(api_root(pasted), root, "{pasted}");
        }
        // Roots with their own path are kept as they are.
        assert_eq!(
            api_root("https://generativelanguage.googleapis.com/v1beta/openai"),
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
    }

    #[test]
    fn a_credential_echoed_back_by_a_provider_is_redacted_before_it_is_displayed() {
        let body =
            r#"{"error":"invalid key sk-abc123secret for Authorization: Bearer gsk_realsecret"}"#;
        let shown = first_line(body, 300);
        assert!(!shown.contains("abc123secret"), "{shown}");
        assert!(!shown.contains("realsecret"), "{shown}");
        assert!(shown.contains("[redacted]"), "{shown}");
        assert!(
            shown.contains("invalid key"),
            "the reason must survive: {shown}"
        );
    }

    #[test]
    fn escape_sequences_in_a_provider_error_cannot_reach_the_terminal() {
        let shown = first_line("bad \u{1b}[31mrequest\u{1b}[0m", 300);
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
        assert!(shown.contains("request"), "{shown:?}");
    }

    #[test]
    fn a_key_sitting_against_json_punctuation_is_still_redacted() {
        for body in [
            r#"{"api_key":"sk-abc123secret"}"#,
            "key=sk-abc123secret",
            r#"{"token":"gsk_abc123secret","x":1}"#,
            "Authorization=Bearer nvapi-abc123secret",
        ] {
            let shown = first_line(body, 300);
            assert!(
                !shown.contains("abc123secret"),
                "{body} leaked as {shown:?}"
            );
            assert!(shown.contains("[redacted]"), "{body} gave {shown:?}");
        }
    }

    #[test]
    fn a_bearer_token_is_redacted_even_when_the_word_carries_punctuation() {
        let shown = first_line(r#"{"header":"Bearer" "abc123secret"}"#, 300);
        assert!(!shown.contains("abc123secret"), "{shown:?}");
    }

    #[test]
    fn an_ordinary_word_containing_a_key_prefix_survives() {
        let shown = first_line("task-runner for risk-free retries", 300);
        assert_eq!(shown, "task-runner for risk-free retries");
    }

    #[test]
    fn an_ordinary_model_id_is_not_mistaken_for_a_credential() {
        let shown = first_line("model gpt-4o-mini-2024-07-18 is unavailable", 300);
        assert_eq!(shown, "model gpt-4o-mini-2024-07-18 is unavailable");
    }

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
        wire: Wire::OpenAi,
    };

    /// Serves one HTTP response and records the request body it read.
    fn serve(
        status_line: &'static str,
        content_type: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        serve_by(move |_| (status_line, content_type, body))
    }

    /// Like [`serve`], but answers each request with `route(request text)`.
    fn serve_by(
        route: impl Fn(&str) -> (&'static str, &'static str, &'static str) + Send + 'static,
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
                let seen = format!("{head}{}", String::from_utf8_lossy(&payload));
                let (status_line, content_type, body) = route(&seen);
                sink.lock().unwrap().push(seen);

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

    /// The Claude path through the real HTTP client: `/messages`, the
    /// key in `x-api-key` (never as a Bearer token), the version header,
    /// Anthropic's body, and its stream parsed into SE's events.
    #[test]
    fn anthropic_wire_sends_its_own_headers_and_parses_its_stream() {
        const BODY: &str = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi there\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let (base, requests) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let service = Service {
            wire: Wire::Anthropic,
            ..TEST_SERVICE
        };
        let p = OpenAiCompatible::with_fixed(service, "sk-ant-test", &base);
        let mut req = request();
        req.messages.insert(0, Message::text(Role::System, "sys"));
        let events = collect(&p, &req);
        assert!(
            events.contains(&StreamEvent::TextDelta("Hi there".into())),
            "{events:?}"
        );
        assert_eq!(events.last(), Some(&StreamEvent::Done));

        let sent = requests.lock().unwrap()[0].clone();
        let lower = sent.to_ascii_lowercase();
        assert!(sent.starts_with("POST /messages"), "{sent}");
        assert!(lower.contains("x-api-key: sk-ant-test"), "{sent}");
        assert!(lower.contains("anthropic-version: 2023-06-01"), "{sent}");
        assert!(!lower.contains("authorization:"), "no bearer token: {sent}");
        assert!(sent.contains("\"text\":\"sys\""), "{sent}");
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

    const BILLING_402: &str = r#"{"type":"error","error":{"type":"billing_error","message":"Insufficient balance"}}"#;
    const SUBSCRIPTION_400: &str = r#"{"error":{"type":"billing_error","message":"This model is currently available only with a subscription"}}"#;
    const FIREWALL_PAGE: &str = "<!DOCTYPE html><html><head><title>Attention Required! | Cloudflare</title></head></html>";

    #[test]
    fn probe_verdicts_follow_the_classification_table() {
        use crate::model_access::Verdict;
        let v = |code, body| probe_verdict("mock", code, body);
        assert_eq!(v(402, BILLING_402), Verdict::Paid("Insufficient balance".into()));
        // billing_error decides even when the status is not 402.
        assert!(matches!(v(400, SUBSCRIPTION_400), Verdict::Paid(r) if r.contains("subscription")));
        assert!(matches!(v(403, r#"{"error":{"message":"no access"}}"#), Verdict::Paid(r) if r == "no access"));
        assert!(matches!(v(404, r#"{"error":{"message":"model not found"}}"#), Verdict::NotChat(_)));
        assert!(matches!(v(410, "{}"), Verdict::NotChat(r) if r == "HTTP 410"));
        assert!(matches!(v(422, "{}"), Verdict::NotChat(_)));
        assert_eq!(
            v(429, r#"{"error":{"message":"slow down"}}"#),
            Verdict::Unchecked("rate limited: slow down".into())
        );
        assert!(matches!(v(401, r#"{"error":{"message":"bad key"}}"#), Verdict::BadKey(_)));
        assert!(matches!(v(500, "oops"), Verdict::Unchecked(_)));
        assert!(matches!(v(503, "{}"), Verdict::Unchecked(_)));
        // A firewall page is never a verdict, even on a 403.
        assert!(matches!(v(403, FIREWALL_PAGE), Verdict::Unchecked(r) if r.contains("web page")));
        // A key echoed back in an error body never reaches the reason.
        assert!(!v(402, r#"{"error":{"message":"key sk-abc123secret has no balance"}}"#)
            .why()
            .contains("abc123secret"));
    }

    fn access_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("se-access-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("model-access.json")
    }

    fn catalogue_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        serve_by(|req| {
            const JSON: &str = "application/json";
            if req.starts_with("GET") {
                return (
                    "HTTP/1.1 200 OK",
                    JSON,
                    r#"{"data":[{"id":"ok"},{"id":"paid"},{"id":"sub"},{"id":"gone"},{"id":"embed"},{"id":"flaky"},{"id":"walled"}]}"#,
                );
            }
            let model = |m: &str| req.contains(&format!("\"model\":\"{m}\""));
            if model("ok") {
                ("HTTP/1.1 200 OK", JSON, r#"{"choices":[{"message":{"content":"h"}}]}"#)
            } else if model("paid") {
                ("HTTP/1.1 402 Payment Required", JSON, BILLING_402)
            } else if model("sub") {
                ("HTTP/1.1 400 Bad Request", JSON, SUBSCRIPTION_400)
            } else if model("gone") {
                ("HTTP/1.1 410 Gone", JSON, "{}")
            } else if model("embed") {
                ("HTTP/1.1 404 Not Found", JSON, r#"{"error":{"message":"not a chat model"}}"#)
            } else if model("flaky") {
                ("HTTP/1.1 503 Service Unavailable", JSON, "{}")
            } else {
                ("HTTP/1.1 403 Forbidden", "text/html", FIREWALL_PAGE)
            }
        })
    }

    #[test]
    fn usable_models_probes_each_listed_model_and_hides_what_the_key_cannot_use() {
        let (base, requests) = catalogue_server();
        let file = access_file("probe");
        let p = OpenAiCompatible::with_fixed(TEST_SERVICE, "sk-test-key", &base)
            .with_access_file(file.clone());
        let got = p.usable_models(false).unwrap();
        let shown: Vec<&str> = got.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(shown, ["ok", "flaky", "walled"]);
        assert_eq!(got.unchecked, ["flaky", "walled"]);
        let hidden: Vec<&str> = got.hidden.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(hidden, ["paid", "sub", "gone", "embed"]);
        assert_eq!(
            got.hidden_summary("/models refresh").as_deref(),
            Some("4 hidden: 2 need a subscription or balance, 2 not chat models — /models refresh to re-check")
        );
        {
            let seen = requests.lock().unwrap();
            assert_eq!(seen.len(), 8, "one list + one probe per model");
            let probe = seen.iter().find(|r| r.contains("\"model\":\"ok\"")).unwrap();
            assert!(probe.starts_with("POST /chat/completions"), "{probe}");
            assert!(probe.contains("\"max_tokens\":1") && probe.contains("\"stream\":false"), "{probe}");
        }
        let stored = std::fs::read_to_string(&file).unwrap();
        assert!(!stored.contains("sk-test-key"));

        // From cache: only the unsettled two are probed again.
        let again = p.usable_models(false).unwrap();
        assert_eq!(again.hidden.len(), 4);
        assert_eq!(requests.lock().unwrap().len(), 8 + 1 + 2);

        // Refresh ignores the cache and probes everything.
        p.usable_models(true).unwrap();
        assert_eq!(requests.lock().unwrap().len(), 11 + 1 + 7);

        // A different key never reuses these verdicts.
        let other = OpenAiCompatible::with_fixed(TEST_SERVICE, "sk-other-key", &base)
            .with_access_file(file);
        other.usable_models(false).unwrap();
        assert_eq!(requests.lock().unwrap().len(), 19 + 1 + 7);
    }

    /// A 429 limits the account, not the model: probing stops, nothing is hidden or cached
    /// as usable, and a refresh that only meets 429s keeps the verdicts already known.
    #[test]
    fn an_account_rate_limit_stops_probing_and_decides_nothing() {
        let (base, requests) = serve_by(|req| {
            if req.starts_with("GET") {
                let list = r#"{"data":[{"id":"m0"},{"id":"m1"},{"id":"m2"},{"id":"m3"},{"id":"m4"},{"id":"m5"},{"id":"m6"},{"id":"m7"},{"id":"m8"},{"id":"m9"},{"id":"m10"},{"id":"m11"},{"id":"m12"},{"id":"m13"},{"id":"m14"},{"id":"m15"},{"id":"m16"},{"id":"m17"},{"id":"m18"},{"id":"m19"}]}"#;
                return ("HTTP/1.1 200 OK", "application/json", list);
            }
            ("HTTP/1.1 429 Too Many Requests", "application/json", r#"{"error":{"type":"rate_limit_error","message":"Max 15 requests per minute"}}"#)
        });
        let file = access_file("limited");
        let key = crate::model_access::entry_key("mock", "k", &base);
        let mut known = crate::model_access::Entry::fresh();
        known.models.insert("m0".into(), crate::model_access::Verdict::Paid("Insufficient balance".into()));
        crate::model_access::store(&file, &key, &known);

        let p = OpenAiCompatible::with_fixed(TEST_SERVICE, "k", &base).with_access_file(file.clone());
        let got = p.usable_models(true).unwrap();
        assert_eq!(got.hidden.len(), 1, "m0 stays hidden: {:?}", got.hidden);
        assert_eq!(got.models.len(), 19);
        assert_eq!(got.unchecked.len(), 19);
        let probes = requests.lock().unwrap().len() - 1;
        assert!(probes < 19, "probing should stop after the first 429s, made {probes}");
        let entry = crate::model_access::load(&file, &key, crate::model_access::TTL).unwrap();
        assert_eq!(entry.models.len(), 1, "a 429 is never cached");
    }

    #[test]
    fn a_probe_refusing_the_key_fails_the_whole_listing() {
        let (base, _) = serve_by(|req| {
            if req.starts_with("GET") {
                ("HTTP/1.1 200 OK", "application/json", r#"{"data":[{"id":"a"}]}"#)
            } else {
                ("HTTP/1.1 401 Unauthorized", "application/json", r#"{"error":{"message":"invalid key"}}"#)
            }
        });
        let p = OpenAiCompatible::with_fixed(TEST_SERVICE, "k", &base);
        assert!(matches!(p.usable_models(false), Err(ProviderError::Auth(_))));
    }

    #[test]
    fn a_chat_turn_failing_with_402_hides_that_model_at_once() {
        let (base, _) = serve("HTTP/1.1 402 Payment Required", "application/json", BILLING_402);
        let file = access_file("learn");
        let p = OpenAiCompatible::with_fixed(TEST_SERVICE, "k", &base).with_access_file(file.clone());
        let err = p.stream(&request(), &|| false, &mut |_| true).unwrap_err().to_string();
        let last = err.lines().last().unwrap();
        assert_eq!(
            last,
            "mock/mock-model is now hidden from /models: needs a subscription or balance (Insufficient balance)"
        );
        let key = crate::model_access::entry_key("mock", "k", &base);
        let entry = crate::model_access::load(&file, &key, crate::model_access::TTL).unwrap();
        assert!(entry.models["mock-model"].hides());
    }

    #[test]
    fn a_chat_turn_failing_with_a_plain_400_hides_nothing() {
        let (base, _) = serve("HTTP/1.1 400 Bad Request", "application/json", r#"{"error":{"message":"bad tools"}}"#);
        let file = access_file("nolearn");
        let p = OpenAiCompatible::with_fixed(TEST_SERVICE, "k", &base).with_access_file(file.clone());
        let err = p.stream(&request(), &|| false, &mut |_| true).unwrap_err();
        assert!(matches!(err, ProviderError::BadRequest(_)));
        assert!(!file.exists());
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "mock-model".into(),
            messages: vec![Message::text(Role::User, "hi")],
            max_tokens: 64,
            tools: Vec::new(),
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

    /// `parse_stream_frame_with_tools` directly: arguments split across
    /// three frames for the same index must join into one complete
    /// JSON string, and the event is emitted only on the
    /// `finish_reason == "tool_calls"` frame, not before.
    #[test]
    fn tool_call_arguments_are_accumulated_across_frames_and_emitted_once() {
        let mut acc = ToolCallAccumulator::new();

        let first = parse_stream_frame_with_tools(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#,
            &mut acc,
        );
        assert!(
            first.is_empty(),
            "no event before the call's arguments are complete: {first:?}"
        );

        let second = parse_stream_frame_with_tools(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\": \"a.t"}}]}}]}"#,
            &mut acc,
        );
        assert!(second.is_empty(), "{second:?}");

        let third = parse_stream_frame_with_tools(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"xt\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            &mut acc,
        );
        assert_eq!(
            third,
            vec![
                StreamEvent::ToolCallRequested {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path": "a.txt"}"#.into(),
                },
                StreamEvent::FinishReason("tool_calls".into()),
            ]
        );
    }

    /// Two calls interleaved by index in the same response: each must
    /// keep its own id, name and arguments separate.
    #[test]
    fn two_interleaved_tool_calls_do_not_mix_arguments() {
        let mut acc = ToolCallAccumulator::new();
        let _ = parse_stream_frame_with_tools(
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":"{\"p"}},
                {"index":1,"id":"call_b","type":"function","function":{"name":"write_file","arguments":"{\"x"}}
            ]}}]}"#,
            &mut acc,
        );
        let _ = parse_stream_frame_with_tools(
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"\":1}"}},
                {"index":1,"function":{"arguments":"\":2}"}}
            ]}}]}"#,
            &mut acc,
        );
        let events = parse_stream_frame_with_tools(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            &mut acc,
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallRequested {
                    id: "call_a".into(),
                    name: "read_file".into(),
                    arguments: r#"{"p":1}"#.into(),
                },
                StreamEvent::ToolCallRequested {
                    id: "call_b".into(),
                    name: "write_file".into(),
                    arguments: r#"{"x":2}"#.into(),
                },
                StreamEvent::FinishReason("tool_calls".into()),
            ]
        );
    }

    /// End-to-end through `stream()`: the real HTTP/SSE path, not just
    /// the frame parser, produces one complete `ToolCallRequested`.
    #[test]
    fn stream_emits_a_complete_tool_call_through_the_real_sse_loop() {
        const BODY: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"type\":\"function\",\"function\":{\"name\":\"run_shell\",\"arguments\":\"{\\\"program\\\":\\\"ec\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"ho\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _req) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = provider(&base);
        let events = collect(&p, &request());

        let tool_calls: Vec<&StreamEvent> = events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ToolCallRequested { .. }))
            .collect();
        assert_eq!(
            tool_calls,
            vec![&StreamEvent::ToolCallRequested {
                id: "call_9".into(),
                name: "run_shell".into(),
                arguments: r#"{"program":"echo"}"#.into(),
            }]
        );
    }

    /// Request-side serialization: a non-empty `tools` list reaches the
    /// wire as the OpenAI function-calling shape, and an empty one is
    /// omitted entirely so every pre-existing body-shape test (which
    /// never set `tools`) keeps asserting against the same body.
    #[test]
    fn tools_are_serialized_as_function_definitions_and_omitted_when_empty() {
        const BODY: &str = "data: [DONE]\n\n";
        let (base, requests) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = provider(&base);

        let mut with_tools = request();
        with_tools.tools = vec![ToolDefinition {
            name: "read_file".into(),
            description: "Reads a file".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        collect(&p, &with_tools);
        let seen = requests.lock().unwrap().join("\n");
        assert!(seen.contains("\"tools\""), "{seen}");
        assert!(seen.contains("\"name\":\"read_file\""), "{seen}");
        assert!(seen.contains("\"type\":\"function\""), "{seen}");

        requests.lock().unwrap().clear();
        collect(&p, &request());
        let seen_without = requests.lock().unwrap().join("\n");
        assert!(
            !seen_without.contains("\"tools\""),
            "an empty tools list must not appear on the wire: {seen_without}"
        );
    }

    /// Round trip serialization: an assistant tool-calls message and a
    /// following tool-result message reach the wire in the shape the
    /// next request must carry, so a provider can match the result to
    /// the call it answers.
    #[test]
    fn assistant_tool_calls_and_tool_results_serialize_to_the_documented_shape() {
        const BODY: &str = "data: [DONE]\n\n";
        let (base, requests) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = provider(&base);
        let mut req = request();
        req.messages
            .push(Message::assistant_tool_calls(vec![ToolCallRecord {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.txt"}"#.into(),
            }]));
        req.messages
            .push(Message::tool_result("call_1", "file contents"));
        collect(&p, &req);

        let seen = requests.lock().unwrap().join("\n");
        assert!(seen.contains("\"tool_calls\""), "{seen}");
        assert!(seen.contains("\"id\":\"call_1\""), "{seen}");
        assert!(seen.contains("\"tool_call_id\":\"call_1\""), "{seen}");
        assert!(seen.contains("\"role\":\"tool\""), "{seen}");
        assert!(seen.contains("file contents"), "{seen}");
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

    /// A 401 is classified as `Auth`, not the generic `Protocol`, so a
    /// future retry/failover decision can match on the type rather than
    /// re-parsing this message -- and specifically so it is never
    /// retried (see `is_retriable`).
    #[test]
    fn an_http_401_is_reported_as_auth_with_the_providers_own_explanation() {
        const BODY: &str = r#"{"error":{"message":"invalid api key"}}"#;
        let (base, _r) = serve("HTTP/1.1 401 Unauthorized", "application/json", BODY);
        let p = provider(&base);
        let err = p.stream(&request(), &|| false, &mut |_| true).unwrap_err();
        assert!(!err.is_retriable());
        match err {
            ProviderError::Auth(detail) => {
                assert!(detail.contains("401"), "{detail}");
                assert!(detail.contains("invalid api key"), "{detail}");
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    /// A firewall page (Cloudflare's block is a 403 HTML page) is not a key verdict.
    #[test]
    fn an_html_outage_or_rate_limit_page_stays_retriable() {
        let page = "<html><head><title>502 Bad Gateway</title></head></html>";
        let e = classify_http_body("x", 503, None, page, "");
        assert!(matches!(e, ProviderError::Server { status: 503, .. }) && e.is_retriable(), "{e:?}");
        let e = classify_http_body("x", 429, Some(Duration::from_secs(7)), page, "");
        assert!(
            matches!(e, ProviderError::RateLimited { retry_after: Some(_), .. }),
            "{e:?}"
        );
    }

    #[test]
    fn a_403_web_page_is_not_reported_as_a_refused_key() {
        const PAGE: &str = "<!DOCTYPE html><html><head><title>Attention Required! | Cloudflare</title></head></html>";
        let (base, _r) = serve("HTTP/1.1 403 Forbidden", "text/html", PAGE);
        let err = provider(&base).list_models().unwrap_err();
        match err {
            ProviderError::Protocol(d) => assert!(d.contains("Cloudflare") && d.contains("web page"), "{d}"),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    /// 403, 429 (with and without Retry-After), and 5xx each classify
    /// distinctly -- the test matrix the next phase's review asked for.
    #[test]
    fn http_error_statuses_classify_into_distinct_typed_variants() {
        let (base, _r) = serve("HTTP/1.1 403 Forbidden", "application/json", "{}");
        let err = provider(&base)
            .stream(&request(), &|| false, &mut |_| true)
            .unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)), "403: {err:?}");

        let (base, _r) = serve("HTTP/1.1 429 Too Many Requests", "application/json", "{}");
        let err = provider(&base)
            .stream(&request(), &|| false, &mut |_| true)
            .unwrap_err();
        match err {
            ProviderError::RateLimited { retry_after, .. } => assert_eq!(retry_after, None),
            other => panic!("429 with no header: expected RateLimited, got {other:?}"),
        }

        for status in [
            "HTTP/1.1 500 Internal Server Error",
            "HTTP/1.1 503 Service Unavailable",
        ] {
            let (base, _r) = serve(status, "application/json", "{}");
            let err = provider(&base)
                .stream(&request(), &|| false, &mut |_| true)
                .unwrap_err();
            assert!(
                matches!(err, ProviderError::Server { .. }),
                "{status}: {err:?}"
            );
        }
    }

    /// A `Retry-After` header is read before the body is consumed and
    /// surfaces as the provider's own stated wait -- never invented.
    #[test]
    fn a_retry_after_header_is_parsed_into_the_rate_limited_variant() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::{BufRead, BufReader, Write};
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                    break;
                }
            }
            let resp = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            // Windows discards unread queued data on an abrupt close,
            // which reached the client as a reset instead of the 429;
            // waiting for the client to hang up first avoids that.
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let mut drain = Vec::new();
            let _ = std::io::Read::read_to_end(&mut reader, &mut drain);
        });
        let p = provider(&format!("http://{addr}"));
        let err = p.stream(&request(), &|| false, &mut |_| true).unwrap_err();
        match err {
            ProviderError::RateLimited {
                retry_after: Some(d),
                ..
            } => assert_eq!(d, Duration::from_secs(7)),
            other => panic!("expected RateLimited with retry_after, got {other:?}"),
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
            access_file: None,
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
            status.detail.contains(&format!("se connect {} <api-url>", TEST_SERVICE.id)),
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
            // Truncated, not Protocol: the whole point of the distinct
            // variant is that the caller (ProviderRunner) can preserve
            // `seen` as Outcome::Incomplete rather than discarding it.
            Err(ProviderError::Truncated(d)) => {
                assert!(d.contains("without completing"), "{d}");
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
            wire: Wire::OpenAi,
        };
        const BODY: &str = "data: [DONE]

";
        let (base, requests) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let p = OpenAiCompatible {
            service: KEYLESS,
            fixed_key: None,
            fixed_base_url: Some(base),
            access_file: None,
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

    /// Regression for a confirmed credential-disclosure defect:
    /// `api_key()` was consulted before `key_required`, so a stray
    /// `OLLAMA_API_KEY` in the environment made SE send a bearer token
    /// to a local Ollama. A nonsense `Bearer` to `http://localhost:11434`
    /// is strictly worse than sending none -- a local process
    /// credential leak. `key_or_error` must short-circuit on
    /// `key_required: false` before anything reads the environment.
    #[test]
    fn a_keyless_service_never_reads_the_key_var_at_all() {
        const KEYLESS: Service = Service {
            id: "keyless-ordering",
            display_name: "Keyless",
            key_var: "SE_TEST_KEYLESS_ORDERING_KEY",
            default_base_url: "",
            base_url_var: "SE_TEST_KEYLESS_ORDERING_BASE",
            key_required: false,
            wire: Wire::OpenAi,
        };
        const BODY: &str = "data: [DONE]\n\n";
        let (base, requests) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);

        // A hostile env var is explicitly set. If the fix has regressed
        // it will be read and forwarded.
        std::env::set_var(KEYLESS.key_var, "nvapi-this-must-never-leave-the-machine");
        let p = OpenAiCompatible {
            service: KEYLESS,
            fixed_key: None,
            fixed_base_url: Some(base),
            access_file: None,
        };
        p.stream(&request(), &|| false, &mut |_| true).unwrap();
        let seen = requests.lock().unwrap().join("\n");
        assert!(
            !seen.to_lowercase().contains("authorization"),
            "a keyless service must not send any Authorization header: {seen}"
        );
        assert!(
            !seen.contains("nvapi-this-must-never-leave-the-machine"),
            "the stray key must not appear anywhere in the request: {seen}"
        );
        std::env::remove_var(KEYLESS.key_var);
    }

    /// E2 end-to-end: a credential stored (pinned) against one origin
    /// must be refused when the provider is later pointed at a
    /// different one -- the exact exfiltration path a hostile
    /// `*_BASE_URL` env var exploited before this fix.
    #[test]
    fn a_provider_refuses_to_send_a_credential_pinned_to_a_different_origin() {
        let _guard = credentials::test_lock();
        const ID: &str = "origin-pin-e2e";
        std::env::remove_var("SE_TEST_PIN_BASE");

        const BODY: &str = "data: [DONE]\n\n";
        let (first_base, _r1) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);
        let (second_base, second_requests) = serve("HTTP/1.1 200 OK", "text/event-stream", BODY);

        let service = Service {
            id: ID,
            display_name: "Pin test",
            key_var: "SE_TEST_PIN_KEY_UNSET",
            default_base_url: "",
            base_url_var: "SE_TEST_PIN_BASE",
            key_required: true,
            wire: Wire::OpenAi,
        };

        // Store exactly as /connect does: through the real credential
        // path, pinned to the first origin.
        credentials::store(ID, "test-key-not-real", Some(&first_base)).unwrap();

        // The endpoint changes -- e.g. the env var is now pointed
        // somewhere else entirely.
        std::env::set_var("SE_TEST_PIN_BASE", &second_base);
        let p = OpenAiCompatible::new(service);
        let err = p
            .stream(&request(), &|| false, &mut |_| true)
            .expect_err("a changed origin must be refused");
        match err {
            ProviderError::NotConfigured(detail) => {
                assert!(detail.contains("run `se connect`"), "{detail}");
            }
            other => panic!("expected NotConfigured, got {other:?}"),
        }
        assert!(
            second_requests.lock().unwrap().is_empty(),
            "no request must reach the new origin"
        );

        credentials::delete(ID).unwrap();
        std::env::remove_var("SE_TEST_PIN_BASE");
    }

    /// Any service that sends a credential must do so over TLS, or over
    /// a loopback address (for local models and the mock servers used
    /// in these tests). Regression for a verified credential-exfil
    /// path: `OPENAI_BASE_URL=http://attacker/v1` would otherwise send
    /// the stored key to an attacker-controlled host on the next turn.
    #[test]
    fn a_credential_bearing_service_refuses_http_to_a_non_loopback_host() {
        let p =
            OpenAiCompatible::with_fixed(TEST_SERVICE, "sk-test", "http://attacker.example.com/v1");
        let err = p
            .stream(&request(), &|| false, &mut |_| true)
            .expect_err("http:// to a non-loopback host must be refused");
        match err {
            ProviderError::NotConfigured(detail) => {
                assert!(detail.contains("https"), "{detail}");
                assert!(detail.contains("loopback"), "{detail}");
                assert!(
                    !detail.contains("sk-test"),
                    "the key must not appear: {detail}"
                );
            }
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    #[test]
    fn https_and_loopback_http_are_both_accepted() {
        // Loopback http is permitted -- the tests themselves rely on it.
        assert!(is_safe_origin("http://127.0.0.1:8080"));
        assert!(is_safe_origin("http://localhost/v1"));
        assert!(is_safe_origin("http://[::1]:11434"));
        // Any https destination is safe.
        assert!(is_safe_origin("https://api.openai.com/v1"));
        // Non-loopback http and other schemes are not.
        assert!(!is_safe_origin("http://attacker.example.com/v1"));
        assert!(!is_safe_origin("http://127.0.0.1.attacker.com/"));
        assert!(!is_safe_origin("file:///etc/passwd"));
        assert!(!is_safe_origin("ftp://example.com/"));
        assert!(!is_safe_origin(""));
    }

    #[test]
    fn a_url_carrying_userinfo_is_refused_over_either_scheme() {
        // The defect this closes: the previous version only looked for
        // userinfo on the http:// branch, so an https:// URL with a
        // credential embedded in it -- the more dangerous case, since
        // https is otherwise trusted outright -- passed unchecked.
        assert!(!is_safe_origin("https://user:key@attacker.example.com/v1"));
        assert!(!is_safe_origin("http://user:key@127.0.0.1/v1"));
        assert!(!is_safe_origin("http://user:key@localhost/v1"));
        // A bare "@" with no credential shape is still userinfo syntax
        // and still refused -- the rule is "contains @", not "looks
        // like a secret".
        assert!(!is_safe_origin("https://@attacker.example.com/"));
    }

    #[test]
    fn a_keyless_service_still_refuses_a_non_loopback_http_endpoint() {
        // E6b: HTTPS/loopback is a data-confidentiality control, not
        // only a credential control. A keyless service pointed at a
        // remote http:// host would ship every prompt in cleartext
        // even though no credential is involved. The previous version
        // only ran this check `if self.service.key_required`, so a
        // keyless service skipped it entirely.
        const KEYLESS: Service = Service {
            id: "keyless-remote",
            display_name: "Keyless",
            key_var: "SE_TEST_KEYLESS_REMOTE_KEY_UNSET",
            default_base_url: "",
            base_url_var: "SE_TEST_KEYLESS_REMOTE_BASE",
            key_required: false,
            wire: Wire::OpenAi,
        };
        let p = OpenAiCompatible {
            service: KEYLESS,
            fixed_key: None,
            fixed_base_url: Some("http://attacker.example.com:11434".to_string()),
            access_file: None,
        };
        let err = p.stream(&request(), &|| false, &mut |_| true).unwrap_err();
        match err {
            ProviderError::NotConfigured(detail) => {
                assert!(detail.contains("insecure"), "{detail}");
            }
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

    /// Guards against a regression of the "dead `header_safe`" defect:
    /// a non-ASCII credential must not reach `ureq` and must not leak
    /// back through a transport error.
    #[test]
    fn a_non_ascii_credential_is_refused_before_the_network_is_touched() {
        let p =
            OpenAiCompatible::with_fixed(TEST_SERVICE, "sk-caf\u{e9}-key", "http://127.0.0.1:1");
        let err = p
            .stream(&request(), &|| false, &mut |_| true)
            .expect_err("non-ASCII must be refused");
        match err {
            ProviderError::NotConfigured(detail) => {
                assert!(detail.contains("non-ASCII"), "{detail}");
                assert!(!detail.contains("caf"), "key must not appear: {detail}");
            }
            // Must not be a Transport error, which is how the pre-fix
            // code leaked the header value back through ureq.
            other => panic!("expected NotConfigured, got {other:?}"),
        }
    }

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
            messages: vec![Message::text(Role::User, "go")],
            max_tokens: 32,
            tools: Vec::new(),
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
