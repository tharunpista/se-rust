//! Model providers.
//!
//! One trait, and adapters that implement it. The trait's primary method
//! is [`ModelProvider::stream`], not a request/response `send`: the
//! previous implementation had streaming as an unused extra, and
//! consequently every answer arrived whole, no matter how long it took
//! to generate. Here, streaming is the only way to run a turn, so it
//! cannot quietly stop being used.
//!
//! This crate performs blocking HTTP on the caller's thread. That is
//! safe because the only caller is a task worker (see `se-core`), never
//! an interface.

pub mod credentials;
pub mod openai_compatible;

pub use credentials::Source;
pub use openai_compatible::{OpenAiCompatible, Service, CATALOGUE};

use serde::{Deserialize, Serialize};

/// A message in a conversation.
///
/// `tool_calls` is non-empty only on an assistant message that made
/// tool calls (the message carries no `content` in that case, matching
/// the OpenAI-compatible wire shape); `tool_call_id` is set only on a
/// `Role::Tool` message, naming which call it answers. Plain
/// text turns use neither — [`Message::text`] is the constructor for
/// that common case so the four other construction sites in this
/// workspace do not each have to spell out the empty fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// An ordinary text turn: no tool calls either way.
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Message {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// The assistant turn that requested one or more tool calls.
    pub fn assistant_tool_calls(calls: Vec<ToolCallRecord>) -> Self {
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: calls,
            tool_call_id: None,
        }
    }

    /// The result of one tool call, fed back as the next turn's input.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Message {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// One tool call an assistant message made, as carried back to the
/// provider on the next round trip (the wire shape's
/// `assistant.tool_calls[]`, fully accumulated — never partial JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    /// A tool's result, answering one call by id (`tool_call_id`).
    Tool,
}

/// A function the model may call, advertised as a JSON Schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// What to ask the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    /// Empty means "no tools offered" — the wire format omits the
    /// `tools` field entirely in that case rather than sending `[]`.
    pub tools: Vec<ToolDefinition>,
}

/// One piece of a streamed response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// Token accounting, if the provider sent any. Not synthesised.
    Usage {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
    },
    /// The provider reported an error *inside* a 200 response stream.
    ///
    /// Vendors do this: the HTTP status is 200, the body opens as an
    /// event stream, and a later frame carries `{"error": …}`. Without
    /// this variant such a frame parsed as "no content" and was
    /// silently dropped, so a failed generation finished as a complete,
    /// usually empty, answer.
    Error(String),
    /// Why the provider stopped, verbatim (`stop`, `length`,
    /// `content_filter`, …).
    ///
    /// Carried because `length` means the answer was truncated, which
    /// must not be reported as a finished reply.
    FinishReason(String),
    /// The provider signalled the end of the response (`[DONE]`).
    Done,
    /// One tool call's arguments are fully accumulated. The provider
    /// layer does the per-index accumulation internally (OpenAI's wire
    /// format streams `tool_calls[].function.arguments` as fragments
    /// keyed by index); callers of `stream()` never see partial JSON.
    ToolCallRequested {
        id: String,
        name: String,
        arguments: String,
    },
}

/// What a provider reports about one of its models.
///
/// Every capability is optional, and absent means "the provider did not
/// say". A local table of guesses would be indistinguishable on screen
/// from fact, so there isn't one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
    pub supports_tools: Option<bool>,
    pub supports_vision: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// No credential, or no endpoint. Never reached the network.
    NotConfigured(String),
    /// The credential was rejected (HTTP 401/403). Never retry-eligible:
    /// hammering an invalid key wastes attempts and can trip the
    /// provider's own abuse detection. The user must fix the credential.
    Auth(String),
    /// The provider refused on policy grounds (a content filter, a
    /// safety block). Never retry-eligible, and specifically never
    /// eligible for failover to a different provider/model -- routing
    /// around a refusal is policy evasion, not error recovery.
    Policy(String),
    /// The request itself was malformed (HTTP 400 with no policy
    /// marker): a bad parameter, an unsupported combination. Retrying
    /// the same request changes nothing; the request must be fixed
    /// first.
    BadRequest(String),
    /// The provider is throttling. `retry_after` is the provider's own
    /// stated wait, when it gave one -- never invented. Eligible for a
    /// bounded, backed-off retry.
    RateLimited {
        retry_after: Option<std::time::Duration>,
        detail: String,
    },
    /// The provider's own infrastructure failed (HTTP 5xx). Eligible
    /// for a bounded retry.
    Server { status: u16, detail: String },
    /// A real answer started but could not finish for a reason that is
    /// not a transient failure: an output-length truncation, or the
    /// connection ending with no terminal signal. Never retry-eligible
    /// with the same parameters -- retrying a truncation at the same
    /// token budget produces the same truncation.
    Truncated(String),
    /// The provider answered in a shape this adapter could not parse,
    /// or reported an error whose nature does not fit the variants
    /// above. Not retry-eligible: the shape will not change on retry.
    Protocol(String),
    /// The request never completed: DNS, TLS, connection reset.
    /// Eligible for a bounded retry.
    Transport(String),
    /// A bound SE itself enforces -- not something the provider
    /// reported -- was exceeded. Eligible for a bounded retry (a
    /// different attempt may simply be faster).
    Timeout(TimeoutKind),
    /// The caller asked to stop. Not a failure, and never retried.
    Cancelled,
}

