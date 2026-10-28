//! OpenAI-compatible chat completions.
//!
//! This is the wire format served by llama.cpp, vLLM, LM Studio, OpenRouter and
//! OpenAI itself, and by Ollama's own `/v1` endpoint. Keeping one
//! implementation covers all of them; only `base_url`, the key and the model
//! name change.
//!
//! The differences from Ollama that this module absorbs:
//!
//! * messages carry `content` only; a tool result must name the `tool_call_id`
//!   it answers, which Ollama does not require;
//! * tool arguments arrive as a JSON *string*, and while streaming they arrive
//!   in fragments that must be concatenated by call index;
//! * token counts are absent unless the request asks for them;
//! * streaming is server-sent events (`data: {...}`) rather than NDJSON.

use crate::cancel::Cancellation;
use crate::config::{Config, ModelConfig};
use crate::error::{JarvisError, Result};
use crate::model::{ChatMessage, ChatTurn, ModelFunction, ModelToolCall};
use crate::provider::{ChatRequest, Provider};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

/// Read a key from the environment, as named in the configuration.
fn api_key(model: &ModelConfig) -> Result<Option<String>> {
    let Some(variable) = model.api_key_env.as_deref() else {
        return Ok(None);
    };
    match std::env::var(variable) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        _ => Err(JarvisError::Config(format!(
            "provider 'openai' needs an API key in ${variable}, which is unset or empty"
        ))),
    }
}

pub struct OpenAiProvider {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
    /// Pauses before each retry of a busy server. See [`OpenAiProvider::post`].
    retry_delays: Vec<std::time::Duration>,
}

/// A server's own `Retry-After` is honoured up to this, so a misbehaving
/// one cannot stall a turn for minutes.
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

impl OpenAiProvider {
    pub fn new(base_url: &str, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            client: reqwest::Client::new(),
            retry_delays: vec![
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(5),
            ],
        }
    }

    /// POST a request, retrying while the server says it is busy.
    ///
    /// Shared free endpoints answer 429 when their pool is full, and 502 or
    /// 503 when the model behind them is overloaded; both usually clear in
    /// seconds. Measured on OpenRouter's free models: without a retry, 6 to
    /// 20 of 46 eval cases got no answer at all. Other errors are returned
    /// at once - retrying a bad request only repeats it. Nothing has been
    /// streamed before a retry, so the streaming path can use this too.
    async fn post(
        &self,
        url: &str,
        body: &Value,
        cancel: Option<&Cancellation>,
    ) -> Result<reqwest::Response> {
        let mut delays = self.retry_delays.iter();
        loop {
            let response =
                crate::model::cancellable(self.request(url.to_string()).json(body).send(), cancel)
                    .await??;
            let status = response.status().as_u16();
            let busy = matches!(status, 429 | 502 | 503);
            let Some(delay) = delays.next().filter(|_| busy) else {
                return Ok(response);
            };
            let wait = retry_after(&response).map_or(*delay, |s| s.min(MAX_RETRY_AFTER));
            tracing::info!("HTTP {} from {}, retrying in {:?}", status, url, wait);
            crate::model::cancellable(tokio::time::sleep(wait), cancel).await?;
        }
    }

    pub fn from_config(config: &Config) -> Result<Self> {
        Self::from_model(&config.model)
    }

    pub fn from_model(model: &ModelConfig) -> Result<Self> {
        Ok(Self::new(&model.base_url, api_key(model)?))
    }

    fn request(&self, url: String) -> reqwest::RequestBuilder {
        let request = self.client.post(url);
        match &self.api_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }
}

