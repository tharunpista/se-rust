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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
}

/// What to ask the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
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
    /// The provider signalled the end of the response.
    Done,
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
    /// The provider answered, with a refusal or an error status.
    Protocol(String),
    /// The request never completed: DNS, TLS, timeout, connection.
    Transport(String),
    /// The caller asked to stop. Not a failure.
    Cancelled,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::NotConfigured(d) => write!(f, "not configured: {d}"),
            ProviderError::Protocol(d) => write!(f, "{d}"),
            ProviderError::Transport(d) => write!(f, "could not reach the provider: {d}"),
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
}