/// Which SE-enforced bound a [`ProviderError::Timeout`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// No byte arrived for longer than the configured inactivity bound.
    Idle,
    /// The whole turn ran longer than its absolute deadline, regardless
    /// of whether bytes kept arriving.
    WholeTurn,
}

impl ProviderError {
    /// Whether this failure is ever eligible for a retry or a
    /// capability-aware failover, independent of what produced the
    /// error message.
    ///
    /// Deliberately a match on the *type*, not a scan of `to_string()`:
    /// a retry loop built on string-matching a provider's error body is
    /// exactly what the charter forbids, because wording varies by
    /// provider and by locale and a match that works today silently
    /// stops matching tomorrow. This function is the one place that
    /// decision is made, so it can be the one place it is tested.
    pub fn is_retriable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. }
            | ProviderError::Server { .. }
            | ProviderError::Transport(_)
            | ProviderError::Timeout(_) => true,
            ProviderError::NotConfigured(_)
            | ProviderError::Auth(_)
            | ProviderError::Policy(_)
            | ProviderError::BadRequest(_)
            | ProviderError::Truncated(_)
            | ProviderError::Protocol(_)
            | ProviderError::Cancelled => false,
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::NotConfigured(d) => write!(f, "not configured: {d}"),
            ProviderError::Auth(d) => write!(f, "authentication failed: {d}"),
            ProviderError::Policy(d) => write!(f, "refused on policy grounds: {d}"),
            ProviderError::BadRequest(d) => write!(f, "the request was rejected: {d}"),
            ProviderError::RateLimited {
                retry_after,
                detail,
            } => match retry_after {
                Some(d) => write!(f, "rate limited (retry after {}s): {detail}", d.as_secs()),
                None => write!(f, "rate limited: {detail}"),
            },
            ProviderError::Server { status, detail } => {
                write!(f, "provider error (HTTP {status}): {detail}")
            }
            ProviderError::Truncated(d) => write!(f, "{d}"),
            ProviderError::Protocol(d) => write!(f, "{d}"),
            ProviderError::Transport(d) => write!(f, "could not reach the provider: {d}"),
            ProviderError::Timeout(TimeoutKind::Idle) => {
                write!(f, "the provider stopped sending data")
            }
            ProviderError::Timeout(TimeoutKind::WholeTurn) => {
                write!(f, "the turn exceeded its time limit")
            }
            ProviderError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// What is known about a provider without making a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub configured: bool,
    /// Safe to display: never contains any part of a credential.
    pub detail: String,
    pub source: Source,
}

/// A model provider.
pub trait ModelProvider: Send + Sync {
    /// Stable identifier, used as the credential key.
    fn id(&self) -> &str;

    fn display_name(&self) -> &str;

    /// The environment variable that overrides stored credentials.
    fn env_var(&self) -> &str;

    /// Whether a credential and endpoint exist. Makes no network call,
    /// and says so rather than implying the provider is reachable.
    fn detect(&self) -> Status;

    /// Streams a response, calling `on_event` for each piece.
    ///
    /// `cancelled` is polled between pieces so a long generation can be
    /// stopped without waiting for it to finish. The callback returning
    /// `false` also stops the stream, which is how a disconnected
    /// interface ends a turn promptly.
    fn stream(
        &self,
        request: &ChatRequest,
        cancelled: &dyn Fn() -> bool,
        on_event: &mut dyn FnMut(StreamEvent) -> bool,
    ) -> Result<(), ProviderError>;

    /// Lists the models the provider itself reports. Metadata only: no
    /// prompt is sent and no tokens are billed.
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Confirms the credential actually works, by listing models.
    ///
    /// Separate from [`detect`](Self::detect) because "a key is present"
    /// and "the key is accepted" are different claims.
    fn validate(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.list_models()
    }

    /// The origin (scheme + host + port) this provider's credential
    /// will be sent to right now, if it has a fixed endpoint.
    ///
    /// Used only at `/connect` time, to pin the credential to the
    /// endpoint it was stored for (see `credentials::check_origin`).
    /// `None` by default: a provider with no configured endpoint has
    /// nothing to pin yet.
    fn current_origin(&self) -> Option<String> {
        None
    }
}

