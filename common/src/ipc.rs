//! IPC protocol definitions for daemon <-> client communication
//!
//! The wire format is one JSON message per line over a Unix socket. A plain
//! request/response exchange is one message each way, but a single request
//! may cause the daemon to ask the client a question *before* it answers.
//! That is what [`ConfirmationRequest`] is for: a tool whose policy is
//! "ask" cannot be decided by the daemon alone, so the daemon sends a
//! prompt and waits for a [`ConfirmationReply`] carrying the same id.
//!
//! Clients therefore read in a loop: handle any confirmation prompts and
//! streaming [`Chunk`]s that arrive, and stop at the [`Response`] whose id
//! matches the request they sent.
//!
//! Two messages exist only for a turn that is still running:
//!
//! * [`Chunk`] carries assistant text as the model produces it. It is
//!   optional: a client may show it, and the final [`Response`] always
//!   carries the complete answer.
//! * [`Cancel`] asks the daemon to stop a request. It is deliberately not a
//!   [`Request`], so it produces no response of its own and can be sent
//!   while the original request's connection is busy streaming.

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
    /// Answers besides "no", when there is more than one way to say yes.
    /// The first is the plain yes (key `y`); `n` is always no. Empty: a
    /// yes/no prompt. Older clients ignore this and answer yes/no, which
    /// reads as the first choice or none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
    /// What would change: a diff of the file, or the command and where it
    /// runs (6.7b). Built by the daemon, which can read the file now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Preview>,
}

/// The change an approval would allow, shown in its card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    /// `diff` or `command`.
    pub kind: String,
    pub text: String,
}

/// One way to approve a prompt, chosen with a single key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub key: char,
    pub label: String,
}

/// The client's answer to a [`ConfirmationRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationReply {
    /// The id of the [`ConfirmationRequest`] being answered.
    pub id: Uuid,
    pub approved: bool,
    /// The key of the chosen [`Choice`], when the prompt offered choices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<char>,
    /// Why not, with a no: it reaches the model (6.7b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ConfirmationReply {
    /// The choice this reply selects from `choices`: an explicit key, or for
    /// a plain yes (an older client) the first choice. `None` is no.
    pub fn selected(&self, choices: &[Choice]) -> Option<char> {
        if !self.approved {
            return None;
        }
        match self.choice {
            Some(key) if choices.iter().any(|c| c.key == key) => Some(key),
            Some(_) => None,
            None => choices.first().map(|c| c.key),
        }
    }
}

/// Incremental assistant text for a request that is still running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// The id of the [`Request`] that produced this text.
    pub id: Uuid,
    /// The new text. Chunks are ordered; concatenating every chunk for an
    /// id yields the text streamed so far.
    pub text: String,
}

/// What a running request is doing now ("asking nemotron (step 3)",
/// "searching the web"), so a client can tell a long turn from a stuck one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    /// The id of the [`Request`] this describes.
    pub id: Uuid,
    pub text: String,
}

/// One step of a turn, as it starts (`running`) and as it ends (6.7b).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The id of the `ask` [`Request`] it belongs to.
    pub id: Uuid,
    /// Counts from 1 within the turn; the start and end share it.
    pub step: u32,
    /// `model` or `tool`.
    pub kind: String,
    pub name: String,
    pub summary: String,
    /// `running`, `done`, `failed`, `declined`, `denied`, `deferred`,
    /// `withheld` or `untrusted`.
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub ms: Option<u64>,
}

/// A [`Step`] before the server adds its request id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepEvent {
    pub step: u32,
    pub kind: &'static str,
    pub name: String,
    pub summary: String,
    pub state: &'static str,
    pub detail: Option<String>,
    pub ms: Option<u64>,
}

/// A file a turn changed, for its row and the diff view (6.7b).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// The undo entries that changed it this turn, newest first.
    pub undo_ids: Vec<String>,
    pub path: String,
    pub tool: String,
    pub added: usize,
    pub removed: usize,
    /// A unified diff; `None` when no copy was kept to diff against.
    #[serde(default)]
    pub diff: Option<String>,
}

/// Ask the daemon to stop a running request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cancel {
    /// The id of the [`Request`] to stop.
    pub id: Uuid,
}

