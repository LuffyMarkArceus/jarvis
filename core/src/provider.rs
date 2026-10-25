//! The model server boundary.
//!
//! Everything above this module - the router, the planner, the orchestrator -
//! decides *what* to ask. This module decides *how* to ask it, and it is the
//! only place that knows a provider's wire format. Two things follow from
//! that: switching servers is a config change, and the orchestration tests do
//! not need one running.
//!
//! A provider is a trait object rather than a concrete client because the
//! orchestrator holds one and the daemon may serve turns for a long time. The
//! trait is deliberately small: one buffered turn and one streamed turn, both
//! returning the same `ChatTurn` so token accounting cannot differ between
//! them.

use crate::cancel::Cancellation;
use crate::config::Config;
use crate::error::{JarvisError, Result};
use crate::model::{ChatMessage, ChatTurn, OllamaClient};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

/// One turn request, in provider-neutral terms.
///
/// `options` stays provider-shaped on purpose: the orchestrator sets sampling
/// and context values once, and each provider translates what it understands.
/// Rewriting it into a neutral enum would mean inventing a third vocabulary
/// for a handful of keys.
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub system: Option<&'a str>,
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [Value]>,
    pub options: Option<Value>,
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// The name used in logs and in `jarvis status`.
    fn name(&self) -> &'static str;

    /// One complete turn.
    async fn chat_turn(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&Cancellation>,
    ) -> Result<ChatTurn>;

    /// One turn, forwarding text deltas to `sink` as they arrive.
    ///
    /// The finished message is still returned: the caller needs it for tool
    /// calls and token accounting, and a provider that cannot stream may
    /// answer with a single delta without breaking that contract.
    async fn chat_turn_streaming(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&Cancellation>,
        sink: &UnboundedSender<String>,
    ) -> Result<ChatTurn>;
}

/// Ollama's native API, which the project is developed against.
pub struct OllamaProvider {
    inner: OllamaClient,
}

impl OllamaProvider {
    pub fn new(base_url: &str) -> Self {
        Self {
            inner: OllamaClient::new(base_url),
        }
    }
}

#[async_trait::async_trait]
impl Provider for OllamaProvider {
    fn name(&self) -> &'static str {
        "ollama"
    }

    async fn chat_turn(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&Cancellation>,
    ) -> Result<ChatTurn> {
        self.inner
            .chat_turn_cancellable(
                request.model,
                request.system,
                request.messages,
                request.tools,
                request.options,
                cancel,
            )
            .await
    }

    async fn chat_turn_streaming(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&Cancellation>,
        sink: &UnboundedSender<String>,
    ) -> Result<ChatTurn> {
        self.inner
            .chat_turn_streaming(
                request.model,
                request.system,
                request.messages,
                request.tools,
                request.options,
                cancel,
                sink,
            )
            .await
    }
}

/// Build the provider named by the configuration.
///
/// An unknown name is a configuration error, not a reason to guess: silently
/// falling back would send prompts to a server the user did not choose.
pub fn from_config(config: &Config) -> Result<Arc<dyn Provider>> {
    match config.model.provider.as_str() {
        "ollama" => Ok(Arc::new(OllamaProvider::new(&config.model.base_url))),
        "openai" => Ok(Arc::new(crate::openai::OpenAiProvider::from_config(
            config,
        )?)),
        other => Err(JarvisError::Config(format!(
            "unknown model provider '{other}' - expected 'ollama' or 'openai'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(provider: &str) -> Config {
        let mut config = Config::default();
        config.model.provider = provider.to_string();
        config
    }

    #[test]
    fn ollama_is_the_default_and_is_constructed() {
        let provider = from_config(&config_with("ollama")).expect("ollama provider");
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn an_unknown_provider_is_an_error_not_a_guess() {
        let message = match from_config(&config_with("gpt-9000")) {
            Ok(provider) => panic!("built a provider named {}", provider.name()),
            Err(error) => error.to_string(),
        };
        assert!(message.contains("gpt-9000"), "{message}");
        assert!(message.contains("ollama"), "{message}");
    }
}
