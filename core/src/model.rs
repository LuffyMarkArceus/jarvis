//! Ollama model client.
//!
//! Two tool-calling paths are supported:
//!
//! * **Native function calling** - the `tools` array is sent with the
//!   request and the model replies with structured `tool_calls`. This is
//!   preferred: the model was trained for it and small models follow it far
//!   more reliably than "please reply with JSON".
//! * **Text parsing** - the model's reply is scanned for JSON tool calls in
//!   any of the common shapes (see `tools::parser`). Used for providers or
//!   models without a native function-calling interface.

use crate::error::{JarvisError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone)]
pub struct OllamaClient {
    base_url: String,
    client: reqwest::Client,
}

/// The function payload of a native tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFunction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    pub name: String,
    /// Arguments as an object. Some providers send a JSON-encoded string
    /// instead, which is normalised at the call site.
    #[serde(default)]
    pub arguments: Value,
}

/// A native tool call returned by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub function: ModelFunction,
}

/// A single chat message, in the shape Ollama expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ModelToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl ChatMessage {
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: None,
            tool_name: None,
        }
    }

    /// Echo back the assistant turn that requested tools.
    pub fn assistant_with_tool_calls(content: &str, calls: Vec<ModelToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_calls: Some(calls),
            tool_name: None,
        }
    }

    /// The result of a tool call, fed back to the model.
    pub fn tool_result(tool_name: &str, content: &str) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.to_string(),
            tool_calls: None,
            tool_name: Some(tool_name.to_string()),
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    /// JSON-schema tool definitions, in OpenAI/Ollama function format.
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    /// Provider tuning options (temperature, top_p, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<Value>,
}

/// One model turn plus the token accounting the provider reports.
///
/// The counts are what the provider actually measured, not an estimate. They
/// are the only honest way to know how close a prompt is to the context
/// window, so they are surfaced rather than discarded.
#[derive(Debug, Clone)]
pub struct ChatTurn {
    pub message: ChatMessage,
    /// Tokens in everything we sent: system prompt, history, tools, request.
    pub prompt_tokens: Option<u64>,
    /// Tokens the model generated.
    pub completion_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    message: ChatMessage,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

impl OllamaClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn chat(&self, model: &str, user_message: &str) -> Result<String> {
        let messages = vec![ChatMessage::text("user", user_message)];
        let turn = self.chat_turn(model, None, &messages, None).await?;
        Ok(turn.message.content)
    }

    /// Send a chat request with an optional system prompt.
    ///
    /// The system prompt is delivered as a `system` role message, which
    /// Ollama's chat template renders separately from the conversation. That
    /// keeps behavioural rules out of the user turn, which matters a lot for
    /// small models that otherwise blend instructions with the request.
    pub async fn chat_with_system(
        &self,
        model: &str,
        system: Option<&str>,
        user_message: &str,
    ) -> Result<String> {
        let mut messages = Vec::new();
        if let Some(s) = system {
            let s = s.trim();
            if !s.is_empty() {
                messages.push(ChatMessage::text("system", s));
            }
        }
        messages.push(ChatMessage::text("user", user_message));
        let turn = self.chat_turn(model, None, &messages, None).await?;
        Ok(turn.message.content)
    }

    /// Run a full conversation turn, optionally exposing tools to the model.
    ///
    /// Returns the assistant message, which may contain native tool calls.
    pub async fn chat_turn(
        &self,
        model: &str,
        system: Option<&str>,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
    ) -> Result<ChatTurn> {
        self.chat_turn_with_options(model, system, messages, tools, None)
            .await
    }

    /// As [`Self::chat_turn`], with explicit provider options.
    pub async fn chat_turn_with_options(
        &self,
        model: &str,
        system: Option<&str>,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        options: Option<Value>,
    ) -> Result<ChatTurn> {
        let mut all_messages: Vec<ChatMessage> = Vec::new();
        if let Some(s) = system {
            let s = s.trim();
            if !s.is_empty() {
                all_messages.push(ChatMessage::text("system", s));
            }
        }
        all_messages.extend(messages.iter().cloned());

        let req = ChatRequest {
            model: model.to_string(),
            messages: all_messages,
            stream: false,
            tools: tools.map(|t| t.to_vec()),
            options,
        };

        let resp = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&req)
            .send()
            .await
            .map_err(|e| JarvisError::Model(format!("Request failed: {}", e)))?;

        if !resp.status().is_success() {
            return Err(JarvisError::Model(format!("HTTP {}", resp.status())));
        }

        let chat_resp: ChatResponse = resp
            .json()
            .await
            .map_err(|e| JarvisError::Model(format!("Parse error: {}", e)))?;

        Ok(ChatTurn {
            message: chat_resp.message,
            prompt_tokens: chat_resp.prompt_eval_count,
            completion_tokens: chat_resp.eval_count,
        })
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        let resp = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map_err(|e| JarvisError::Model(format!("Request failed: {}", e)))?;

        let json: serde_json::Value = resp.json().await?;
        let models = json["models"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(models)
    }
}
