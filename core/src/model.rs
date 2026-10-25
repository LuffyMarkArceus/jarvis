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

use crate::cancel::Cancellation;
use crate::error::{JarvisError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use tokio::sync::mpsc::UnboundedSender;

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

/// One newline-delimited event from Ollama's streaming `/api/chat`.
#[derive(Debug, Deserialize)]
struct ChatStreamResponse {
    #[serde(default)]
    message: Option<ChatMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

/// Accumulated state while reading a streamed chat response.
#[derive(Debug, Default)]
struct StreamState {
    content: String,
    tool_calls: Vec<ModelToolCall>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    done: bool,
}

impl StreamState {
    /// Apply one decoded NDJSON line, forwarding new text to `sink`.
    fn apply(&mut self, line: &str, sink: Option<&UnboundedSender<String>>) -> Result<()> {
        let chunk: ChatStreamResponse = serde_json::from_str(line)
            .map_err(|e| JarvisError::Model(format!("Parse error: {e}")))?;

        if let Some(error) = chunk.error {
            return Err(JarvisError::Model(error));
        }

        if let Some(message) = chunk.message {
            if !message.content.is_empty() {
                self.content.push_str(&message.content);
                if let Some(sink) = sink {
                    // A closed receiver only means the client went away; the
                    // turn itself can still finish and be recorded.
                    let _ = sink.send(message.content);
                }
            }
            if let Some(calls) = message.tool_calls {
                self.tool_calls.extend(calls);
            }
        }

        if let Some(count) = chunk.prompt_eval_count {
            self.prompt_tokens = Some(count);
        }
        if let Some(count) = chunk.eval_count {
            self.completion_tokens = Some(count);
        }
        self.done |= chunk.done;
        Ok(())
    }

    fn into_turn(self) -> ChatTurn {
        ChatTurn {
            message: ChatMessage {
                role: "assistant".to_string(),
                content: self.content,
                tool_calls: if self.tool_calls.is_empty() {
                    None
                } else {
                    Some(self.tool_calls)
                },
                tool_name: None,
            },
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
        }
    }
}

/// Build the wire request shared by the buffered and streaming paths.
fn build_request(
    model: &str,
    system: Option<&str>,
    messages: &[ChatMessage],
    tools: Option<&[Value]>,
    options: Option<Value>,
    stream: bool,
) -> ChatRequest {
    let mut all_messages: Vec<ChatMessage> = Vec::new();
    if let Some(system) = system {
        let system = system.trim();
        if !system.is_empty() {
            all_messages.push(ChatMessage::text("system", system));
        }
    }
    all_messages.extend(messages.iter().cloned());

    ChatRequest {
        model: model.to_string(),
        messages: all_messages,
        stream,
        tools: tools.map(|tools| tools.to_vec()),
        options,
    }
}

/// Await `future`, abandoning it when `cancel` fires.
pub(crate) async fn cancellable<T>(
    future: impl Future<Output = T>,
    cancel: Option<&Cancellation>,
) -> Result<T> {
    match cancel {
        None => Ok(future.await),
        Some(cancel) => {
            let mut cancel = cancel.clone();
            tokio::select! {
                _ = cancel.cancelled() => Err(JarvisError::Cancelled),
                value = future => Ok(value),
            }
        }
    }
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
        self.chat_turn_cancellable(model, system, messages, tools, options, None)
            .await
    }

    /// As [`Self::chat_turn_with_options`], but stops if `cancel` fires.
    pub async fn chat_turn_cancellable(
        &self,
        model: &str,
        system: Option<&str>,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        options: Option<Value>,
        cancel: Option<&Cancellation>,
    ) -> Result<ChatTurn> {
        let req = build_request(model, system, messages, tools, options, false);

        let resp = cancellable(
            self.client
                .post(format!("{}/api/chat", self.base_url))
                .json(&req)
                .send(),
            cancel,
        )
        .await??;

        if !resp.status().is_success() {
            return Err(JarvisError::Model(format!("HTTP {}", resp.status())));
        }

        let chat_resp: ChatResponse = cancellable(resp.json(), cancel)
            .await?
            .map_err(|e| JarvisError::Model(format!("Parse error: {}", e)))?;

        Ok(ChatTurn {
            message: chat_resp.message,
            prompt_tokens: chat_resp.prompt_eval_count,
            completion_tokens: chat_resp.eval_count,
        })
    }

    /// Stream one model turn, forwarding text deltas to `sink`.
    ///
    /// The complete message is still returned at the end: the caller needs
    /// it for tool-call handling and token accounting. Cancellation stops the
    /// HTTP stream at the next chunk boundary.
    #[allow(clippy::too_many_arguments)]
    pub async fn chat_turn_streaming(
        &self,
        model: &str,
        system: Option<&str>,
        messages: &[ChatMessage],
        tools: Option<&[Value]>,
        options: Option<Value>,
        cancel: Option<&Cancellation>,
        sink: &UnboundedSender<String>,
    ) -> Result<ChatTurn> {
        let req = build_request(model, system, messages, tools, options, true);

        let mut response = cancellable(
            self.client
                .post(format!("{}/api/chat", self.base_url))
                .json(&req)
                .send(),
            cancel,
        )
        .await??;

        if !response.status().is_success() {
            return Err(JarvisError::Model(format!("HTTP {}", response.status())));
        }

        let mut state = StreamState::default();
        // Bytes are buffered until a newline: a chunk boundary can split a
        // UTF-8 character as well as a JSON line.
        let mut buffer: Vec<u8> = Vec::new();

        while !state.done {
            let next = cancellable(response.chunk(), cancel).await??;
            let Some(bytes) = next else { break };
            buffer.extend_from_slice(&bytes);

            while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=position).collect();
                let line = std::str::from_utf8(&line[..line.len().saturating_sub(1)])
                    .map_err(|e| JarvisError::Model(format!("Invalid UTF-8 from provider: {e}")))?;
                if !line.trim().is_empty() {
                    state.apply(line, Some(sink))?;
                }
            }
        }

        if !buffer.is_empty() {
            let line = std::str::from_utf8(&buffer)
                .map_err(|e| JarvisError::Model(format!("Invalid UTF-8 from provider: {e}")))?;
            if !line.trim().is_empty() {
                state.apply(line, Some(sink))?;
            }
        }

        if !state.done {
            // A closed stream means the answer may be cut off in the middle.
            // Returning it would present a truncated reply as complete.
            return Err(JarvisError::Model(
                "the model stream ended before the turn finished".to_string(),
            ));
        }

        Ok(state.into_turn())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> (
        UnboundedSender<String>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        tokio::sync::mpsc::unbounded_channel()
    }

    #[test]
    fn request_prepends_a_non_empty_system_message() {
        let messages = vec![ChatMessage::text("user", "hi")];
        let request = build_request(
            "llama3.2:1b",
            Some("  be brief  "),
            &messages,
            None,
            None,
            true,
        );

        assert!(request.stream);
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].role, "system");
        assert_eq!(request.messages[0].content, "be brief");
        assert_eq!(request.messages[1].content, "hi");
    }

    #[test]
    fn blank_system_prompt_is_not_sent() {
        let messages = vec![ChatMessage::text("user", "hi")];
        let request = build_request("m", Some("   "), &messages, None, None, false);
        assert_eq!(request.messages.len(), 1);
    }

    #[test]
    fn streaming_content_is_accumulated_and_forwarded() {
        let (tx, mut rx) = sink();
        let mut state = StreamState::default();

        state
            .apply(
                r#"{"message":{"role":"assistant","content":"Hel"}}"#,
                Some(&tx),
            )
            .unwrap();
        state
            .apply(
                r#"{"message":{"role":"assistant","content":"lo"},"done":false}"#,
                Some(&tx),
            )
            .unwrap();
        state
            .apply(
                r#"{"message":{"role":"assistant","content":""},"done":true,"prompt_eval_count":12,"eval_count":2}"#,
                Some(&tx),
            )
            .unwrap();
        drop(tx);

        assert!(state.done);
        assert_eq!(state.content, "Hello");
        assert_eq!(rx.try_recv().unwrap(), "Hel");
        assert_eq!(rx.try_recv().unwrap(), "lo");

        let turn = state.into_turn();
        assert_eq!(turn.message.content, "Hello");
        assert_eq!(turn.message.role, "assistant");
        assert_eq!(turn.prompt_tokens, Some(12));
        assert_eq!(turn.completion_tokens, Some(2));
        assert!(turn.message.tool_calls.is_none());
    }

    #[test]
    fn streaming_tool_calls_are_collected() {
        let mut state = StreamState::default();
        state
            .apply(
                r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"shell.run","arguments":{"command":"ls"}}}]},"done":false}"#,
                None,
            )
            .unwrap();
        state.apply(r#"{"done":true}"#, None).unwrap();

        let turn = state.into_turn();
        let calls = turn.message.tool_calls.expect("tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "shell.run");
        assert_eq!(calls[0].function.arguments["command"], "ls");
    }

    #[test]
    fn an_error_line_is_returned_as_a_model_error() {
        let mut state = StreamState::default();
        let error = state
            .apply(r#"{"error":"model exploded"}"#, None)
            .unwrap_err();
        assert!(matches!(error, JarvisError::Model(message) if message == "model exploded"));
    }

    #[test]
    fn malformed_json_is_a_model_error() {
        let mut state = StreamState::default();
        let error = state.apply("not json", None).unwrap_err();
        assert!(matches!(error, JarvisError::Model(_)));
    }

    #[tokio::test]
    async fn cancellation_wins_over_a_pending_future() {
        let registry = crate::cancel::TurnRegistry::default();
        let id = uuid::Uuid::new_v4();
        let (cancel, _guard) = registry.begin(id);
        registry.cancel(id);

        let result = cancellable(std::future::pending::<()>(), Some(&cancel)).await;
        assert!(matches!(result, Err(JarvisError::Cancelled)));
    }
}