/// Any message that can cross the socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Request(Request),
    Response(Response),
    ConfirmationRequest(ConfirmationRequest),
    ConfirmationReply(ConfirmationReply),
    Chunk(Chunk),
    Cancel(Cancel),
    Progress(Progress),
    Step(Step),
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

    #[test]
    fn step_round_trips() {
        let step = Message::Step(Step {
            id: Uuid::new_v4(),
            step: 2,
            kind: "tool".into(),
            name: "filesystem.list_dir".into(),
            summary: "looking through files".into(),
            state: "done".into(),
            detail: Some("34 entries".into()),
            ms: Some(12),
        });
        let line = step.encode();
        assert!(line.contains("\"type\":\"step\""), "{line}");
        match Message::decode(&line).unwrap() {
            Message::Step(back) => {
                assert_eq!(back.step, 2);
                assert_eq!(back.detail.as_deref(), Some("34 entries"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn confirmation_without_preview_or_reason_still_parses() {
        let id = Uuid::new_v4();
        let old = format!(
            r#"{{"type":"confirmation_request","id":"{id}","tool":"shell.run","args":{{}},"prompt":"run it?"}}"#
        );
        match Message::decode(&old).unwrap() {
            Message::ConfirmationRequest(r) => assert!(r.preview.is_none()),
            other => panic!("{other:?}"),
        }
        let reply = format!(r#"{{"type":"confirmation_reply","id":"{id}","approved":false}}"#);
        match Message::decode(&reply).unwrap() {
            Message::ConfirmationReply(r) => assert!(r.reason.is_none()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn file_change_round_trips() {
        let change = FileChange {
            undo_ids: vec!["b".into(), "a".into()],
            path: "/w/notes.md".into(),
            tool: "filesystem.edit".into(),
            added: 2,
            removed: 1,
            diff: None,
        };
        let back: FileChange =
            serde_json::from_value(serde_json::to_value(&change).unwrap()).unwrap();
        assert_eq!(back.undo_ids, vec!["b", "a"]);
        assert!(back.diff.is_none());
    }

    #[test]
    fn a_reply_selects_a_choice_and_an_old_client_still_works() {
        let choices = vec![
            Choice {
                key: 'y',
                label: "send once".into(),
            },
            Choice {
                key: 'a',
                label: "always".into(),
            },
        ];
        let reply = |approved, choice| ConfirmationReply {
            reason: None,
            id: Uuid::nil(),
            approved,
            choice,
        };
        assert_eq!(reply(true, Some('a')).selected(&choices), Some('a'));
        // A client that knows only yes/no.
        assert_eq!(reply(true, None).selected(&choices), Some('y'));
        assert_eq!(reply(false, Some('a')).selected(&choices), None);
        // A key that was not offered approves nothing.
        assert_eq!(reply(true, Some('z')).selected(&choices), None);
        // The old wire format still decodes.
        let old: ConfirmationReply = serde_json::from_str(
            r#"{"id":"00000000-0000-0000-0000-000000000000","approved":true}"#,
        )
        .unwrap();
        assert_eq!(old.choice, None);
    }

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
            preview: None,
            id: Uuid::new_v4(),
            tool: "shell.run".to_string(),
            args: json!({"command": "firefox"}),
            prompt: "Run shell command: firefox".to_string(),
            choices: Vec::new(),
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
    fn chunk_roundtrips() {
        let chunk = Chunk {
            id: Uuid::new_v4(),
            text: "Hello".to_string(),
        };
        let line = Message::Chunk(chunk.clone()).encode();
        match Message::decode(&line).expect("should decode") {
            Message::Chunk(decoded) => {
                assert_eq!(decoded.id, chunk.id);
                assert_eq!(decoded.text, "Hello");
            }
            other => panic!("expected a chunk, got {:?}", other),
        }
    }

    #[test]
    fn cancel_roundtrips() {
        let cancel = Cancel { id: Uuid::new_v4() };
        let line = Message::Cancel(cancel.clone()).encode();
        match Message::decode(&line).expect("should decode") {
            Message::Cancel(decoded) => assert_eq!(decoded.id, cancel.id),
            other => panic!("expected a cancel, got {:?}", other),
        }
    }

    #[test]
    fn chunk_wire_tag_is_stable() {
        let line = Message::Chunk(Chunk {
            id: Uuid::nil(),
            text: "x".to_string(),
        })
        .encode();
        assert!(
            line.contains(r#""type":"chunk""#),
            "wire tag changed: {line}"
        );
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(Message::decode("not json").is_err());
    }
}