/// Translate neutral chat messages into OpenAI's message objects.
///
/// A tool result in this codebase carries the tool's *name*; OpenAI requires
/// the *call id* it answers. The orchestrator always appends the assistant's
/// tool-call message immediately before its results, so the ids are recovered
/// by looking back at the pending calls and matching on name, in order. A
/// result with no matching call is still sent (with a synthetic id) rather
/// than dropped, because silently removing context is worse than an id the
/// server may ignore.
pub fn to_openai_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // Tool-call ids from the last assistant message that had calls, consumed in
    // order as their results arrive.
    let mut pending: Vec<(String, String)> = Vec::new();

    for message in messages {
        match message.role.as_str() {
            "tool" => {
                let name = message.tool_name.clone().unwrap_or_default();
                let id = match pending.iter().position(|(call_name, _)| *call_name == name) {
                    Some(index) => pending.remove(index).1,
                    None => format!("call_{name}"),
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": message.content,
                }));
            }
            "assistant" => {
                let mut object = json!({
                    "role": "assistant",
                    "content": message.content,
                });
                if let Some(calls) = &message.tool_calls {
                    pending.clear();
                    let translated: Vec<Value> = calls
                        .iter()
                        .enumerate()
                        .map(|(index, call)| {
                            let id = call
                                .id
                                .clone()
                                .unwrap_or_else(|| format!("call_{}_{index}", call.function.name));
                            pending.push((call.function.name.clone(), id.clone()));
                            json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": call.function.name,
                                    "arguments": arguments_string(&call.function.arguments),
                                }
                            })
                        })
                        .collect();
                    object["tool_calls"] = Value::Array(translated);
                }
                out.push(object);
            }
            role => out.push(json!({ "role": role, "content": message.content })),
        }
    }
    out
}

/// Arguments must be a JSON string on the wire.
fn arguments_string(arguments: &Value) -> String {
    match arguments {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Map this project's options onto OpenAI's names.
///
/// `num_ctx` has no equivalent - the server decides the context window - so it
/// is dropped rather than sent as an unknown field, which some servers reject.
pub fn to_openai_options(options: Option<&Value>) -> Value {
    let Some(options) = options else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for key in ["temperature", "top_p", "stop"] {
        if let Some(value) = options.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(out)
}

/// The request body for one turn.
pub fn build_body(request: &ChatRequest<'_>, stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = request.system {
        let system = system.trim();
        if !system.is_empty() {
            messages.push(json!({ "role": "system", "content": system }));
        }
    }
    messages.extend(to_openai_messages(request.messages));

    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": stream,
    });
    let options = to_openai_options(request.options.as_ref());
    if let Value::Object(map) = options {
        for (key, value) in map {
            body[key] = value;
        }
    }
    if let Some(tools) = request.tools {
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }
    }
    if stream {
        // Without this the final chunk carries no counts; servers that do not
        // support it ignore the field.
        body["stream_options"] = json!({ "include_usage": true });
    }
    body
}

/// Read `data:` payloads from a byte stream and reassemble the turn.
#[derive(Debug, Default)]
pub struct OpenAiStream {
    content: String,
    /// Tool calls being assembled, keyed by the index the server assigns.
    calls: Vec<(usize, String, String, String)>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    done: bool,
}

impl OpenAiStream {
    /// Apply one SSE `data:` payload. `[DONE]` ends the stream.
    pub fn apply(&mut self, data: &str, sink: Option<&UnboundedSender<String>>) -> Result<()> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|e| JarvisError::Model(format!("Parse error: {e}")))?;
        if let Some(error) = chunk
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            return Err(JarvisError::Model(error.to_string()));
        }

        if let Some(usage) = chunk.get("usage") {
            if let Some(count) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                self.prompt_tokens = Some(count);
            }
            if let Some(count) = usage.get("completion_tokens").and_then(Value::as_u64) {
                self.completion_tokens = Some(count);
            }
        }

        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return Ok(());
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    self.content.push_str(text);
                    if let Some(sink) = sink {
                        let _ = sink.send(text.to_string());
                    }
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.apply_tool_delta(call);
                }
            }
        }

        if choice
            .get("finish_reason")
            .is_some_and(|reason| !reason.is_null())
        {
            self.done = true;
        }
        Ok(())
    }

    fn apply_tool_delta(&mut self, call: &Value) {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let arguments = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        match self.calls.iter_mut().find(|(slot, _, _, _)| *slot == index) {
            Some((_, existing_id, existing_name, existing_arguments)) => {
                // A later fragment may carry the id or name once; arguments
                // are always fragments to append.
                if !id.is_empty() {
                    *existing_id = id;
                }
                if !name.is_empty() {
                    *existing_name = name;
                }
                existing_arguments.push_str(&arguments);
            }
            None => self.calls.push((index, id, name, arguments)),
        }
    }

    pub fn into_turn(mut self) -> ChatTurn {
        self.calls.sort_by_key(|(index, _, _, _)| *index);
        let tool_calls: Vec<ModelToolCall> = self
            .calls
            .into_iter()
            .filter(|(_, _, name, _)| !name.is_empty())
            .map(|(_, id, name, arguments)| ModelToolCall {
                id: if id.is_empty() { None } else { Some(id) },
                function: ModelFunction {
                    index: None,
                    name,
                    arguments: parse_arguments(&arguments),
                },
            })
            .collect();

        ChatTurn {
            message: ChatMessage {
                role: "assistant".to_string(),
                content: self.content,
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                tool_name: None,
                label: None,
            },
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
        }
    }
}

