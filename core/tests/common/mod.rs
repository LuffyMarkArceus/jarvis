//! Shared by integration tests: a model that plays a script.

use jarvis_core::cancel::Cancellation;
use jarvis_core::error::Result;
use jarvis_core::model::{ChatMessage, ChatTurn, ModelFunction, ModelToolCall};
use jarvis_core::provider::{ChatRequest, Provider};
use serde_json::Value;
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

/// A model that plays a script: one reply per request, the last repeated.
pub struct Script {
    replies: Vec<ChatTurn>,
    next: Mutex<usize>,
}

impl Script {
    pub fn new(replies: Vec<ChatTurn>) -> Self {
        Script {
            replies,
            next: Mutex::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Script {
    fn name(&self) -> &'static str {
        "script"
    }

    async fn chat_turn(
        &self,
        _request: ChatRequest<'_>,
        _cancel: Option<&Cancellation>,
    ) -> Result<ChatTurn> {
        let mut next = self.next.lock().unwrap();
        let turn = self.replies[(*next).min(self.replies.len() - 1)].clone();
        *next += 1;
        Ok(turn)
    }

    async fn chat_turn_streaming(
        &self,
        request: ChatRequest<'_>,
        cancel: Option<&Cancellation>,
        _sink: &UnboundedSender<String>,
    ) -> Result<ChatTurn> {
        self.chat_turn(request, cancel).await
    }
}

pub fn call(name: &str, args: Value) -> ChatTurn {
    ChatTurn {
        message: ChatMessage::assistant_with_tool_calls(
            "",
            vec![ModelToolCall {
                id: None,
                function: ModelFunction {
                    index: None,
                    name: name.into(),
                    arguments: args,
                },
            }],
        ),
        prompt_tokens: None,
        completion_tokens: None,
    }
}

pub fn answer(text: &str) -> ChatTurn {
    ChatTurn {
        message: ChatMessage::text("assistant", text),
        prompt_tokens: None,
        completion_tokens: None,
    }
}
