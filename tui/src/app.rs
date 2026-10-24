//! TUI state and the decisions the UI makes.
//!
//! Everything here is pure: key presses and daemon events turn into
//! [`Action`]s, and `main` performs them against the socket. Keeping the
//! interesting logic out of the drawing code means it can be tested without
//! a terminal.

use crate::client::ClientEvent;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jarvis_common::ipc::{ConfirmationRequest, Response};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: Role,
    pub text: String,
}

impl Entry {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            role: Role::Error,
            text: text.into(),
        }
    }
}

/// One editable line (or several, with Alt+Enter) of user input.
#[derive(Debug, Default)]
pub struct Input {
    chars: Vec<char>,
    cursor: usize,
}

impl Input {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, ch: char) {
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.insert(ch);
        }
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Take the text out of the box.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }
}

/// A request that has been sent but not answered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    Turn,
    CurrentSession,
    ShowSession(String),
    ListSessions,
    SwitchSession(String),
    NewSession,
    DeleteSession(String),
    Prune,
    Status,
    Tools,
}

/// What `main` has to do after a key press or daemon event.
#[derive(Debug)]
pub enum Action {
    Request {
        method: &'static str,
        params: Value,
        pending: Pending,
    },
    /// Answer a tool confirmation prompt.
    Confirm {
        id: Uuid,
        approved: bool,
    },
    /// Stop a running request without closing the connection.
    Cancel {
        id: Uuid,
    },
    /// Throw away the connection and connect again.
    Reconnect,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    pub message_count: i64,
}

#[derive(Debug)]
pub enum Overlay {
    Help,
    Confirm(ConfirmationRequest),
    Sessions {
        rows: Vec<SessionRow>,
        selected: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    Connecting,
    Ready,
    Offline(String),
}

/// A command typed into the input box. Anything that is not one of these is
/// sent to the model, so a prompt that starts with `/` (a path, say) works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    New,
    Sessions,
    Switch(String),
    Delete(String),
    Prune,
    Status,
    Tools,
    Session,
    Clear,
    Quit,
}