/// Tool arguments should be an object when possible; raw text is kept as a
/// string so the orchestrator's own normalisation can report it.
fn parse_arguments(text: &str) -> Value {
    if text.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn chat_turn(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&Cancellation>,
    ) -> Result<ChatTurn> {
        let url = format!("{}/chat/completions", self.base_url);
        tracing::debug!("openai request: POST {}", url);
        let body = build_body(&request, false);
        let response = self.post(&url, &body, cancel).await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(JarvisError::Model(format!(
                "HTTP {status}: {}",
                first_line(&text)
            )));
        }
        let json: Value = response
            .json()
            .await
            .map_err(|e| JarvisError::Model(format!("Parse error: {e}")))?;
        parse_completion(&json)
    }

    async fn chat_turn_streaming(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&Cancellation>,
        sink: &UnboundedSender<String>,
    ) -> Result<ChatTurn> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = build_body(&request, true);
        tracing::debug!("openai request: POST {} (stream)", url);
        let mut response = self.post(&url, &body, cancel).await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(JarvisError::Model(format!(
                "HTTP {status}: {}",
                first_line(&text)
            )));
        }

        let mut state = OpenAiStream::default();
        let mut buffer: Vec<u8> = Vec::new();
        while !state.done {
            let next = crate::model::cancellable(response.chunk(), cancel).await??;
            let Some(bytes) = next else { break };
            buffer.extend_from_slice(&bytes);
            drain_complete_lines(&mut buffer, &mut state, sink)?;
        }
        // A server may end the stream without a final newline; the last event
        // still counts. A truncated line is a malformed stream, not a reason
        // to return a half-built turn.
        if !state.done && !buffer.is_empty() {
            drain_line(&mut state, &buffer, sink)?;
        }
        if !state.done {
            return Err(JarvisError::Model(
                "the model stream ended before the turn finished".to_string(),
            ));
        }
        Ok(state.into_turn())
    }
}

/// Apply every complete line in `buffer`, leaving any partial line behind.
fn drain_complete_lines(
    buffer: &mut Vec<u8>,
    state: &mut OpenAiStream,
    sink: &UnboundedSender<String>,
) -> Result<()> {
    while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
        let line: Vec<u8> = buffer.drain(..=position).collect();
        drain_line(state, &line[..line.len().saturating_sub(1)], sink)?;
    }
    Ok(())
}

/// Apply one complete SSE line, ignoring anything that is not data.
fn drain_line(state: &mut OpenAiStream, line: &[u8], sink: &UnboundedSender<String>) -> Result<()> {
    let line = std::str::from_utf8(line)
        .map_err(|e| JarvisError::Model(format!("Invalid UTF-8 from provider: {e}")))?;
    if let Some(data) = sse_data(line)? {
        state.apply(data, Some(sink))?;
    }
    Ok(())
}

/// Extract the payload of one server-sent event line.
///
/// Comment lines (`: keep-alive`) and event names are not data and yield
/// `None`.
fn sse_data(line: &str) -> Result<Option<&str>> {
    let line = line.trim_end_matches('\r');
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(None);
    };
    let data = data.trim();
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(data))
}

