//! IPC protocol definitions for daemon <-> client communication
//!
//! The wire format is one JSON message per line over a Unix socket. A plain
//! request/response exchange is one message each way, but a single request
//! may cause the daemon to ask the client a question *before* it answers.
//! That is what [`ConfirmationRequest`] is for: a tool whose policy is
//! "ask" cannot be decided by the daemon alone, so the daemon sends a
//! prompt and waits for a [`ConfirmationReply`] carrying the same id.
//!
//! Clients therefore read in a loop: handle any confirmation prompts that
//! arrive, and stop at the [`Response`] whose id matches the request they
//! sent.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: Uuid,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: Uuid,
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// A tool call the daemon will not run without the user's consent.
///
/// `prompt` is pre-rendered by the daemon so every client shows the same
/// wording, including the exact arguments that would run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationRequest {
    /// Identifies this prompt. The reply must echo it so a stale answer
    /// cannot approve a different action.
    pub id: Uuid,
    pub tool: String,
    pub args: Value,
    pub prompt: String,
}

/// The client's answer to a [`ConfirmationRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationReply {
    /// The id of the [`ConfirmationRequest`] being answered.
    pub id: Uuid,
    pub approved: bool,
}

/// Any message that can cross the socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Request(Request),
    Response(Response),
    ConfirmationRequest(ConfirmationRequest),
    ConfirmationReply(ConfirmationReply),
}

impl Message {
    /// Parse one wire line.
    ///
    /// Tagged messages are the current format. A bare object with a
    /// `method` field is accepted as an untagged [`Request`] so a client
    /// built against the older protocol still works for the commands that
    /// never need confirmation.
    pub fn decode(line: &str) -> Result<Message, serde_json::Error> {
        match serde_json::from_str::<Message>(line) {
            Ok(message) => Ok(message),
            Err(err) => match serde_json::from_str::<Request>(line) {
                Ok(request) => Ok(Message::Request(request)),
                Err(_) => Err(err),
            },
        }
    }

    /// Serialise as one line (without the trailing newline).
    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl Request {
    pub fn new(method: &str, params: Value) -> Self {
        Self {
            id: Uuid::new_v4(),
            method: method.to_string(),
            params,
        }
    }
}

impl Response {
    pub fn ok(id: Uuid, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Uuid, error: String) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_roundtrips_through_message() {
        let request = Request::new("ping", json!({}));
        let line = Message::Request(request.clone()).encode();
        let decoded = Message::decode(&line).expect("should decode");
        match decoded {
            Message::Request(r) => {
                assert_eq!(r.id, request.id);
                assert_eq!(r.method, "ping");
            }
            other => panic!("expected a request, got {:?}", other),
        }
    }

    #[test]
    fn response_roundtrips_through_message() {
        let response = Response::ok(Uuid::new_v4(), json!({"pong": true}));
        let line = Message::Response(response.clone()).encode();
        match Message::decode(&line).expect("should decode") {
            Message::Response(r) => {
                assert_eq!(r.id, response.id);
                assert!(r.ok);
            }
            other => panic!("expected a response, got {:?}", other),
        }
    }

    #[test]
    fn confirmation_roundtrips() {
        let request = ConfirmationRequest {
            id: Uuid::new_v4(),
            tool: "shell.run".to_string(),
            args: json!({"command": "firefox"}),
            prompt: "Run shell command: firefox".to_string(),
        };
        let line = Message::ConfirmationRequest(request.clone()).encode();
        match Message::decode(&line).expect("should decode") {
            Message::ConfirmationRequest(r) => {
                assert_eq!(r.id, request.id);
                assert_eq!(r.tool, "shell.run");
                assert_eq!(r.args["command"], "firefox");
            }
            other => panic!("expected a confirmation request, got {:?}", other),
        }
    }

    #[test]
    fn untagged_request_is_still_accepted() {
        let legacy = json!({
            "id": Uuid::new_v4(),
            "method": "status",
            "params": {},
        })
        .to_string();
        match Message::decode(&legacy).expect("should decode") {
            Message::Request(r) => assert_eq!(r.method, "status"),
            other => panic!("expected a request, got {:?}", other),
        }
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(Message::decode("not json").is_err());
    }
}