pub fn parse_command(text: &str) -> Option<Command> {
    let mut parts = text.split_whitespace();
    let head = parts.next()?;
    let argument = parts.next().map(str::to_string);
    Some(match head {
        "/help" => Command::Help,
        "/new" => Command::New,
        "/sessions" => Command::Sessions,
        "/switch" => Command::Switch(argument.unwrap_or_default()),
        "/delete" => Command::Delete(argument.unwrap_or_default()),
        "/prune" => Command::Prune,
        "/status" => Command::Status,
        "/tools" => Command::Tools,
        "/session" => Command::Session,
        "/clear" => Command::Clear,
        "/quit" | "/exit" => Command::Quit,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
struct StreamingTurn {
    request_id: Uuid,
    index: usize,
}

pub struct App {
    pub entries: Vec<Entry>,
    pub input: Input,
    /// Lines scrolled up from the newest message. Zero follows the tail.
    pub scroll: usize,
    /// Largest scroll offset for the last drawn frame, used for clamping.
    pub max_scroll: usize,
    pub state: DaemonState,
    pub session_id: Option<String>,
    pub model: String,
    pub overlay: Option<Overlay>,
    pub pending: Option<Pending>,
    pub pending_id: Option<Uuid>,
    /// The assistant entry being built from chunks, if any.
    streaming: Option<StreamingTurn>,
    /// A cancel was sent for the in-flight turn and its answer is pending.
    pub cancelling: bool,
    pub should_quit: bool,
    pub spinner: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
}

impl App {
    pub fn new(model: String) -> Self {
        Self {
            entries: vec![Entry::system(
                "Jarvis TUI. Type a message and press Enter. /help lists commands.",
            )],
            input: Input::default(),
            scroll: 0,
            max_scroll: 0,
            state: DaemonState::Connecting,
            session_id: None,
            model,
            overlay: None,
            pending: None,
            pending_id: None,
            streaming: None,
            cancelling: false,
            should_quit: false,
            spinner: 0,
            history: Vec::new(),
            history_pos: None,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.pending.is_some()
    }

    pub fn daemon_label(&self) -> &'static str {
        match self.state {
            DaemonState::Connecting => "connecting",
            DaemonState::Ready => "connected",
            DaemonState::Offline(_) => "offline",
        }
    }

    pub fn tick(&mut self) {
        self.spinner = (self.spinner + 1) % 10;
    }

    pub fn note(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.scroll = 0;
    }

    /// Called when the socket connects.
    pub fn on_connected(&mut self) -> Vec<Action> {
        self.state = DaemonState::Ready;
        self.pending = None;
        self.pending_id = None;
        self.streaming = None;
        self.cancelling = false;
        vec![Action::Request {
            method: "session.current",
            params: json!({}),
            pending: Pending::CurrentSession,
        }]
    }

    pub fn on_disconnected(&mut self, reason: String) {
        self.state = DaemonState::Offline(reason.clone());
        self.pending = None;
        self.pending_id = None;
        self.streaming = None;
        self.cancelling = false;
        // A confirmation can no longer be answered; the daemon will deny the
        // tool after its own timeout, so do not pretend it was sent.
        if matches!(self.overlay, Some(Overlay::Confirm(_))) {
            self.overlay = None;
        }
        self.note(Entry::error(reason));
    }

    pub fn on_client_event(&mut self, event: ClientEvent) -> Vec<Action> {
        match event {
            ClientEvent::Connected => self.on_connected(),
            ClientEvent::Disconnected(reason) => {
                self.on_disconnected(reason);
                vec![]
            }
            ClientEvent::Confirmation(request) => {
                self.overlay = Some(Overlay::Confirm(request));
                vec![]
            }
            ClientEvent::Chunk { id, text } => self.on_chunk(id, text),
            ClientEvent::Response(response) => self.on_response(response),
        }
    }

    /// Record that a request is in flight.
    pub fn begin_request(&mut self, id: Uuid, pending: Pending) {
        self.pending_id = Some(id);
        self.pending = Some(pending);
        self.streaming = None;
        self.cancelling = false;
    }

    /// The request could not even be sent (no connection).
    pub fn request_failed(&mut self, reason: impl Into<String>) {
        self.pending = None;
        self.pending_id = None;
        self.streaming = None;
        self.cancelling = false;
        self.note(Entry::error(reason));
    }

    pub fn submit(&mut self) -> Vec<Action> {
        let text = self.input.take();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return vec![];
        }
        self.scroll = 0;

        if let Some(command) = parse_command(trimmed) {
            return self.run_command(command);
        }

        if self.pending.is_some() {
            self.input.set(trimmed);
            self.note(Entry::system(
                "Still waiting for the daemon. Press Esc after it answers to retry.",
            ));
            return vec![];
        }

        if self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_string());
        }
        self.history_pos = None;

        self.note(Entry::user(text.clone()));
        vec![Action::Request {
            method: "ask",
            params: json!({ "prompt": text }),
            pending: Pending::Turn,
        }]
    }

    fn run_command(&mut self, command: Command) -> Vec<Action> {
        let needs_daemon = !matches!(
            command,
            Command::Help | Command::Quit | Command::Clear | Command::Session
        );
        if needs_daemon && self.pending.is_some() {
            self.note(Entry::system("Still waiting for the daemon."));
            return vec![];
        }

        match command {
            Command::Help => {
                self.overlay = Some(Overlay::Help);
                vec![]
            }
            Command::Quit => {
                self.should_quit = true;
                vec![Action::Quit]
            }
            Command::Clear => {
                self.entries.clear();
                self.scroll = 0;
                vec![]
            }
            Command::Session => {
                let id = self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "none yet".to_string());
                self.note(Entry::system(format!("Session: {id}")));
                vec![]
            }
            Command::New => vec![Action::Request {
                method: "session.new",
                params: json!({}),
                pending: Pending::NewSession,
            }],
            Command::Sessions => vec![Action::Request {
                method: "session.list",
                params: json!({ "limit": 50 }),
                pending: Pending::ListSessions,
            }],
            Command::Switch(id) => {
                if id.is_empty() {
                    self.note(Entry::system("Usage: /switch <session-id>"));
                    return vec![];
                }
                self.switch_request(&id)
            }
            Command::Delete(id) => {
                if id.is_empty() {
                    self.note(Entry::system("Usage: /delete <session-id>"));
                    return vec![];
                }
                vec![Action::Request {
                    method: "session.delete",
                    params: json!({ "session_id": id }),
                    pending: Pending::DeleteSession(id),
                }]
            }
            Command::Prune => vec![Action::Request {
                method: "session.prune",
                params: json!({}),
                pending: Pending::Prune,
            }],
            Command::Status => vec![Action::Request {
                method: "status",
                params: json!({}),
                pending: Pending::Status,
            }],
            Command::Tools => vec![Action::Request {
                method: "tools.list",
                params: json!({}),
                pending: Pending::Tools,
            }],
        }
    }

    fn switch_request(&mut self, id: &str) -> Vec<Action> {
        vec![Action::Request {
            method: "session.switch",
            params: json!({ "session_id": id }),
            pending: Pending::SwitchSession(id.to_string()),
        }]
    }

    fn show_request(&self, id: &str) -> Action {
        Action::Request {
            method: "session.show",
            params: json!({ "session_id": id }),
            pending: Pending::ShowSession(id.to_string()),
        }
    }

    pub fn on_response(&mut self, response: Response) -> Vec<Action> {
        if self.pending_id != Some(response.id) {
            // An answer to a request that was abandoned, e.g. after a
            // reconnect. Showing it would attach it to the wrong turn.
            return vec![];
        }
        self.pending_id = None;
        self.cancelling = false;
        let pending = self.pending.take();
        let streaming_id = self.streaming.as_ref().map(|s| s.request_id);
        let streaming_index = self.streaming.take().map(|s| s.index);

        if !response.ok {
            let reason = response
                .error
                .unwrap_or_else(|| "the daemon did not say why".to_string());
            self.note(Entry::error(reason));
            return vec![];
        }
        let result = response.result.unwrap_or(Value::Null);

        match pending {
            Some(Pending::Turn) => {
                self.scroll = 0;
                let cancelled = result
                    .get("cancelled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let text = result
                    .get("response")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if cancelled {
                    if let Some(index) = streaming_index {
                        if index < self.entries.len() {
                            // Keep the partial answer the user already saw.
                            self.entries[index].role = Role::Assistant;
                        }
                    }
                    if !text.trim().is_empty()
                        && (streaming_id != Some(response.id) || streaming_index.is_none())
                    {
                        self.note(Entry::assistant(text));
                    }
                    self.note(Entry::system("Cancelled."));
                    return vec![];
                }
                if text.trim().is_empty() {
                    self.note(Entry::error("The daemon returned an empty answer."));
                } else if let Some(index) = streaming_index {
                    if index < self.entries.len() {
                        self.entries[index].role = Role::Assistant;
                        self.entries[index].text = text;
                    } else {
                        self.note(Entry::assistant(text));
                    }
                } else {
                    self.note(Entry::assistant(text));
                }
                vec![]
            }
            Some(Pending::CurrentSession) => {
                let Some(id) = result.get("session_id").and_then(Value::as_str) else {
                    self.note(Entry::error("The daemon did not report a session."));
                    return vec![];
                };
                self.session_id = Some(id.to_string());
                vec![self.show_request(id)]
            }
            Some(Pending::ShowSession(id)) => {
                self.session_id = Some(id);
                self.entries = messages_to_entries(&result);
                self.scroll = 0;
                vec![]
            }
            Some(Pending::ListSessions) => {
                self.overlay = Some(Overlay::Sessions {
                    rows: session_rows(&result),
                    selected: 0,
                });
                vec![]
            }
            Some(Pending::SwitchSession(id)) => {
                self.session_id = Some(id.clone());
                self.entries.clear();
                self.scroll = 0;
                vec![self.show_request(&id)]
            }
            Some(Pending::NewSession) => {
                let id = result
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.session_id = Some(id.clone());
                self.entries.clear();
                self.note(Entry::system(format!("Started session {id}.")));
                vec![]
            }
            Some(Pending::DeleteSession(id)) => {
                self.note(Entry::system(format!("Deleted session {id}.")));
                self.overlay = None;
                vec![Action::Request {
                    method: "session.current",
                    params: json!({}),
                    pending: Pending::CurrentSession,
                }]
            }
            Some(Pending::Prune) => {
                let pruned = result
                    .get("pruned")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                self.note(Entry::system(format!("Pruned {pruned} empty session(s).")));
                vec![]
            }
            Some(Pending::Status) => {
                let status = result
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let session = result
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or("none");
                let tools = result
                    .get("tools")
                    .and_then(Value::as_array)
                    .map(|tools| tools.len())
                    .unwrap_or(0);
                self.note(Entry::system(format!(
                    "Daemon {status} · session {session} · {tools} tools"
                )));
                vec![]
            }
            Some(Pending::Tools) => {
                let tools = result
                    .get("tools")
                    .and_then(Value::as_array)
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                self.note(Entry::system(format!("Tools: {tools}")));
                vec![]
            }
            None => vec![],
        }
    }

    fn on_chunk(&mut self, id: Uuid, text: String) -> Vec<Action> {
        if self.pending_id != Some(id) {
            return vec![];
        }
        if !matches!(self.pending, Some(Pending::Turn)) {
            return vec![];
        }
        if text.is_empty() {
            return vec![];
        }
        if let Some(streaming) = self.streaming.as_mut() {
            if streaming.request_id == id && streaming.index < self.entries.len() {
                self.entries[streaming.index].text.push_str(&text);
                // Leave scroll alone: if the user scrolled up to read, new
                // chunks must not yank them back to the bottom.
                return vec![];
            }
        }
        let index = self.entries.len();
        self.entries.push(Entry::assistant(text));
        self.streaming = Some(StreamingTurn {
            request_id: id,
            index,
        });
        vec![]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl && key.code == KeyCode::Char('c') {
            // A confirmation belongs to the running turn. Cancelling should
            // abort the turn itself; denying only the tool would let the
            // model continue explaining the denial, which is not what Ctrl+C
            // means here.
            if let Some(Overlay::Confirm(request)) = self.overlay.take() {
                if matches!(self.pending, Some(Pending::Turn)) {
                    if let Some(id) = self.pending_id {
                        if !self.cancelling {
                            self.cancelling = true;
                            return vec![Action::Cancel { id }];
                        }
                        return vec![];
                    }
                }
                return vec![Action::Confirm {
                    id: request.id,
                    approved: false,
                }];
            }
            if matches!(self.pending, Some(Pending::Turn)) {
                if let Some(id) = self.pending_id {
                    if !self.cancelling {
                        self.cancelling = true;
                        return vec![Action::Cancel { id }];
                    }
                    // Already asked. Pressing Ctrl+C again must not queue a
                    // second cancel; Ctrl+Q still quits immediately.
                    return vec![];
                }
            }
            self.should_quit = true;
            return vec![Action::Quit];
        }

        if ctrl && key.code == KeyCode::Char('q') {
            self.should_quit = true;
            return vec![Action::Quit];
        }

        if self.overlay.is_some() {
            return self.handle_overlay_key(key);
        }

        if key.code == KeyCode::F(1) || (ctrl && key.code == KeyCode::Char('h')) {
            self.overlay = Some(Overlay::Help);
            return vec![];
        }
        if ctrl && key.code == KeyCode::Char('n') {
            return self.run_command(Command::New);
        }
        if ctrl && key.code == KeyCode::Char('s') {
            return self.run_command(Command::Sessions);
        }
        if ctrl && key.code == KeyCode::Char('l') {
            return self.run_command(Command::Clear);
        }
        if ctrl && key.code == KeyCode::Char('u') {
            self.input.clear();
            return vec![];
        }
        if ctrl && key.code == KeyCode::Char('r') {
            return vec![Action::Reconnect];
        }

        match key.code {
            KeyCode::Enter if alt => self.input.newline(),
            KeyCode::Enter => return self.submit(),
            KeyCode::Char(' ') if ctrl => {}
            KeyCode::Char(c) if !ctrl && !alt => self.input.insert(c),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up if self.input.is_empty() || self.history_pos.is_some() => {
                self.history_previous()
            }
            KeyCode::Down if self.input.is_empty() || self.history_pos.is_some() => {
                self.history_next()
            }
            KeyCode::Up => self.scroll_up(1),
            KeyCode::Down => self.scroll_down(1),
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll_down(10),
            _ => {}
        }
        vec![]
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let Some(overlay) = self.overlay.take() else {
            return vec![];
        };
        match overlay {
            Overlay::Help => vec![],
            Overlay::Confirm(request) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => vec![Action::Confirm {
                    id: request.id,
                    approved: true,
                }],
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => vec![Action::Confirm {
                    id: request.id,
                    approved: false,
                }],
                _ => {
                    self.overlay = Some(Overlay::Confirm(request));
                    vec![]
                }
            },
            Overlay::Sessions { rows, mut selected } => {
                let last = rows.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        self.overlay = Some(Overlay::Sessions { rows, selected });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(last);
                        self.overlay = Some(Overlay::Sessions { rows, selected });
                    }
                    KeyCode::Enter => {
                        if let Some(row) = rows.get(selected) {
                            let id = row.id.clone();
                            return self.switch_request(&id);
                        }
                    }
                    _ => {
                        self.overlay = Some(Overlay::Sessions { rows, selected });
                    }
                }
                vec![]
            }
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        if self.overlay.is_some() {
            return;
        }
        self.input.insert_str(&text.replace("\r\n", "\n"));
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_pos {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_pos = Some(next);
        let text = self.history[next].clone();
        self.input.set(&text);
    }

    fn history_next(&mut self) {
        match self.history_pos {
            None => {}
            Some(index) if index + 1 < self.history.len() => {
                self.history_pos = Some(index + 1);
                let text = self.history[index + 1].clone();
                self.input.set(&text);
            }
            Some(_) => {
                self.history_pos = None;
                self.input.clear();
            }
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = (self.scroll + lines).min(self.max_scroll);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }
}