/// `Retry-After` in seconds, when the server sent one.
fn retry_after(response: &reqwest::Response) -> Option<std::time::Duration> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(std::time::Duration::from_secs)
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// Parse a non-streamed completion.
pub fn parse_completion(json: &Value) -> Result<ChatTurn> {
    let choice = json
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| JarvisError::Model("provider returned no choices".to_string()))?;
    let message = choice.get("message").unwrap_or(&Value::Null);

    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    let function = call.get("function")?;
                    let name = function.get("name")?.as_str()?.to_string();
                    let arguments = function
                        .get("arguments")
                        .map(|value| match value {
                            Value::String(text) => parse_arguments(text),
                            other => other.clone(),
                        })
                        .unwrap_or_else(|| json!({}));
                    Some(ModelToolCall {
                        id: call.get("id").and_then(Value::as_str).map(str::to_string),
                        function: ModelFunction {
                            index: None,
                            name,
                            arguments,
                        },
                    })
                })
                .collect::<Vec<_>>()
        })
        .filter(|calls| !calls.is_empty());

    let usage = json.get("usage");
    Ok(ChatTurn {
        message: ChatMessage {
            role: "assistant".to_string(),
            content,
            tool_calls,
            tool_name: None,
            label: None,
        },
        prompt_tokens: usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_u64),
        completion_tokens: usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> ChatMessage {
        ChatMessage::text(role, content)
    }

    #[test]
    fn a_system_prompt_becomes_the_first_message() {
        let messages = vec![message("user", "hi")];
        let request = ChatRequest {
            model: "local",
            system: Some("be brief"),
            messages: &messages,
            tools: None,
            options: None,
        };
        let body = build_body(&request, false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be brief");
        assert_eq!(body["stream"], false);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn an_empty_system_prompt_is_not_sent() {
        let messages = vec![message("user", "hi")];
        let request = ChatRequest {
            model: "local",
            system: Some("   "),
            messages: &messages,
            tools: None,
            options: None,
        };
        let body = build_body(&request, false);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_tool_result_names_the_call_it_answers() {
        let call = ModelToolCall {
            id: Some("call_abc".to_string()),
            function: ModelFunction {
                index: None,
                name: "mcp.search.search".to_string(),
                arguments: json!({"query": "rust"}),
            },
        };
        let messages = vec![
            ChatMessage::assistant_with_tool_calls("", vec![call]),
            ChatMessage::tool_result("mcp.search.search", "results"),
        ];
        let translated = to_openai_messages(&messages);
        assert_eq!(translated[1]["role"], "tool");
        assert_eq!(translated[1]["tool_call_id"], "call_abc");
        // Arguments are a string on the wire, not an object.
        assert_eq!(
            translated[0]["tool_calls"][0]["function"]["arguments"],
            "{\"query\":\"rust\"}"
        );
    }

    #[test]
    fn a_tool_result_without_a_recorded_call_still_has_an_id() {
        let messages = vec![ChatMessage::tool_result("filesystem.read", "text")];
        let translated = to_openai_messages(&messages);
        assert_eq!(translated[0]["tool_call_id"], "call_filesystem.read");
    }

    #[test]
    fn a_generated_id_is_used_when_the_model_call_has_none() {
        let call = ModelToolCall {
            id: None,
            function: ModelFunction {
                index: None,
                name: "filesystem.read".to_string(),
                arguments: json!({}),
            },
        };
        let messages = vec![
            ChatMessage::assistant_with_tool_calls("", vec![call]),
            ChatMessage::tool_result("filesystem.read", "text"),
        ];
        let translated = to_openai_messages(&messages);
        let id = translated[0]["tool_calls"][0]["id"].as_str().unwrap();
        assert_eq!(translated[1]["tool_call_id"], id);
    }

    #[test]
    fn ollama_only_options_are_not_sent() {
        let messages = vec![message("user", "hi")];
        let request = ChatRequest {
            model: "local",
            system: None,
            messages: &messages,
            tools: None,
            options: Some(json!({"temperature": 0.2, "num_ctx": 4096})),
        };
        let body = build_body(&request, false);
        assert_eq!(body["temperature"], 0.2);
        assert!(body.get("num_ctx").is_none());
    }

    #[test]
    fn tool_schemas_are_passed_through_unchanged() {
        let messages = vec![message("user", "hi")];
        let tools = vec![json!({"type": "function", "function": {"name": "x"}})];
        let request = ChatRequest {
            model: "local",
            system: None,
            messages: &messages,
            tools: Some(&tools),
            options: None,
        };
        let body = build_body(&request, false);
        assert_eq!(body["tools"][0]["function"]["name"], "x");
    }

    #[test]
    fn streaming_requests_ask_for_usage_counts() {
        let messages = vec![message("user", "hi")];
        let request = ChatRequest {
            model: "local",
            system: None,
            messages: &messages,
            tools: None,
            options: None,
        };
        let body = build_body(&request, true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn streamed_text_is_accumulated_and_forwarded() {
        let (sink, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut state = OpenAiStream::default();
        state
            .apply(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#, Some(&sink))
            .unwrap();
        state
            .apply(r#"{"choices":[{"delta":{"content":"lo"}}]}"#, Some(&sink))
            .unwrap();
        state.apply("[DONE]", Some(&sink)).unwrap();
        assert_eq!(receiver.recv().await.as_deref(), Some("Hel"));
        assert_eq!(receiver.recv().await.as_deref(), Some("lo"));
        let turn = state.into_turn();
        assert_eq!(turn.message.content, "Hello");
        assert!(turn.message.tool_calls.is_none());
    }

    #[test]
    fn streamed_tool_arguments_are_joined_by_index() {
        let mut state = OpenAiStream::default();
        // The name arrives in the first fragment, the arguments in pieces.
        state
            .apply(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"mcp.fetch.fetch","arguments":"{\"url\":"}}]}}]}"#,
                None,
            )
            .unwrap();
        state
            .apply(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"https://example.com\"}"}}]}}]}"#,
                None,
            )
            .unwrap();
        state.apply("[DONE]", None).unwrap();

        let turn = state.into_turn();
        let calls = turn.message.tool_calls.expect("a tool call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(calls[0].function.name, "mcp.fetch.fetch");
        assert_eq!(calls[0].function.arguments["url"], "https://example.com");
    }

    #[test]
    fn a_finish_reason_ends_the_stream_without_done() {
        let mut state = OpenAiStream::default();
        state
            .apply(
                r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
                None,
            )
            .unwrap();
        assert!(state.done);
        assert_eq!(state.into_turn().message.content, "ok");
    }

    #[test]
    fn usage_is_read_from_the_final_chunk() {
        let mut state = OpenAiStream::default();
        state
            .apply(
                r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3}}"#,
                None,
            )
            .unwrap();
        let turn = state.into_turn();
        assert_eq!(turn.prompt_tokens, Some(11));
        assert_eq!(turn.completion_tokens, Some(3));
    }

    #[test]
    fn a_provider_error_is_surfaced_not_swallowed() {
        let mut state = OpenAiStream::default();
        let error = state
            .apply(r#"{"error":{"message":"model not found"}}"#, None)
            .unwrap_err();
        assert!(error.to_string().contains("model not found"));
    }

    #[test]
    fn sse_lines_that_are_not_data_are_ignored() {
        assert_eq!(sse_data(": keep-alive").unwrap(), None);
        assert_eq!(sse_data("event: message").unwrap(), None);
        assert_eq!(sse_data("").unwrap(), None);
        assert_eq!(sse_data("data: {\"a\":1}").unwrap(), Some("{\"a\":1}"));
        assert_eq!(sse_data("data: [DONE]\r").unwrap(), Some("[DONE]"));
    }

    #[test]
    fn a_line_split_across_chunks_is_only_applied_when_complete() {
        let (sink, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut state = OpenAiStream::default();
        let mut buffer = br#"data: {"choices":[{"delta":{"content":"hi"}}]}"#.to_vec();

        drain_complete_lines(&mut buffer, &mut state, &sink).unwrap();
        assert!(state.content.is_empty());
        assert!(!buffer.is_empty(), "the partial line must be kept");

        buffer.push(b'\n');
        drain_complete_lines(&mut buffer, &mut state, &sink).unwrap();
        assert_eq!(state.into_turn().message.content, "hi");
    }

    #[test]
    fn a_buffered_completion_with_a_tool_call_is_parsed() {
        let json = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {
                            "name": "filesystem.read",
                            "arguments": "{\"path\":\"/tmp/x\"}"
                        }
                    }]
                }
            }],
            "usage": {"prompt_tokens": 7, "completion_tokens": 2}
        });
        let turn = parse_completion(&json).unwrap();
        let calls = turn.message.tool_calls.expect("a call");
        assert_eq!(calls[0].function.arguments["path"], "/tmp/x");
        assert_eq!(turn.prompt_tokens, Some(7));
    }

    /// A one-endpoint HTTP server that answers each connection with the
    /// next status in `statuses`, and a completion once they run out.
    async fn stub_server(
        statuses: Vec<u16>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = vec![0u8; 65536];
                let _ = socket.read(&mut buf).await;
                let (status, body) = match statuses.get(n) {
                    Some(code) => (*code, r#"{"error":{"message":"busy"}}"#.to_string()),
                    None => (
                        200,
                        r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#
                            .to_string(),
                    ),
                };
                let reply = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (url, hits)
    }

    fn quick(url: &str) -> OpenAiProvider {
        let mut provider = OpenAiProvider::new(url, None);
        provider.retry_delays = vec![std::time::Duration::from_millis(5); 2];
        provider
    }

    fn hello() -> Vec<ChatMessage> {
        vec![message("user", "hi")]
    }

    fn request(messages: &[ChatMessage]) -> ChatRequest<'_> {
        ChatRequest {
            model: "m",
            system: None,
            messages,
            tools: None,
            options: None,
        }
    }

    #[tokio::test]
    async fn a_busy_server_is_retried_until_it_answers() {
        let (url, hits) = stub_server(vec![429, 503]).await;
        let messages = hello();
        let turn = quick(&url)
            .chat_turn(request(&messages), None)
            .await
            .unwrap();
        assert_eq!(turn.message.content, "ok");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_stop_after_the_last_delay() {
        let (url, hits) = stub_server(vec![429, 429, 429, 429]).await;
        let messages = hello();
        let error = quick(&url)
            .chat_turn(request(&messages), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("429"), "{}", error);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_bad_request_is_not_retried() {
        let (url, hits) = stub_server(vec![400]).await;
        let messages = hello();
        assert!(quick(&url)
            .chat_turn(request(&messages), None)
            .await
            .is_err());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// End-to-end against a real OpenAI-compatible server.
    ///
    /// Ollama serves this format at `/v1`, so the same weights can prove the
    /// translation without a hosted account. Ignored by default because it
    /// needs a running server:
    ///
    /// ```sh
    /// JARVIS_OPENAI_TEST_URL=http://127.0.0.1:11434/v1 \
    ///   cargo test -p jarvis-core --lib -- --ignored openai
    /// ```
    #[tokio::test]
    #[ignore = "needs a running OpenAI-compatible server"]
    async fn a_real_server_answers_and_reports_tokens() {
        let Ok(url) = std::env::var("JARVIS_OPENAI_TEST_URL") else {
            return;
        };
        let model =
            std::env::var("JARVIS_OPENAI_TEST_MODEL").unwrap_or_else(|_| "llama3.2:1b".to_string());
        let provider = OpenAiProvider::new(&url, None);

        let messages = vec![message("user", "Reply with exactly: ok")];
        let request = ChatRequest {
            model: &model,
            system: Some("Answer in one word."),
            messages: &messages,
            tools: None,
            options: Some(json!({"temperature": 0})),
        };
        let turn = provider.chat_turn(request, None).await.expect("a turn");
        assert!(!turn.message.content.trim().is_empty());
        assert!(turn.prompt_tokens.is_some(), "the server reported no usage");

        // The streaming path parses SSE instead of one JSON body; both must
        // produce the same shape of answer.
        let (sink, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let messages = vec![message("user", "Count to three.")];
        let streamed = provider
            .chat_turn_streaming(
                ChatRequest {
                    model: &model,
                    system: Some("Answer in one short line."),
                    messages: &messages,
                    tools: None,
                    options: Some(json!({"temperature": 0})),
                },
                None,
                &sink,
            )
            .await
            .expect("a streamed turn");
        let mut forwarded = String::new();
        while let Ok(part) = receiver.try_recv() {
            forwarded.push_str(&part);
        }
        assert!(!streamed.message.content.trim().is_empty());
        assert_eq!(forwarded, streamed.message.content);
        assert!(streamed.completion_tokens.is_some(), "no streamed usage");
    }

    #[test]
    fn a_response_without_choices_is_an_error() {
        assert!(parse_completion(&json!({})).is_err());
    }
}