/// The providers SE knows how to look for.
///
/// Registration says nothing about usability: each entry reports its own
/// credential state, and nothing is claimed about a model until the
/// provider's own endpoint answers.
pub struct Registry {
    providers: Vec<Box<dyn ModelProvider>>,
}

impl Registry {
    /// Builds the registry from the catalogue.
    ///
    /// Cheap and stateless: no credential is read here, so a registry
    /// built at any moment sees the same credential state as any other.
    /// That is deliberate -- the opposite arrangement was a real defect.
    pub fn new() -> Self {
        Registry {
            providers: openai_compatible::registered(),
        }
    }

    pub fn empty() -> Self {
        Registry {
            providers: Vec::new(),
        }
    }

    pub fn register(&mut self, provider: Box<dyn ModelProvider>) {
        self.providers.push(provider);
    }

    pub fn get(&self, id: &str) -> Option<&dyn ModelProvider> {
        self.providers
            .iter()
            .find(|p| p.id() == id)
            .map(|p| p.as_ref())
    }

    pub fn ids(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.id()).collect()
    }

    /// Every provider with its current state.
    pub fn statuses(&self) -> Vec<(&str, &str, Status)> {
        self.providers
            .iter()
            .map(|p| (p.id(), p.display_name(), p.detect()))
            .collect()
    }

    /// Providers that have a credential. Still not a claim that any of
    /// them is reachable.
    pub fn configured(&self) -> Vec<&dyn ModelProvider> {
        self.providers
            .iter()
            .filter(|p| p.detect().configured)
            .map(|p| p.as_ref())
            .collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_lists_the_whole_catalogue() {
        let registry = Registry::new();
        let ids = registry.ids();
        assert!(ids.contains(&"nvidia"), "{ids:?}");
        assert!(ids.contains(&"openai"), "{ids:?}");
        assert!(ids.contains(&"openrouter"), "{ids:?}");
        assert_eq!(ids.len(), CATALOGUE.len());
    }

    #[test]
    fn an_unknown_provider_is_absent_rather_than_invented() {
        let registry = Registry::new();
        assert!(registry.get("not-a-provider").is_none());
    }

    #[test]
    fn an_empty_registry_reports_nothing_configured() {
        let registry = Registry::empty();
        assert!(registry.configured().is_empty());
        assert!(registry.statuses().is_empty());
    }

    #[test]
    fn errors_render_without_leaking_anything() {
        let e = ProviderError::NotConfigured("no credential for nvidia".into());
        assert_eq!(e.to_string(), "not configured: no credential for nvidia");
        assert_eq!(ProviderError::Cancelled.to_string(), "cancelled");
    }

    /// The negative cases the charter is explicit about: these must
    /// never be retried or failed over, no matter how the retry logic
    /// is later written. A match on the type, never on wording.
    #[test]
    fn auth_policy_and_bad_request_are_never_retriable() {
        assert!(!ProviderError::Auth("bad key".into()).is_retriable());
        assert!(!ProviderError::Policy("content filtered".into()).is_retriable());
        assert!(!ProviderError::BadRequest("unknown parameter".into()).is_retriable());
        assert!(!ProviderError::NotConfigured("no credential".into()).is_retriable());
        assert!(!ProviderError::Cancelled.is_retriable());
    }

    #[test]
    fn transient_failures_are_retriable() {
        assert!(ProviderError::RateLimited {
            retry_after: None,
            detail: "slow down".into()
        }
        .is_retriable());
        assert!(ProviderError::Server {
            status: 503,
            detail: "overloaded".into()
        }
        .is_retriable());
        assert!(ProviderError::Transport("connection reset".into()).is_retriable());
        assert!(ProviderError::Timeout(TimeoutKind::Idle).is_retriable());
        assert!(ProviderError::Timeout(TimeoutKind::WholeTurn).is_retriable());
    }

    /// Truncation and an unparseable response shape are not transient:
    /// retrying with the same parameters reproduces the same problem.
    #[test]
    fn truncation_and_protocol_shape_errors_are_not_retriable() {
        assert!(!ProviderError::Truncated("output limit reached".into()).is_retriable());
        assert!(!ProviderError::Protocol("unexpected shape".into()).is_retriable());
    }

    #[test]
    fn a_retry_after_duration_is_shown_when_the_provider_gave_one() {
        let with = ProviderError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(12)),
            detail: "too many requests".into(),
        };
        assert!(with.to_string().contains("12s"), "{with}");

        let without = ProviderError::RateLimited {
            retry_after: None,
            detail: "too many requests".into(),
        };
        assert!(!without.to_string().contains("retry after"), "{without}");
    }
}