fn messages_to_entries(result: &Value) -> Vec<Entry> {
    result
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| {
                    let text = message.get("content")?.as_str()?.to_string();
                    let entry = match message.get("role").and_then(Value::as_str) {
                        Some("user") => Entry::user(text),
                        Some("assistant") => Entry::assistant(text),
                        Some(role) => Entry::system(format!("{role}: {text}")),
                        None => Entry::system(text),
                    };
                    Some(entry)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn session_rows(result: &Value) -> Vec<SessionRow> {
    result
        .get("sessions")
        .and_then(Value::as_array)
        .map(|sessions| {
            sessions
                .iter()
                .filter_map(|session| {
                    Some(SessionRow {
                        id: session.get("id")?.as_str()?.to_string(),
                        title: session
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("New Session")
                            .to_string(),
                        message_count: session
                            .get("message_count")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_common::ipc::Request;

    fn press(app: &mut App, code: KeyCode) -> Vec<Action> {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn typing_and_editing_handles_unicode() {
        let mut input = Input::default();
        input.insert_str("héllo");
        input.left();
        input.left();
        input.insert('!');
        assert_eq!(input.text(), "hél!lo");
        input.backspace();
        assert_eq!(input.text(), "héllo");
        input.home();
        input.delete();
        assert_eq!(input.text(), "éllo");
        input.end();
        input.delete();
        assert_eq!(input.text(), "éllo");
    }

    #[test]
    fn known_slash_words_are_commands() {
        assert_eq!(parse_command("/help"), Some(Command::Help));
        assert_eq!(
            parse_command("/switch abc"),
            Some(Command::Switch("abc".into()))
        );
        assert_eq!(
            parse_command("/switch"),
            Some(Command::Switch(String::new()))
        );
    }

    #[test]
    fn a_prompt_that_merely_starts_with_a_slash_is_sent_to_the_model() {
        assert_eq!(parse_command("/etc/hosts is broken"), None);
        assert_eq!(parse_command("/unknown command"), None);
    }

    #[test]
    fn enter_sends_the_prompt_and_adds_it_to_history() {
        let mut app = App::new("test".into());
        app.input.set("hello");
        let actions = press(&mut app, KeyCode::Enter);
        match actions.as_slice() {
            [Action::Request {
                method,
                params,
                pending,
            }] => {
                assert_eq!(*method, "ask");
                assert_eq!(params["prompt"], "hello");
                assert_eq!(*pending, Pending::Turn);
            }
            other => panic!("unexpected actions: {other:?}"),
        }
        assert!(app.input.is_empty());
        // The user turn is shown immediately, before the answer arrives.
        assert_eq!(app.entries.last(), Some(&Entry::user("hello")));
    }

    #[test]
    fn alt_enter_inserts_a_newline() {
        let mut app = App::new("test".into());
        app.input.set("one");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        app.input.insert_str("two");
        assert_eq!(app.input.text(), "one\ntwo");
        assert!(app.entries.iter().all(|entry| entry.role != Role::User));
    }

    #[test]
    fn a_turn_response_becomes_an_assistant_entry() {
        let mut app = App::new("test".into());
        let request = Request::new("ask", json!({"prompt": "hi"}));
        app.begin_request(request.id, Pending::Turn);
        let actions = app.on_response(Response::ok(request.id, json!({"response": "hello there"})));
        assert!(actions.is_empty());
        assert_eq!(app.entries.last(), Some(&Entry::assistant("hello there")));
        assert!(!app.is_busy());
    }

    #[test]
    fn a_response_for_another_request_is_ignored() {
        let mut app = App::new("test".into());
        let request = Request::new("ask", json!({}));
        app.begin_request(request.id, Pending::Turn);
        let actions = app.on_response(Response::ok(Uuid::new_v4(), json!({"response": "x"})));
        assert!(actions.is_empty());
        assert_eq!(
            app.entries.last().map(|entry| entry.role),
            Some(Role::System)
        );
        assert!(app.is_busy(), "the real response is still outstanding");
    }

    #[test]
    fn a_failed_response_is_shown_as_an_error() {
        let mut app = App::new("test".into());
        let request = Request::new("ask", json!({}));
        app.begin_request(request.id, Pending::Turn);
        app.on_response(Response::err(request.id, "model exploded".into()));
        assert_eq!(app.entries.last(), Some(&Entry::error("model exploded")));
    }

    #[test]
    fn current_session_chains_into_loading_the_transcript() {
        let mut app = App::new("test".into());
        let request = Request::new("session.current", json!({}));
        app.begin_request(request.id, Pending::CurrentSession);
        let actions = app.on_response(Response::ok(request.id, json!({"session_id": "abc"})));
        match actions.as_slice() {
            [Action::Request {
                method, pending, ..
            }] => {
                assert_eq!(*method, "session.show");
                assert_eq!(*pending, Pending::ShowSession("abc".into()));
            }
            other => panic!("unexpected actions: {other:?}"),
        }
        assert_eq!(app.session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn switching_sessions_replaces_the_transcript() {
        let mut app = App::new("test".into());
        app.entries.push(Entry::assistant("old"));
        let request = Request::new("session.switch", json!({}));
        app.begin_request(request.id, Pending::SwitchSession("new".into()));
        let actions = app.on_response(Response::ok(request.id, json!({})));
        assert!(app.entries.is_empty());
        assert!(matches!(
            actions.as_slice(),
            [Action::Request {
                pending: Pending::ShowSession(id),
                ..
            }] if id == "new"
        ));
    }

    #[test]
    fn session_list_is_parsed_into_rows() {
        let result = json!({
            "sessions": [
                {"id": "a", "title": "First", "message_count": 4},
                {"id": "b", "title": "Second", "message_count": 0}
            ]
        });
        let rows = session_rows(&result);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "First");
        assert_eq!(rows[1].message_count, 0);
    }

    #[test]
    fn ctrl_u_clears_the_message_box() {
        let mut app = App::new("test".into());
        app.input.set("half typed");
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());
    }

    #[test]
    fn help_swallows_typing_and_closes_on_the_next_key() {
        let mut app = App::new("test".into());
        app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::Help)));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.overlay.is_none());
        assert!(app.input.is_empty());
    }

    #[test]
    fn confirmation_prompts_are_answered_explicitly() {
        let mut app = App::new("test".into());
        let id = Uuid::new_v4();
        app.overlay = Some(Overlay::Confirm(ConfirmationRequest {
            id,
            tool: "shell.run".into(),
            args: json!({"command": "firefox"}),
            prompt: "Run shell command: firefox".into(),
        }));

        // An unrelated key must not dismiss or approve the request.
        let actions = press(&mut app, KeyCode::Char('x'));
        assert!(actions.is_empty());
        assert!(matches!(app.overlay, Some(Overlay::Confirm(_))));

        let actions = press(&mut app, KeyCode::Char('n'));
        assert!(matches!(
            actions.as_slice(),
            [Action::Confirm { id: answer, approved: false }] if *answer == id
        ));
        assert!(app.overlay.is_none());
    }

    #[test]
    fn disconnect_drops_a_confirmation_that_can_no_longer_be_sent() {
        let mut app = App::new("test".into());
        app.overlay = Some(Overlay::Confirm(ConfirmationRequest {
            id: Uuid::new_v4(),
            tool: "shell.run".into(),
            args: json!({}),
            prompt: "Run?".into(),
        }));
        app.on_disconnected("gone".into());
        assert!(app.overlay.is_none());
        assert!(matches!(app.state, DaemonState::Offline(_)));
    }

    #[test]
    fn history_walks_backwards_and_forwards() {
        let mut app = App::new("test".into());
        for prompt in ["one", "two"] {
            app.input.set(prompt);
            press(&mut app, KeyCode::Enter);
            let id = Uuid::new_v4();
            app.begin_request(id, Pending::Turn);
            app.on_response(Response::ok(id, json!({"response": "ok"})));
        }
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.text(), "two");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input.text(), "one");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input.text(), "two");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input.text(), "");
    }

    #[test]
    fn ctrl_c_during_a_turn_cancels_it_once() {
        let mut app = App::new("test".into());
        let id = Uuid::new_v4();
        app.begin_request(id, Pending::Turn);
        let actions = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.cancelling);
        assert!(matches!(
            actions.as_slice(),
            [Action::Cancel { id: cancel_id }] if *cancel_id == id
        ));
        let actions = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(actions.is_empty());
        assert!(app.cancelling);
    }

    #[test]
    fn ctrl_c_during_confirmation_cancels_the_turn() {
        let mut app = App::new("test".into());
        let turn_id = Uuid::new_v4();
        app.begin_request(turn_id, Pending::Turn);
        let confirm_id = Uuid::new_v4();
        app.overlay = Some(Overlay::Confirm(ConfirmationRequest {
            id: confirm_id,
            tool: "shell.run".into(),
            args: json!({"command": "firefox"}),
            prompt: "Run shell command: firefox".into(),
        }));
        let actions = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(
            actions.as_slice(),
            [Action::Cancel { id: cancel_id }] if *cancel_id == turn_id
        ));
        assert!(app.overlay.is_none());
        assert!(app.cancelling);
    }
}
