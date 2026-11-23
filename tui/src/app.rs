//! TUI state and the decisions the UI makes.
//!
//! Everything here is pure: key presses and daemon events turn into
//! [`Action`]s, and `main` performs them against the socket. Keeping the
//! interesting logic out of the drawing code means it can be tested without
//! a terminal.

use crate::client::ClientEvent;
pub use crate::input::Input;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jarvis_common::ipc::{ConfirmationRequest, Response};
pub use jarvis_core::config::FollowUp;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
    Error,
    /// A turn's steps: live, then one row (6.7b).
    Work,
}

/// The steps of one turn, and the files it changed (6.7b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkLog {
    /// The `ask` request it belongs to.
    pub request: Uuid,
    pub steps: Vec<jarvis_common::ipc::Step>,
    pub done: bool,
    pub ms: u64,
    pub changes: Vec<jarvis_common::ipc::FileChange>,
    /// Opened by a click.
    pub open: bool,
    /// The turn failed: its reply could not say what it changed.
    pub failed_turn: bool,
}

/// The one row a finished turn's steps fold into.
pub fn work_row(log: &WorkLog, open: bool) -> String {
    let mut row = format!(
        "{} {} steps · {:.1} s",
        if open { "▾" } else { "▸" },
        log.steps.len(),
        log.ms as f64 / 1000.0
    );
    if log.failed_turn {
        row.push_str(" · changes: see /undo");
    } else if !log.changes.is_empty() {
        row.push_str(&format!(" · {} files changed", log.changes.len()));
    }
    let declined = log.steps.iter().filter(|s| s.state == "declined").count();
    if declined > 0 {
        row.push_str(&format!(" · {declined} declined"));
    }
    row
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: Role,
    pub text: String,
    /// Under an answer: what produced it and where it ran.
    pub footer: Option<String>,
    /// A turn's steps (`Role::Work` only).
    pub work: Option<WorkLog>,
}

impl Entry {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            footer: None,
            work: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            footer: None,
            work: None,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            text: text.into(),
            footer: None,
            work: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            role: Role::Error,
            text: text.into(),
            footer: None,
            work: None,
        }
    }
}

/// A request that has been sent but not answered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    Turn,
    /// `session.last`, for `--continue`.
    LastSession,
    ShowSession(String),
    ListSessions,
    NewSession,
    DeleteSession(String),
    Prune,
    Recall,
    Status,
    Tools,
    Tier,
    Trust,
    /// `model.list`, for the picker.
    Models,
    /// `model.set`; then the message to send, if one followed the name.
    Model(Option<String>),
    /// `/bad` or `/good`.
    Feedback,
    /// `mode.set`: `/mode`, `/approval`, `/sandbox`, Shift+Tab.
    Mode,
    /// `/approve`.
    Approve,
}

/// What `main` has to do after a key press or daemon event.
#[derive(Debug)]
pub enum Action {
    Request {
        method: &'static str,
        params: Value,
        pending: Pending,
    },
    /// Answer a tool confirmation prompt; `choice` when it offered several.
    Confirm {
        id: Uuid,
        approved: bool,
        choice: Option<char>,
    },
    /// Stop a running request without closing the connection.
    Cancel {
        id: Uuid,
    },
    /// Throw away the connection and connect again.
    Reconnect,
    /// Show what undoing the latest change (or this id) would do. Read from
    /// the undo folder directly, so it works with the daemon stopped.
    UndoPreview(Option<String>),
    UndoList,
    /// Undo exactly the change that was previewed.
    UndoApply(String),
    /// Send a steer into the running turn, beside it, not as the pending
    /// request (6.7a).
    Steer {
        steer: Queued,
    },
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
    Recall(Vec<RecallRow>),
    Models {
        rows: Vec<ModelRow>,
        selected: usize,
    },
}

/// A model in the `/model` picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRow {
    pub name: String,
    pub remote: bool,
    pub current: bool,
}

#[derive(Debug, Clone)]
pub struct RecallRow {
    pub session_id: String,
    pub role: String,
    pub snippet: String,
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
    Recall(String),
    /// `/remote` (true) or `/local` (false).
    Tier(bool),
    /// `/model` opens the picker; `/model <name>` switches to it.
    Model(String),
    /// Where the last turn's time went.
    Timing,
    /// `/bad [note]` or `/good [note]` about the last turn.
    Feedback(String, String),
    /// `/undo`, `/undo list`, `/undo yes`, `/undo <id>`.
    Undo(String),
    /// `/trust`, `/trust clear`.
    Trust(String),
    /// `/mode`, `/mode <what> <value>`, `/mode autopilot` (6.6).
    Mode(String),
    /// `/approval manual|assisted|allow-all`.
    Approval(String),
    /// `/sandbox on|off`.
    Sandbox(String),
    /// `/approve [all|none|<n>]`: what Autopilot left.
    Approve(String),
    /// `/go [auto]`: carry out the plan above.
    Go(String),
    /// `/resume`: send this conversation's held queue (6.7a).
    Resume,
    /// `/queue`: what is queued in this conversation.
    Queue,
    /// `/steer <text>`: into the running turn.
    Steer(String),
    Clear,
    Quit,
    /// `/word` that is no command: shown as a mistake, not sent to the
    /// model.
    Unknown(String),
}

/// `/timing`: where a turn's time went, in plain words.
pub fn format_timing(t: &Value) -> String {
    let secs = |ms: u64| format!("{:.1} s", ms as f64 / 1000.0);
    let num = |v: &Value| v.as_u64().unwrap_or(0);
    let total = num(&t["total_ms"]);
    let you = num(&t["you_ms"]);
    let mut lines = vec![format!(
        "Last turn {} (Jarvis {}, you {})",
        secs(total),
        secs(total.saturating_sub(you)),
        secs(you)
    )];
    for m in t["model"].as_array().into_iter().flatten() {
        let mut parts = Vec::new();
        if !m["load_ms"].is_null() {
            parts.push(format!("loading {}", secs(num(&m["load_ms"]))));
        }
        if !m["prompt_ms"].is_null() {
            parts.push(format!(
                "reading {} tokens {}",
                num(&m["prompt_tokens"]),
                secs(num(&m["prompt_ms"]))
            ));
        }
        if !m["gen_ms"].is_null() {
            parts.push(format!(
                "writing {} tokens {}",
                num(&m["gen_tokens"]),
                secs(num(&m["gen_ms"]))
            ));
        }
        let detail = if parts.is_empty() {
            String::new()
        } else {
            format!(" - {}", parts.join(", "))
        };
        lines.push(format!(
            "  {}: {}{}",
            m["model"].as_str().unwrap_or("model"),
            secs(num(&m["wall_ms"])),
            detail
        ));
    }
    for tool in t["tools"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  {} {}",
            tool["name"].as_str().unwrap_or("tool"),
            secs(num(&tool["ms"]))
        ));
    }
    lines.join("\n")
}

pub fn parse_command(text: &str) -> Option<Command> {
    let mut parts = text.splitn(2, char::is_whitespace);
    let head = parts.next()?;
    let rest = parts.next().unwrap_or("").trim().to_string();
    Some(match head {
        "/help" => Command::Help,
        "/new" => Command::New,
        "/sessions" => Command::Sessions,
        "/switch" => Command::Switch(rest),
        "/delete" => Command::Delete(rest),
        "/prune" => Command::Prune,
        "/recall" => Command::Recall(rest),
        "/status" => Command::Status,
        "/remote" => Command::Tier(true),
        "/local" => Command::Tier(false),
        "/model" => Command::Model(rest),
        "/timing" => Command::Timing,
        "/bad" => Command::Feedback("bad".into(), rest),
        "/good" => Command::Feedback("good".into(), rest),
        "/undo" => Command::Undo(rest),
        "/trust" => Command::Trust(rest),
        "/mode" => Command::Mode(rest),
        "/approval" => Command::Approval(rest),
        "/sandbox" => Command::Sandbox(rest),
        "/approve" => Command::Approve(rest),
        "/go" => Command::Go(rest),
        "/resume" => Command::Resume,
        "/queue" => Command::Queue,
        "/steer" => Command::Steer(rest),
        "/tools" => Command::Tools,
        "/session" => Command::Session,
        "/clear" => Command::Clear,
        "/quit" | "/exit" => Command::Quit,
        // A path ("/etc/hosts is broken") is a prompt; a lone "/word" is a
        // mistyped command.
        other
            if other.starts_with('/')
                && other.len() > 1
                && other[1..].chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            Command::Unknown(other.to_string())
        }
        _ => return None,
    })
}

/// A message waiting to be sent after the running turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Queued {
    pub id: Uuid,
    pub text: String,
    /// A steer that came back unused (6.7a).
    pub steer: bool,
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
    /// The change `/undo` showed; `/undo yes` undoes this one and no other.
    undo_offer: Option<String>,
    pub pending_id: Option<Uuid>,
    /// The assistant entry being built from chunks, if any.
    streaming: Option<StreamingTurn>,
    /// A cancel was sent for the in-flight turn and its answer is pending.
    pub cancelling: bool,
    pub should_quit: bool,
    pub spinner: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    /// The last answered turn's timing, for `/timing`.
    last_timing: Option<Value>,
    /// Open the conversation used last instead of a new one (`--continue`).
    pub continue_last: bool,
    /// The terminal tells Shift+Enter (and Ctrl+Enter) from Enter.
    pub keyboard_enhanced: bool,
    /// Ctrl+O: every turn's steps open (6.7b).
    pub show_steps: bool,
    /// Where each work row was drawn: (screen row, entry index), for clicks.
    pub work_rows: Vec<(u16, usize)>,
    /// Messages typed while a turn ran, per conversation (6.7a).
    pub queues: std::collections::HashMap<String, std::collections::VecDeque<Queued>>,
    /// Conversations whose queue waits for `/resume`.
    pub held: std::collections::HashSet<String>,
    /// What Enter does while a turn runs (`[tui] follow_up`).
    pub follow_up: FollowUp,
    /// Steers sent into the running turn, not yet known used.
    pub in_flight: Vec<Queued>,
    /// Steer requests waiting for their reply: request id -> steer id.
    steer_requests: std::collections::HashMap<Uuid, Uuid>,
    /// The `/` menu's selected row, and whether Esc closed it.
    pub menu_selected: usize,
    menu_closed: bool,
    /// This window's conversation's mode, for the header (6.6).
    pub mode_label: String,
    /// Its autonomy, for Shift+Tab: `plan`, `interactive` or `autopilot`.
    autonomy: String,
    /// When the pending request was sent.
    busy_since: Option<Instant>,
    /// What the pending turn says it is doing now.
    progress: Option<String>,
    /// Lines for `tui.log` about decisions the app made on its own.
    diag: Vec<String>,
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
            undo_offer: None,
            pending_id: None,
            streaming: None,
            cancelling: false,
            should_quit: false,
            spinner: 0,
            history: Vec::new(),
            history_pos: None,
            last_timing: None,
            continue_last: false,
            keyboard_enhanced: false,
            show_steps: false,
            work_rows: Vec::new(),
            queues: Default::default(),
            held: Default::default(),
            follow_up: FollowUp::Queue,
            in_flight: Vec::new(),
            steer_requests: Default::default(),
            menu_selected: 0,
            menu_closed: false,
            mode_label: "Interactive · Manual · Sandbox on".into(),
            autonomy: "interactive".into(),
            busy_since: None,
            progress: None,
            diag: Vec::new(),
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
        if let Some(since) = self.busy_since {
            self.check_stuck(since.elapsed());
        }
    }

    /// Lines for `tui.log`, once each.
    pub fn take_diag(&mut self) -> Vec<String> {
        std::mem::take(&mut self.diag)
    }

    /// How long the pending request has been running.
    pub fn busy_for(&self) -> Duration {
        self.busy_since.map(|s| s.elapsed()).unwrap_or_default()
    }

    /// The header text while busy: what the turn is doing and for how long,
    /// so a long turn does not look like a stuck one. Empty for the first
    /// few seconds.
    pub fn busy_label(&self, elapsed: Duration) -> String {
        let secs = elapsed.as_secs();
        if self.progress.is_none() && secs < 3 {
            return String::new();
        }
        let mut label = format!(
            "{} · {} s",
            self.progress.as_deref().unwrap_or("working"),
            secs
        );
        if matches!(self.pending, Some(Pending::Turn)) && secs >= 20 {
            label.push_str(" · Ctrl+C cancels");
        }
        label
    }

    /// Give up on a request that is not a turn and has had no answer for 30
    /// seconds, so the window is never stuck saying "still waiting". A turn
    /// can rightly take minutes; only the owner cancels it.
    pub fn check_stuck(&mut self, elapsed: Duration) {
        if self.pending.is_none()
            || matches!(self.pending, Some(Pending::Turn))
            || elapsed <= Duration::from_secs(30)
        {
            return;
        }
        self.diag.push(format!(
            "gave up on {:?} after {} s",
            self.pending,
            elapsed.as_secs()
        ));
        self.pending = None;
        self.pending_id = None;
        self.busy_since = None;
        self.note(Entry::error(
            "The daemon did not answer that request in 30 s, so the window is free again. If it keeps happening, Ctrl+R reconnects.",
        ));
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
        // A window keeps its conversation across reconnects; a new one
        // starts its own, or with --continue the one used last.
        if let Some(id) = self.session_id.clone() {
            return vec![self.show_request(&id)];
        }
        if self.continue_last {
            return vec![Action::Request {
                method: "session.last",
                params: json!({}),
                pending: Pending::LastSession,
            }];
        }
        vec![Self::new_session_request()]
    }

    fn new_session_request() -> Action {
        Action::Request {
            method: "session.new",
            params: json!({}),
            pending: Pending::NewSession,
        }
    }

    /// `params` for `method`, with this window's conversation added when
    /// the request is about one. `None` when the window has no conversation
    /// yet: the request waits rather than going out without one.
    pub fn scoped(&self, method: &str, mut params: Value) -> Option<Value> {
        const CONVERSATION: &[&str] = &[
            "ask",
            "model.list",
            "model.set",
            "tier.set",
            "feedback",
            "trust.list",
            "trust.clear",
            "mode.set",
            "approve",
            "steer",
        ];
        if !CONVERSATION.contains(&method) {
            return Some(params);
        }
        let session = self.session_id.as_ref()?;
        params["session"] = json!(session);
        Some(params)
    }

    pub fn on_disconnected(&mut self, reason: String) {
        self.abandon_turn();
        self.state = DaemonState::Offline(reason.clone());
        self.pending = None;
        self.pending_id = None;
        self.busy_since = None;
        self.progress = None;
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
            ClientEvent::Step(step) => {
                self.on_step(step);
                vec![]
            }
            ClientEvent::Progress { id, text } => {
                if self.pending_id == Some(id) {
                    self.progress = Some(text);
                }
                vec![]
            }
            ClientEvent::Response(response) => self.on_response(response),
        }
    }

    /// Record that a request is in flight.
    /// After `/remote`, `/local` or `/model`: the header follows the model
    /// now answering, and the switch is noted.
    fn on_response_tier(&mut self, result: &Value) -> Vec<Action> {
        if let Some(model) = result.get("model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        let field = |k: &str| {
            result
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string()
        };
        self.note(Entry::system(format!(
            "Now answering with {} ({}). {}",
            field("model"),
            field("tier"),
            field("privacy")
        )));
        vec![]
    }

    /// Switch to the model `name` names, then send `then` if given.
    fn model_set(name: &str, then: Option<String>) -> Action {
        Action::Request {
            method: "model.set",
            params: json!({ "model": name }),
            pending: Pending::Model(then),
        }
    }

    pub fn begin_request(&mut self, id: Uuid, pending: Pending) {
        if matches!(pending, Pending::Turn) {
            self.entries.push(Entry {
                role: Role::Work,
                text: String::new(),
                footer: None,
                work: Some(WorkLog {
                    request: id,
                    steps: Vec::new(),
                    done: false,
                    ms: 0,
                    changes: Vec::new(),
                    open: false,
                    failed_turn: false,
                }),
            });
        }
        self.pending_id = Some(id);
        self.pending = Some(pending);
        self.busy_since = Some(Instant::now());
        self.progress = None;
        self.streaming = None;
        self.cancelling = false;
    }

    /// The request could not even be sent (no connection).
    pub fn request_failed(&mut self, reason: impl Into<String>) {
        self.pending = None;
        self.pending_id = None;
        self.busy_since = None;
        self.progress = None;
        self.streaming = None;
        self.cancelling = false;
        self.note(Entry::error(reason));
    }

    pub fn submit(&mut self) -> Vec<Action> {
        self.submit_with(false)
    }

    /// Enter (`flip` false) or Ctrl+Enter (`flip` true): while a turn runs,
    /// the one queues and the other steers, by `[tui] follow_up`.
    fn submit_with(&mut self, flip: bool) -> Vec<Action> {
        let text = self.input.take();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return vec![];
        }
        self.scroll = 0;

        if let Some(command) = parse_command(trimmed) {
            return self.run_command(command);
        }

        // Typed while a turn runs: it waits its turn, or joins it (6.7a).
        if matches!(self.pending, Some(Pending::Turn)) {
            if (self.follow_up == FollowUp::Steer) != flip {
                return self.start_steer(trimmed.to_string());
            }
            if let Some(session) = self.session_id.clone() {
                self.queues.entry(session).or_default().push_back(Queued {
                    id: Uuid::new_v4(),
                    text: trimmed.to_string(),
                    steer: false,
                });
                return vec![];
            }
        }
        if self.pending.is_some() {
            self.input.set(trimmed);
            self.note(Entry::system(
                "Still waiting for the daemon. Press Esc after it answers to retry.",
            ));
            return vec![];
        }

        if self.session_id.is_none() {
            self.input.set(trimmed);
            self.note(Entry::system(
                "This window has no conversation yet. Type /new to start one.",
            ));
            return vec![];
        }

        self.send_prompt(text.clone())
    }

    /// Send `text` as this window's next question.
    fn send_prompt(&mut self, text: String) -> Vec<Action> {
        let trimmed = text.trim();
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

    /// `mode.set` for one part of the mode, or an error line.
    fn mode_request(&mut self, what: &str, value: &str) -> Vec<Action> {
        match jarvis_common::mode_change(what, value) {
            Ok(params) => vec![Action::Request {
                method: "mode.set",
                params,
                pending: Pending::Mode,
            }],
            Err(e) => {
                self.note(Entry::error(e));
                vec![]
            }
        }
    }

    /// Take the mode a reply carries (`session.new`, `session.show`,
    /// `mode.set`).
    fn take_mode(&mut self, result: &Value) {
        if let Some(label) = result.get("mode_label").and_then(Value::as_str) {
            self.mode_label = label.to_string();
        }
        if let Some(autonomy) = result["mode"]["autonomy"].as_str() {
            self.autonomy = autonomy.to_string();
        }
    }

    fn run_command(&mut self, command: Command) -> Vec<Action> {
        let needs_daemon = !matches!(
            command,
            Command::Help
                | Command::Quit
                | Command::Clear
                | Command::Session
                | Command::Queue
                | Command::Resume
                | Command::Steer(_)
                | Command::Timing
                | Command::Unknown(_)
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
            Command::Mode(rest) if rest.is_empty() => {
                self.note(Entry::system(format!(
                    "Mode: {}. Change it with /mode plan|interactive|autopilot, /approval manual|assisted|allow-all, /sandbox on|off; Shift+Tab cycles plan, interactive, autopilot.",
                    self.mode_label
                )));
                vec![]
            }
            Command::Mode(rest) => {
                let mut words = rest.splitn(2, char::is_whitespace);
                let what = words.next().unwrap_or("");
                let value = words.next().unwrap_or("").trim();
                self.mode_request(what, value)
            }
            Command::Approval(value) => self.mode_request("approval", &value),
            Command::Sandbox(value) => self.mode_request("sandbox", &value),
            Command::Approve(which) => {
                let which = if which.is_empty() {
                    "list".to_string()
                } else {
                    which
                };
                vec![Action::Request {
                    method: "approve",
                    params: json!({ "which": which }),
                    pending: Pending::Approve,
                }]
            }
            Command::Go(rest) => {
                let autonomy = if rest.trim() == "auto" {
                    "autopilot"
                } else {
                    "interactive"
                };
                self.note(Entry::user(format!("/go {rest}").trim_end().to_string()));
                vec![Action::Request {
                    method: "ask",
                    params: json!({
                        "prompt": "Carry out the plan above, step by step.",
                        "autonomy": autonomy,
                    }),
                    pending: Pending::Turn,
                }]
            }
            Command::Timing => {
                let text = match &self.last_timing {
                    Some(timing) => format_timing(timing),
                    None => "No turn has finished in this window yet.".to_string(),
                };
                self.note(Entry::system(text));
                vec![]
            }
            Command::Feedback(kind, note) => vec![Action::Request {
                method: "feedback",
                params: json!({ "kind": kind, "note": note }),
                pending: Pending::Feedback,
            }],
            Command::Unknown(name) => {
                self.note(Entry::error(format!(
                    "Unknown command {name}. /help lists the commands; nothing was sent."
                )));
                vec![]
            }
            Command::Clear => {
                self.entries.clear();
                self.scroll = 0;
                vec![]
            }
            Command::Steer(text) if text.trim().is_empty() => {
                self.note(Entry::error("/steer what? /steer <message>"));
                vec![]
            }
            Command::Steer(text) => {
                if matches!(self.pending, Some(Pending::Turn)) {
                    self.start_steer(text)
                } else if self.pending.is_some() {
                    self.note(Entry::system("Still waiting for the daemon."));
                    vec![]
                } else {
                    self.send_prompt(text)
                }
            }
            Command::Resume => {
                let Some(session) = self.session_id.clone() else {
                    return vec![];
                };
                self.held.remove(&session);
                if self.pending.is_some() {
                    return vec![];
                }
                self.send_next_queued()
            }
            Command::Queue => {
                let text = match self.queue().filter(|q| !q.is_empty()) {
                    None => "Nothing is queued.".to_string(),
                    Some(q) => {
                        let mut out = format!("Queued ({}):", q.len());
                        for (i, m) in q.iter().enumerate() {
                            out.push_str(&format!("\n{}. {}", i + 1, m.text));
                        }
                        out
                    }
                };
                self.note(Entry::system(text));
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
            Command::Recall(query) => {
                if query.is_empty() {
                    self.note(Entry::system("Usage: /recall <query>"));
                    return vec![];
                }
                vec![Action::Request {
                    method: "memory.recall",
                    params: json!({ "query": query, "limit": 10 }),
                    pending: Pending::Recall,
                }]
            }
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
            Command::Model(name) if name.is_empty() => vec![Action::Request {
                method: "model.list",
                params: json!({}),
                pending: Pending::Models,
            }],
            Command::Model(rest) => {
                // "/model qwen", "/model qwen what is 2+2?", "/model qwen: ...":
                // the first word is the model, the rest a message.
                let rest = rest.trim();
                let (name, then) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                let name = name.trim_end_matches([':', ',']);
                let then = then.trim().trim_start_matches([':', ',']).trim();
                let then = (!then.is_empty()).then(|| then.to_string());
                vec![Self::model_set(name, then)]
            }
            Command::Tier(remote) => vec![Action::Request {
                method: "tier.set",
                params: json!({ "tier": if remote { "remote" } else { "local" } }),
                pending: Pending::Tier,
            }],
            Command::Undo(arg) => match arg.as_str() {
                "" => vec![Action::UndoPreview(None)],
                "list" => vec![Action::UndoList],
                "yes" => match self.undo_offer.take() {
                    Some(id) => vec![Action::UndoApply(id)],
                    None => {
                        self.note(Entry::system(
                            "Nothing to confirm: type /undo first to see what would be undone.",
                        ));
                        vec![]
                    }
                },
                id => vec![Action::UndoPreview(Some(id.to_string()))],
            },
            Command::Trust(arg) => vec![Action::Request {
                method: if arg == "clear" {
                    "trust.clear"
                } else {
                    "trust.list"
                },
                params: json!({}),
                pending: Pending::Trust,
            }],
        }
    }

    /// `/undo` found `id` to undo: show what it would do and wait for yes.
    pub fn offer_undo(&mut self, id: String, question: String) {
        self.undo_offer = Some(id);
        self.note(Entry::system(format!(
            "{}\nType /undo yes to do it.",
            question
        )));
    }

    /// Switching is the window's choice: it loads that conversation and
    /// sends its id from then on. Nothing changes for other windows.
    fn switch_request(&mut self, id: &str) -> Vec<Action> {
        vec![self.show_request(id)]
    }

    fn show_request(&self, id: &str) -> Action {
        Action::Request {
            method: "session.show",
            params: json!({ "session_id": id }),
            pending: Pending::ShowSession(id.to_string()),
        }
    }

    pub fn on_response(&mut self, response: Response) -> Vec<Action> {
        // A steer's reply: it joined the turn, or it waits like a message.
        if let Some(steer_id) = self.steer_requests.remove(&response.id) {
            let took = response.ok
                && response.result.as_ref().and_then(|r| r["status"].as_str()) == Some("steering");
            if !took {
                if let Some(pos) = self.in_flight.iter().position(|q| q.id == steer_id) {
                    let mut steer = self.in_flight.remove(pos);
                    steer.steer = false;
                    if let Some(session) = self.session_id.clone() {
                        self.queues.entry(session).or_default().push_back(steer);
                    }
                    // Too late for a turn that has ended: nothing else will
                    // send it, so it goes now.
                    if self.pending.is_none() {
                        return self.send_next_queued();
                    }
                }
            }
            return vec![];
        }
        let turn_ended =
            self.pending_id == Some(response.id) && matches!(self.pending, Some(Pending::Turn));
        if turn_ended {
            let ids = |key: &str| -> Option<Vec<String>> {
                response
                    .result
                    .as_ref()
                    .and_then(|r| r[key].as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().or_else(|| v["id"].as_str()))
                            .map(String::from)
                            .collect()
                    })
            };
            // Back to the front of the queue: what the turn did not use -
            // all of it when it failed and nobody can say.
            // A steer whose own reply has not come yet is not this turn's to
            // judge: it waits for that reply (6.7a review).
            let outstanding: std::collections::HashSet<Uuid> =
                self.steer_requests.values().copied().collect();
            let (waiting, settled): (Vec<Queued>, Vec<Queued>) = self
                .in_flight
                .drain(..)
                .partition(|q| outstanding.contains(&q.id));
            self.in_flight = waiting;
            let reported = ids("unused_steers").is_some() || ids("used_steers").is_some();
            let unused = ids("unused_steers").unwrap_or_default();
            let used = ids("used_steers").unwrap_or_default();
            let mut back = Vec::new();
            for steer in settled {
                let id = steer.id.to_string();
                // Used: done. Unused, or a failed turn nobody can speak for:
                // back to the front. Accepted but unreported: back, to be safe.
                if !reported || !used.contains(&id) || unused.contains(&id) {
                    back.push(steer);
                }
            }
            self.requeue_front(back);
        }
        let answered = response.ok
            && !response
                .result
                .as_ref()
                .and_then(|r| r.get("cancelled"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if turn_ended {
            let result = response.result.as_ref();
            let ms = result
                .and_then(|r| r["timing"]["total_ms"].as_u64())
                .unwrap_or(0);
            let changes: Vec<jarvis_common::ipc::FileChange> = result
                .and_then(|r| serde_json::from_value(r["changes"].clone()).ok())
                .unwrap_or_default();
            let failed = !response.ok;
            if let Some(log) = self.work_log_mut(response.id) {
                log.done = true;
                log.ms = ms;
                log.changes = changes;
                log.failed_turn = failed;
            }
        }
        let mut actions = self.on_response_inner(response);
        if turn_ended {
            if answered {
                actions.extend(self.send_next_queued());
            } else {
                self.hold_queue();
            }
        }
        actions
    }

    /// The connection is going (Ctrl+R): the running turn's steers go back
    /// to the queue, and the queue is held, as for a drop.
    pub fn reconnecting(&mut self) {
        self.abandon_turn();
    }

    /// A turn whose answer will not come: every steer sent into it goes
    /// back to the front of the queue, and the queue waits for `/resume`.
    fn abandon_turn(&mut self) {
        if let (Some(Pending::Turn), Some(id)) = (&self.pending, self.pending_id) {
            if let Some(log) = self.work_log_mut(id) {
                log.done = true;
                log.failed_turn = true;
            }
        }
        if matches!(self.pending, Some(Pending::Turn)) || !self.in_flight.is_empty() {
            let back: Vec<Queued> = self.in_flight.drain(..).collect();
            self.requeue_front(back);
            self.hold_queue();
        }
        self.steer_requests.clear();
    }

    /// A steer `main` could not send: it waits in the queue instead.
    pub fn steer_unsent(&mut self, mut steer: Queued) {
        steer.steer = false;
        if let Some(session) = self.session_id.clone() {
            self.queues.entry(session).or_default().push_back(steer);
        }
        self.hold_queue();
    }

    /// Steers that came back, to the front of this conversation's queue,
    /// in the order they were sent.
    fn requeue_front(&mut self, steers: Vec<Queued>) {
        let Some(session) = self.session_id.clone() else {
            return;
        };
        let queue = self.queues.entry(session).or_default();
        for steer in steers.into_iter().rev() {
            queue.push_front(steer);
        }
    }

    /// Steer `text` into the running turn.
    fn start_steer(&mut self, text: String) -> Vec<Action> {
        let steer = Queued {
            id: Uuid::new_v4(),
            text: text.clone(),
            steer: true,
        };
        self.note(Entry::system(format!("steering: {text}")));
        vec![Action::Steer { steer }]
    }

    /// `main` sent `steer` as request `request`.
    pub fn steer_sent(&mut self, request: Uuid, steer: Queued) {
        self.steer_requests.insert(request, steer.id);
        self.in_flight.push(steer);
    }

    fn work_log_mut(&mut self, request: Uuid) -> Option<&mut WorkLog> {
        self.entries
            .iter_mut()
            .rev()
            .filter_map(|e| e.work.as_mut())
            .find(|w| w.request == request)
    }

    /// A step started or ended: the end keeps what the start said.
    fn on_step(&mut self, step: jarvis_common::ipc::Step) {
        let Some(log) = self.work_log_mut(step.id) else {
            return;
        };
        match log.steps.iter_mut().find(|s| s.step == step.step) {
            Some(known) => {
                known.state = step.state;
                known.detail = step.detail;
                known.ms = step.ms;
            }
            None => log.steps.push(step),
        }
    }

    /// A click at screen row `row`: a work row there opens or closes.
    pub fn on_click(&mut self, row: u16) {
        let Some(&(_, index)) = self.work_rows.iter().find(|(r, _)| *r == row) else {
            return;
        };
        if let Some(log) = self.entries.get_mut(index).and_then(|e| e.work.as_mut()) {
            log.open = !log.open;
        }
    }

    /// This conversation's queue, if it has one.
    pub fn queue(&self) -> Option<&std::collections::VecDeque<Queued>> {
        self.session_id.as_ref().and_then(|s| self.queues.get(s))
    }

    /// Send the front of this conversation's queue, unless it is held.
    fn send_next_queued(&mut self) -> Vec<Action> {
        let Some(session) = self.session_id.clone() else {
            return vec![];
        };
        if self.held.contains(&session) {
            return vec![];
        }
        match self.queues.get_mut(&session).and_then(|q| q.pop_front()) {
            Some(next) => self.send_prompt(next.text),
            None => vec![],
        }
    }

    /// A turn that did not answer holds what was queued after it.
    fn hold_queue(&mut self) {
        let Some(session) = self.session_id.clone() else {
            return;
        };
        if self.queues.get(&session).is_some_and(|q| !q.is_empty()) {
            self.held.insert(session);
            self.note(Entry::system(
                "Queue held: /resume sends it, Alt+Up edits the last one.",
            ));
        }
    }

    fn on_response_inner(&mut self, response: Response) -> Vec<Action> {
        if self.pending_id != Some(response.id) {
            // An answer to a request that was abandoned, e.g. after a
            // reconnect. Showing it would attach it to the wrong turn.
            return vec![];
        }
        self.pending_id = None;
        self.cancelling = false;
        self.busy_since = None;
        self.progress = None;
        let pending = self.pending.take();
        let streaming_id = self.streaming.as_ref().map(|s| s.request_id);
        let streaming_index = self.streaming.take().map(|s| s.index);

        if !response.ok {
            let reason = response
                .error
                .unwrap_or_else(|| "the daemon did not say why".to_string());
            self.note(Entry::error(reason));
            match pending {
                // The message that was to follow a failed /model is not lost.
                Some(Pending::Model(Some(message))) => self.input.set(&message),
                // The window could not reopen or continue its conversation:
                // it starts a new one rather than being left with none.
                Some(Pending::LastSession | Pending::ShowSession(_))
                    if self.session_id.is_none() =>
                {
                    return vec![Self::new_session_request()];
                }
                _ => {}
            }
            return vec![];
        }
        let result = response.result.unwrap_or(Value::Null);

        if matches!(pending, Some(Pending::Turn)) {
            if let Some(timing) = result.get("timing").filter(|t| !t.is_null()) {
                self.last_timing = Some(timing.clone());
            }
        }
        match pending {
            Some(Pending::Feedback) => {
                let kind = result["kind"].as_str().unwrap_or("feedback");
                self.note(Entry::system(format!(
                    "Noted ({kind}) for the last turn. `jarvis report` lists it."
                )));
                vec![]
            }
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
                let footer = result
                    .get("footer")
                    .and_then(Value::as_str)
                    .map(String::from);
                if text.trim().is_empty() {
                    self.note(Entry::error("The daemon returned an empty answer."));
                } else if let Some(index) = streaming_index.filter(|i| *i < self.entries.len()) {
                    self.entries[index].role = Role::Assistant;
                    self.entries[index].text = text;
                    self.entries[index].footer = footer;
                } else {
                    let mut entry = Entry::assistant(text);
                    entry.footer = footer;
                    self.note(entry);
                }
                vec![]
            }
            Some(Pending::LastSession) => match result.get("session_id").and_then(Value::as_str) {
                Some(id) => vec![self.show_request(id)],
                None => vec![Self::new_session_request()],
            },
            Some(Pending::ShowSession(id)) => {
                self.take_mode(&result);
                self.session_id = Some(id);
                if let Some(model) = result.get("model").and_then(Value::as_str) {
                    self.model = model.to_string();
                }
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
            Some(Pending::NewSession) => {
                self.take_mode(&result);
                let id = result
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if id.is_empty() {
                    self.note(Entry::error(
                        "The daemon did not start a conversation. Type /new to try again.",
                    ));
                    return vec![];
                }
                self.session_id = Some(id.clone());
                if let Some(model) = result.get("model").and_then(Value::as_str) {
                    self.model = model.to_string();
                }
                self.entries.clear();
                self.note(Entry::system(format!("Started session {id}.")));
                vec![]
            }
            Some(Pending::DeleteSession(id)) => {
                self.note(Entry::system(format!("Deleted session {id}.")));
                self.overlay = None;
                // This window's own: it needs a new one. Another's: carry on.
                if self.session_id.as_deref() == Some(id.as_str()) {
                    self.session_id = None;
                    vec![Self::new_session_request()]
                } else {
                    vec![]
                }
            }
            Some(Pending::Prune) => {
                let pruned = result
                    .get("pruned")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                self.note(Entry::system(format!("Pruned {pruned} empty session(s).")));
                vec![]
            }
            Some(Pending::Recall) => {
                let rows: Vec<RecallRow> = result
                    .get("results")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|r| {
                                Some(RecallRow {
                                    session_id: r
                                        .get("session_id")
                                        .and_then(Value::as_str)?
                                        .to_string(),
                                    role: r.get("role").and_then(Value::as_str)?.to_string(),
                                    snippet: r.get("snippet").and_then(Value::as_str)?.to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if rows.is_empty() {
                    self.note(Entry::system("No memories found."));
                } else {
                    self.overlay = Some(Overlay::Recall(rows));
                }
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
            Some(Pending::Trust) => {
                let names: Vec<&str> = result["trusted"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                self.note(Entry::system(if names.is_empty() {
                    "Nothing is trusted in this conversation: actions after reading pages or files outside your allowed folders ask first.".to_string()
                } else {
                    format!(
                        "Trusted in this conversation (/trust clear to forget):\n  {}",
                        names.join("\n  ")
                    )
                }));
                vec![]
            }
            Some(Pending::Models) => {
                let rows: Vec<ModelRow> = result["models"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| ModelRow {
                        name: m["name"].as_str().unwrap_or_default().to_string(),
                        remote: m["remote"].as_bool().unwrap_or(false),
                        current: m["current"].as_bool().unwrap_or(false),
                    })
                    .collect();
                let selected = rows.iter().position(|r| r.current).unwrap_or(0);
                self.overlay = Some(Overlay::Models { rows, selected });
                vec![]
            }
            Some(Pending::Model(then)) => {
                let mut actions = self.on_response_tier(&result);
                if let Some(message) = then {
                    self.history.push(message.clone());
                    self.note(Entry::user(message.clone()));
                    actions.push(Action::Request {
                        method: "ask",
                        params: json!({ "prompt": message }),
                        pending: Pending::Turn,
                    });
                }
                actions
            }
            Some(Pending::Tier) => self.on_response_tier(&result),
            Some(Pending::Mode) => {
                self.take_mode(&result);
                let mut text = format!("Mode: {}.", self.mode_label);
                if let Some(note) = result.get("note").and_then(Value::as_str) {
                    text.push_str(&format!(" ({note})"));
                }
                self.note(Entry::system(text));
                vec![]
            }
            Some(Pending::Approve) => {
                let text = result.get("text").and_then(Value::as_str).unwrap_or("");
                self.note(Entry::system(text.trim_end().to_string()));
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
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

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
                    choice: None,
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

        // Help is F1 only: Ctrl+H is how most terminals send Ctrl+Backspace.
        if key.code == KeyCode::F(1) {
            self.overlay = Some(Overlay::Help);
            return vec![];
        }
        if ctrl && key.code == KeyCode::Char('n') {
            return self.run_command(Command::New);
        }
        if ctrl && key.code == KeyCode::Char('s') {
            return self.run_command(Command::Sessions);
        }
        if ctrl && key.code == KeyCode::Char('p') {
            return self.run_command(Command::Model(String::new()));
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
        if ctrl && key.code == KeyCode::Char('o') {
            self.show_steps = !self.show_steps;
            return vec![];
        }

        // The `/` menu takes its keys first while it is open.
        if let Some(rows) = self.menu_rows() {
            let exact = rows.iter().any(|c| c.name == self.input.text());
            match key.code {
                KeyCode::Up => {
                    self.menu_selected = self.menu_selected.saturating_sub(1);
                    return vec![];
                }
                KeyCode::Down => {
                    self.menu_selected = (self.menu_selected + 1).min(rows.len() - 1);
                    return vec![];
                }
                KeyCode::Esc => {
                    self.menu_closed = true;
                    return vec![];
                }
                KeyCode::Tab => {
                    self.complete_menu(&rows);
                    return vec![];
                }
                KeyCode::Enter if !exact && !alt && !shift => {
                    self.complete_menu(&rows);
                    return vec![];
                }
                _ => {}
            }
        }
        let before = self.input.text();
        let actions = self.edit_key(key, ctrl, alt, shift);
        if self.input.text() != before {
            // A new text reopens the menu, from the top.
            self.menu_closed = false;
            self.menu_selected = 0;
        }
        actions
    }

    /// The `/` menu's rows, while it is open: the input is a command name
    /// still being typed.
    pub fn menu_rows(&self) -> Option<Vec<&'static crate::commands::CommandInfo>> {
        let text = self.input.text();
        if self.menu_closed
            || self.overlay.is_some()
            || !text.starts_with('/')
            || text.contains(char::is_whitespace)
        {
            return None;
        }
        let rows = crate::commands::matching(&text);
        (!rows.is_empty()).then_some(rows)
    }

    fn complete_menu(&mut self, rows: &[&'static crate::commands::CommandInfo]) {
        if let Some(chosen) = rows.get(self.menu_selected.min(rows.len() - 1)) {
            self.input.set(&format!("{} ", chosen.name));
        }
    }

    /// Keys for the input box itself.
    fn edit_key(&mut self, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) -> Vec<Action> {
        match key.code {
            KeyCode::Enter if alt || shift => self.input.newline(),
            KeyCode::Enter if ctrl => return self.submit_with(true),
            KeyCode::Enter => return self.submit(),
            KeyCode::Char(' ') if ctrl => {}
            // Editing like a text field (6.7a), with the terminal habits
            // that do not clash with it.
            KeyCode::Char('a') if ctrl => self.input.line_start(),
            KeyCode::Char('e') if ctrl => self.input.line_end(),
            KeyCode::Char('h') | KeyCode::Char('w') if ctrl => self.input.delete_word_back(),
            KeyCode::Char('k') if ctrl => self.input.kill_line_end(),
            KeyCode::Char('b') if alt => self.input.word_left(),
            KeyCode::Char('f') if alt => self.input.word_right(),
            KeyCode::Char('d') if alt => self.input.delete_word_forward(),
            KeyCode::Char(c) if !ctrl && !alt => self.input.insert(c),
            KeyCode::Backspace if ctrl || alt => self.input.delete_word_back(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete if ctrl => self.input.delete_word_forward(),
            KeyCode::Delete => self.input.delete(),
            // Shift+Tab cycles autonomy, as in Claude Code.
            KeyCode::BackTab => {
                let next = match self.autonomy.as_str() {
                    "interactive" => "autopilot",
                    "autopilot" => "plan",
                    _ => "interactive",
                };
                return self.mode_request("autonomy", next);
            }
            // The newest queued message, back to edit (or delete).
            KeyCode::Up if alt && self.input.is_empty() => {
                if let Some(session) = self.session_id.clone() {
                    if let Some(last) = self.queues.get_mut(&session).and_then(|q| q.pop_back()) {
                        self.input.set(&last.text);
                    }
                }
            }
            KeyCode::Left if ctrl => self.input.word_left(),
            KeyCode::Right if ctrl => self.input.word_right(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home if ctrl => self.input.home(),
            KeyCode::End if ctrl => self.input.end(),
            KeyCode::Home => self.input.line_start(),
            KeyCode::End => self.input.line_end(),
            // In a message of several lines, Up/Down move between them;
            // past the first or last line they walk history, as before.
            KeyCode::Up if self.history_pos.is_none() && self.input.line_up() => {}
            KeyCode::Down if self.history_pos.is_none() && self.input.line_down() => {}
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
            Overlay::Recall(_) => vec![],
            Overlay::Confirm(request) => {
                let answer = |approved, choice| Action::Confirm {
                    id: request.id,
                    approved,
                    choice,
                };
                match key.code {
                    // `y` is the plain yes: the first choice, if any.
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        vec![answer(true, request.choices.first().map(|c| c.key))]
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        vec![answer(false, None)]
                    }
                    KeyCode::Char(c) if request.choices.iter().any(|x| x.key == c) => {
                        vec![answer(true, Some(c))]
                    }
                    _ => {
                        self.overlay = Some(Overlay::Confirm(request));
                        vec![]
                    }
                }
            }
            Overlay::Models { rows, mut selected } => {
                let last = rows.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        self.overlay = Some(Overlay::Models { rows, selected });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(last);
                        self.overlay = Some(Overlay::Models { rows, selected });
                    }
                    KeyCode::Enter => {
                        if let Some(row) = rows.get(selected) {
                            return vec![Self::model_set(&row.name, None)];
                        }
                    }
                    _ => {
                        self.overlay = Some(Overlay::Models { rows, selected });
                    }
                }
                vec![]
            }
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

    /// A mouse wheel step: scrolls the conversation, or moves a picker's
    /// selection. Without this the terminal sent arrow keys, which recalled
    /// old prompts.
    pub fn on_wheel(&mut self, up: bool) {
        let code = if up { KeyCode::Up } else { KeyCode::Down };
        match &self.overlay {
            Some(Overlay::Models { .. }) | Some(Overlay::Sessions { .. }) => {
                self.handle_overlay_key(KeyEvent::new(code, KeyModifiers::NONE));
            }
            Some(_) => {}
            None if up => self.scroll_up(3),
            None => self.scroll_down(3),
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
                        Some("assistant") => Entry {
                            footer: message
                                .get("footer")
                                .and_then(Value::as_str)
                                .map(String::from),
                            ..Entry::assistant(text)
                        },
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

    fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Action> {
        app.handle_key(KeyEvent::new(code, modifiers))
    }

    /// Type `text` and press Enter.
    fn enter(app: &mut App, text: &str) -> Vec<Action> {
        app.input.set(text);
        press(app, KeyCode::Enter)
    }

    /// The prompt an action list asks, if it asks one.
    fn asked(actions: &[Action]) -> Option<String> {
        actions.iter().find_map(|a| match a {
            Action::Request {
                method: "ask",
                params,
                ..
            } => params["prompt"].as_str().map(String::from),
            _ => None,
        })
    }

    /// End the running turn: answered, cancelled, or failed.
    fn end_turn(app: &mut App, how: &str) -> Vec<Action> {
        let id = app.pending_id.expect("a turn is running");
        let response = match how {
            "answer" => Response::ok(id, json!({ "response": "done" })),
            "cancel" => Response::ok(id, json!({ "cancelled": true, "response": "" })),
            _ => Response::err(id, "the model fell over".into()),
        };
        app.on_response(response)
    }

    fn queued(app: &App) -> Vec<String> {
        app.queue()
            .map(|q| q.iter().map(|m| m.text.clone()).collect())
            .unwrap_or_default()
    }

    fn running(app: &mut App, text: &str) {
        let actions = enter(app, text);
        let Action::Request { .. } = &actions[0] else {
            panic!("no request")
        };
        let request = Request::new("ask", json!({}));
        app.begin_request(request.id, Pending::Turn);
    }

    /// Enter `text` and record the steer it sends.
    fn enter_steer(app: &mut App, text: &str) -> (Uuid, Queued) {
        let actions = enter(app, text);
        sent_steer(app, &actions)
    }

    /// The steer an action list sends, recorded as sent; its request id.
    fn sent_steer(app: &mut App, actions: &[Action]) -> (Uuid, Queued) {
        let steer = actions
            .iter()
            .find_map(|a| match a {
                Action::Steer { steer } => Some(steer.clone()),
                _ => None,
            })
            .expect("a steer");
        let request = Uuid::new_v4();
        app.steer_sent(request, steer.clone());
        (request, steer)
    }

    #[test]
    fn slash_steer_sends_a_steer_request() {
        let mut app = with_session();
        running(&mut app, "first");
        let (_, steer) = enter_steer(&mut app, "/steer use Pictures");
        assert_eq!(steer.text, "use Pictures");
        assert!(app
            .entries
            .iter()
            .any(|e| e.text.contains("steering: use Pictures")));
        assert!(queued(&app).is_empty());
    }

    #[test]
    fn slash_steer_with_no_turn_running_just_asks() {
        let mut app = with_session();
        assert_eq!(
            asked(&enter(&mut app, "/steer hello")).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn follow_up_steer_makes_enter_steer() {
        let mut app = with_session();
        app.follow_up = FollowUp::Steer;
        running(&mut app, "first");
        enter_steer(&mut app, "x");
    }

    #[test]
    fn ctrl_enter_flips_follow_up() {
        let mut app = with_session();
        running(&mut app, "first");
        app.input.set("x");
        let actions = key(&mut app, KeyCode::Enter, KeyModifiers::CONTROL);
        sent_steer(&mut app, &actions);
        app.follow_up = FollowUp::Steer;
        app.input.set("y");
        assert!(key(&mut app, KeyCode::Enter, KeyModifiers::CONTROL).is_empty());
        assert_eq!(queued(&app), vec!["y"]);
    }

    #[test]
    fn not_running_queues_the_steer() {
        let mut app = with_session();
        running(&mut app, "first");
        let (request, _) = enter_steer(&mut app, "/steer late");
        app.on_response(Response::ok(request, json!({ "status": "not_running" })));
        assert_eq!(queued(&app), vec!["late"]);
        assert!(app.in_flight.is_empty());
    }

    #[test]
    fn unused_steers_go_to_the_front_and_are_sent_next() {
        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "q1");
        let (request, steer) = enter_steer(&mut app, "/steer s");
        app.on_response(Response::ok(request, json!({ "status": "steering" })));
        let id = app.pending_id.unwrap();
        let next = app.on_response(Response::ok(
            id,
            json!({ "response": "done",
                    "unused_steers": [{ "id": steer.id.to_string(), "text": "s" }] }),
        ));
        assert_eq!(asked(&next).as_deref(), Some("s"));
        assert_eq!(queued(&app), vec!["q1"]);
        assert!(app.in_flight.is_empty());
    }

    #[test]
    fn a_cancel_requeues_only_unused_steers_held() {
        let mut app = with_session();
        running(&mut app, "first");
        let (r1, s1) = enter_steer(&mut app, "/steer one");
        let (r2, s2) = enter_steer(&mut app, "/steer two");
        app.on_response(Response::ok(r1, json!({ "status": "steering" })));
        app.on_response(Response::ok(r2, json!({ "status": "steering" })));
        let id = app.pending_id.unwrap();
        let next = app.on_response(Response::ok(
            id,
            json!({ "cancelled": true, "response": "",
                    "used_steers": [s1.id.to_string()],
                    "unused_steers": [{ "id": s2.id.to_string(), "text": "two" }] }),
        ));
        assert_eq!(asked(&next), None, "held");
        assert_eq!(queued(&app), vec!["two"]);
    }

    #[test]
    fn a_disconnect_requeues_every_in_flight_steer_first() {
        // Review Focus 3.
        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "q1");
        enter(&mut app, "q2");
        enter_steer(&mut app, "/steer s");
        app.on_disconnected("gone".into());
        assert_eq!(queued(&app), vec!["s", "q1", "q2"]);
        assert!(app.held.contains("abc"));
    }

    fn step(id: Uuid, n: u32, kind: &str, state: &str) -> jarvis_common::ipc::Step {
        jarvis_common::ipc::Step {
            id,
            step: n,
            kind: kind.into(),
            name: if kind == "tool" {
                "filesystem.list_dir".into()
            } else {
                "qwen".into()
            },
            summary: if state == "running" {
                format!("{kind} {n}")
            } else {
                String::new()
            },
            state: state.into(),
            detail: (state != "running").then(|| "ok".into()),
            ms: None,
        }
    }

    fn work_entry(app: &App) -> &WorkLog {
        app.entries
            .iter()
            .rev()
            .find_map(|e| e.work.as_ref())
            .expect("a work entry")
    }

    fn change(path: &str) -> jarvis_common::ipc::FileChange {
        jarvis_common::ipc::FileChange {
            undo_ids: vec![format!("id-{path}")],
            path: path.into(),
            tool: "filesystem.write".into(),
            added: 1,
            removed: 0,
            diff: Some(format!("+++ {path}\n+x\n")),
        }
    }

    #[test]
    fn steps_show_live_then_fold() {
        let mut app = with_session();
        running(&mut app, "list the files");
        let id = app.pending_id.unwrap();
        app.on_client_event(ClientEvent::Step(step(id, 1, "model", "running")));
        app.on_client_event(ClientEvent::Step(step(id, 1, "model", "done")));
        app.on_client_event(ClientEvent::Step(step(id, 2, "tool", "running")));
        let log = work_entry(&app);
        assert_eq!(log.steps.len(), 2);
        assert_eq!(log.steps[0].state, "done");
        assert_eq!(
            log.steps[0].summary, "model 1",
            "the end keeps the start's summary"
        );
        assert!(!log.done);
        app.on_client_event(ClientEvent::Step(step(id, 2, "tool", "done")));
        app.on_response(Response::ok(
            id,
            json!({ "response": "two files", "timing": { "total_ms": 12400 },
                    "changes": [change("/w/a.md")] }),
        ));
        let log = work_entry(&app);
        assert!(log.done);
        assert_eq!(log.ms, 12400);
        assert_eq!(log.changes.len(), 1);
    }

    #[test]
    fn the_row_counts_steps_files_and_declines() {
        let id = Uuid::new_v4();
        let log = WorkLog {
            request: id,
            steps: vec![
                step(id, 1, "model", "done"),
                step(id, 2, "tool", "declined"),
                step(id, 3, "model", "done"),
            ],
            done: true,
            ms: 12400,
            changes: vec![change("/w/a.md"), change("/w/b.md")],
            open: false,
            failed_turn: false,
        };
        assert_eq!(
            work_row(&log, false),
            "▸ 3 steps · 12.4 s · 2 files changed · 1 declined"
        );
        assert_eq!(
            work_row(&log, true),
            "▾ 3 steps · 12.4 s · 2 files changed · 1 declined"
        );
    }

    #[test]
    fn ctrl_o_opens_every_turn() {
        let mut app = with_session();
        assert!(!app.show_steps);
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(app.show_steps);
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(!app.show_steps);
    }

    #[test]
    fn a_click_on_a_row_opens_that_turn_only() {
        let mut app = with_session();
        running(&mut app, "one");
        end_turn(&mut app, "answer");
        running(&mut app, "two");
        end_turn(&mut app, "answer");
        let works: Vec<usize> = app
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.work.is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(works.len(), 2);
        app.work_rows = vec![(5, works[0]), (9, works[1])];
        app.on_click(9);
        assert!(!app.entries[works[0]].work.as_ref().unwrap().open);
        assert!(app.entries[works[1]].work.as_ref().unwrap().open);
        app.on_click(9);
        assert!(!app.entries[works[1]].work.as_ref().unwrap().open);
    }

    #[test]
    fn a_failed_turn_points_to_undo() {
        let mut app = with_session();
        running(&mut app, "one");
        end_turn(&mut app, "error");
        let log = work_entry(&app);
        assert!(log.failed_turn && log.done);
        assert!(
            work_row(log, false).contains("changes: see /undo"),
            "{}",
            work_row(log, false)
        );
    }

    #[test]
    fn a_used_steer_is_not_sent_again() {
        let mut app = with_session();
        running(&mut app, "first");
        let (request, steer) = enter_steer(&mut app, "/steer s");
        app.on_response(Response::ok(request, json!({ "status": "steering" })));
        let id = app.pending_id.unwrap();
        let next = app.on_response(Response::ok(
            id,
            json!({ "response": "done", "unused_steers": [], "used_steers": [steer.id.to_string()] }),
        ));
        assert_eq!(asked(&next), None);
        assert!(queued(&app).is_empty() && app.in_flight.is_empty());
    }

    #[test]
    fn a_late_not_running_after_the_turn_ended_is_queued_and_sent() {
        // Review of 6.7a: the reply came after the turn's end had dropped it.
        let mut app = with_session();
        running(&mut app, "first");
        let actions = enter(&mut app, "/steer one more thing");
        let (request, _) = sent_steer(&mut app, &actions);
        assert!(asked(&end_turn(&mut app, "answer")).is_none());
        let next = app.on_response(Response::ok(request, json!({ "status": "not_running" })));
        assert_eq!(asked(&next).as_deref(), Some("one more thing"));
    }

    #[test]
    fn a_late_steering_reply_belongs_to_the_next_turn() {
        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "queued next");
        let actions = enter(&mut app, "/steer late");
        let (request, steer) = sent_steer(&mut app, &actions);
        // The turn ends; the queued message starts the next one.
        let next = end_turn(&mut app, "answer");
        assert_eq!(asked(&next).as_deref(), Some("queued next"));
        let request2 = Request::new("ask", json!({}));
        app.begin_request(request2.id, Pending::Turn);
        app.on_response(Response::ok(request, json!({ "status": "steering" })));
        let after = app.on_response(Response::ok(
            request2.id,
            json!({ "response": "done",
                    "unused_steers": [{ "id": steer.id.to_string(), "text": "late" }] }),
        ));
        assert_eq!(asked(&after).as_deref(), Some("late"), "it was not lost");
    }

    #[test]
    fn reconnecting_mid_turn_holds_the_queue_and_keeps_steers() {
        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "q1");
        let actions = enter(&mut app, "/steer s");
        sent_steer(&mut app, &actions);
        app.reconnecting();
        assert_eq!(queued(&app), vec!["s", "q1"]);
        assert!(app.held.contains("abc"));
        assert!(app.in_flight.is_empty());
    }

    #[test]
    fn a_steer_that_could_not_be_sent_is_queued() {
        let mut app = with_session();
        running(&mut app, "first");
        let actions = enter(&mut app, "/steer offline");
        let Some(Action::Steer { steer }) = actions
            .into_iter()
            .find(|a| matches!(a, Action::Steer { .. }))
        else {
            panic!("no steer")
        };
        app.steer_unsent(steer);
        assert_eq!(queued(&app), vec!["offline"]);
    }

    #[test]
    fn enter_while_busy_queues_instead_of_refusing() {
        let mut app = with_session();
        running(&mut app, "first");
        let actions = enter(&mut app, "second");
        assert!(actions.is_empty());
        assert_eq!(queued(&app), vec!["second"]);
        assert!(app.input.text().is_empty());
        assert!(!app
            .entries
            .iter()
            .any(|e| e.role == Role::User && e.text == "second"));
    }

    #[test]
    fn the_queue_is_sent_in_order_after_each_turn() {
        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "second");
        enter(&mut app, "third");
        let next = end_turn(&mut app, "answer");
        assert_eq!(asked(&next).as_deref(), Some("second"));
        let request = Request::new("ask", json!({}));
        app.begin_request(request.id, Pending::Turn);
        assert_eq!(
            asked(&end_turn(&mut app, "answer")).as_deref(),
            Some("third")
        );
        let request = Request::new("ask", json!({}));
        app.begin_request(request.id, Pending::Turn);
        assert_eq!(asked(&end_turn(&mut app, "answer")), None);
        assert!(queued(&app).is_empty());
    }

    #[test]
    fn a_cancel_holds_the_queue_and_resume_sends_it() {
        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "second");
        assert_eq!(asked(&end_turn(&mut app, "cancel")), None);
        assert!(app
            .entries
            .iter()
            .any(|e| e.text.contains("Queue held: /resume sends it")));
        assert_eq!(
            asked(&enter(&mut app, "/resume")).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn an_error_or_disconnect_holds_the_queue() {
        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "second");
        assert_eq!(asked(&end_turn(&mut app, "error")), None);
        assert_eq!(queued(&app), vec!["second"]);

        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "second");
        app.on_disconnected("gone".into());
        assert_eq!(queued(&app), vec!["second"]);
        assert_eq!(
            asked(&enter(&mut app, "/resume")).as_deref(),
            Some("second")
        );
    }

    #[test]
    fn alt_up_takes_the_newest_back_to_edit() {
        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "a");
        enter(&mut app, "b");
        key(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.input.text(), "b");
        assert_eq!(queued(&app), vec!["a"]);
    }

    #[test]
    fn the_queue_belongs_to_its_conversation() {
        let mut app = with_session(); // conversation "abc"
        running(&mut app, "first");
        enter(&mut app, "for abc");
        end_turn(&mut app, "cancel"); // held
        app.session_id = Some("other".into());
        assert!(queued(&app).is_empty());
        assert_eq!(
            asked(&enter(&mut app, "/resume")),
            None,
            "nothing of abc's goes to other"
        );
        app.session_id = Some("abc".into());
        assert_eq!(queued(&app), vec!["for abc"]);
        assert_eq!(
            asked(&enter(&mut app, "/resume")).as_deref(),
            Some("for abc")
        );
    }

    #[test]
    fn commands_other_than_resume_and_queue_are_still_refused_while_busy() {
        let mut app = with_session();
        running(&mut app, "first");
        enter(&mut app, "second");
        assert!(enter(&mut app, "/new").is_empty());
        assert!(app.entries.last().unwrap().text.contains("Still waiting"));
        let shown = enter(&mut app, "/queue");
        assert!(shown.is_empty());
        assert!(
            app.entries.last().unwrap().text.contains("second"),
            "{:?}",
            app.entries.last()
        );
    }

    fn type_keys(app: &mut App, text: &str) {
        for ch in text.chars() {
            press(app, KeyCode::Char(ch));
        }
    }

    fn menu_names(app: &App) -> Option<Vec<&'static str>> {
        app.menu_rows()
            .map(|rows| rows.iter().map(|c| c.name).collect())
    }

    #[test]
    fn menu_opens_on_slash_and_closes_on_space() {
        let mut app = with_session();
        assert!(menu_names(&app).is_none());
        type_keys(&mut app, "/ap");
        assert_eq!(menu_names(&app).unwrap(), vec!["/approval", "/approve"]);
        type_keys(&mut app, "proval ");
        assert!(menu_names(&app).is_none(), "an argument is being typed");
    }

    #[test]
    fn tab_completes_the_selection_with_a_space() {
        let mut app = with_session();
        type_keys(&mut app, "/ap");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input.text(), "/approve ");
    }

    #[test]
    fn enter_runs_an_exact_command_or_completes() {
        let mut app = with_session();
        type_keys(&mut app, "/ne");
        let actions = press(&mut app, KeyCode::Enter);
        assert!(actions.is_empty());
        assert_eq!(app.input.text(), "/new ");
        app.input.clear();
        type_keys(&mut app, "/new");
        let actions = press(&mut app, KeyCode::Enter);
        assert_eq!(request_of(&actions).0, "session.new");
    }

    #[test]
    fn esc_closes_the_menu_and_keeps_the_text() {
        let mut app = with_session();
        type_keys(&mut app, "/mo");
        press(&mut app, KeyCode::Esc);
        assert!(menu_names(&app).is_none());
        assert_eq!(app.input.text(), "/mo");
        type_keys(&mut app, "d");
        assert!(menu_names(&app).is_some(), "typing again reopens it");
    }

    #[test]
    fn up_down_select_in_the_menu_not_history() {
        let mut app = with_session();
        app.history.push("old prompt".into());
        type_keys(&mut app, "/");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.menu_selected, 2);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.menu_selected, 1);
        assert_eq!(app.input.text(), "/", "history replaced the text");
    }

    #[test]
    fn ctrl_h_deletes_a_word_not_help() {
        // Without the keyboard protocol, Ctrl+Backspace arrives as Ctrl+H.
        let mut app = App::new("test".into());
        app.input.set("rename the files");
        key(&mut app, KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(app.input.text(), "rename the ");
        assert!(app.overlay.is_none(), "help opened mid-sentence");
        key(&mut app, KeyCode::Backspace, KeyModifiers::CONTROL);
        assert_eq!(app.input.text(), "rename ");
    }

    #[test]
    fn ctrl_arrows_move_by_word() {
        let mut app = App::new("test".into());
        app.input.set("/mnt/storage/CS");
        key(&mut app, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(app.input.cursor(), 13);
        key(&mut app, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(app.input.cursor(), 0);
        key(&mut app, KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(app.input.cursor(), 4);
        key(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(app.input.cursor(), 15);
        key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(app.input.cursor(), 0);
    }

    #[test]
    fn up_on_a_later_line_moves_not_history() {
        let mut app = App::new("test".into());
        app.history.push("an old prompt".into());
        app.input.set("first line\nsecond");
        key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            app.input.text(),
            "first line\nsecond",
            "history replaced the message"
        );
        assert_eq!(app.input.cursor(), 6);
        key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            app.input.text(),
            "first line\nsecond",
            "a message being written is not history"
        );
    }

    #[test]
    fn shift_enter_is_a_newline() {
        let mut app = App::new("test".into());
        app.session_id = Some("abc".into());
        app.input.set("one");
        let actions = key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        assert!(actions.is_empty());
        assert_eq!(app.input.text(), "one\n");
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
        assert_eq!(parse_command("/tmp/x.log has an error"), None);
    }

    #[test]
    fn an_unknown_command_is_not_sent_to_the_model() {
        // "/model remote : ..." in a TUI without /model went to llama as a
        // question (session fe3f7d69).
        assert_eq!(
            parse_command("/modle qwen"),
            Some(Command::Unknown("/modle".into()))
        );
        let mut app = App::new("test".into());
        app.input.set("/modle qwen");
        let actions = press(&mut app, KeyCode::Enter);
        assert!(actions.is_empty(), "{actions:?}");
        let last = app.entries.last().unwrap();
        assert!(last.text.contains("Unknown command /modle"), "{:?}", last);
    }

    #[test]
    fn the_mouse_wheel_scrolls_the_conversation_not_the_prompt_history() {
        let mut app = App::new("test".into());
        app.max_scroll = 100;
        app.history.push("an old prompt".into());
        app.on_wheel(true);
        assert_eq!(app.scroll, 3);
        assert!(app.input.is_empty(), "history was not recalled");
        app.on_wheel(false);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn the_wheel_moves_the_picker_selection() {
        let mut app = App::new("llama3.2:1b".into());
        let request = Request::new("model.list", json!({}));
        app.begin_request(request.id, Pending::Models);
        app.on_response(Response::ok(request.id, models_reply()));
        app.on_wheel(false);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Models { selected: 1, .. })
        ));
    }

    #[test]
    fn enter_sends_the_prompt_and_adds_it_to_history() {
        let mut app = App::new("test".into());
        app.session_id = Some("abc".into());
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
    fn a_reopened_answer_keeps_its_models_footer() {
        let entries = messages_to_entries(&json!({ "messages": [
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "hello", "footer": "qwen2.5:3b · local" },
            { "role": "assistant", "content": "old" },
        ]}));
        assert_eq!(entries[0].footer, None);
        assert_eq!(entries[1].footer.as_deref(), Some("qwen2.5:3b · local"));
        assert_eq!(entries[2].footer, None, "stored before models were");
    }

    #[test]
    fn the_footer_is_kept_beside_the_answer_not_in_it() {
        let mut app = App::new("test".into());
        let request = Request::new("ask", json!({"prompt": "hi"}));
        app.begin_request(request.id, Pending::Turn);
        app.on_response(Response::ok(
            request.id,
            json!({"response": "hello", "footer": "qwen2.5:3b · local · 40 tokens"}),
        ));
        let entry = app.entries.last().unwrap();
        assert_eq!(entry.text, "hello");
        assert_eq!(
            entry.footer.as_deref(),
            Some("qwen2.5:3b · local · 40 tokens")
        );
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
        assert!(
            !app.entries.iter().any(|e| e.role == Role::Assistant),
            "another request's answer was shown"
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

    fn typed(app: &mut App, text: &str) -> Vec<Action> {
        app.input.set(text);
        app.submit()
    }

    #[test]
    fn undo_previews_first_and_applies_only_what_was_shown() {
        let mut app = App::new("test".into());
        assert!(matches!(
            typed(&mut app, "/undo").as_slice(),
            [Action::UndoPreview(None)]
        ));
        assert!(matches!(
            typed(&mut app, "/undo list").as_slice(),
            [Action::UndoList]
        ));
        assert!(matches!(
            typed(&mut app, "/undo 2026110").as_slice(),
            [Action::UndoPreview(Some(id))] if id == "2026110"
        ));

        // Nothing previewed: nothing to confirm.
        assert!(typed(&mut app, "/undo yes").is_empty());
        assert!(app.entries.last().unwrap().text.contains("/undo first"));

        // The id shown is the id undone, even if a newer change lands
        // between the preview and the yes.
        app.offer_undo(
            "abc".into(),
            "Restore notes.md to its version from 10:15?".into(),
        );
        assert!(app.entries.last().unwrap().text.contains("/undo yes"));
        assert!(matches!(
            typed(&mut app, "/undo yes").as_slice(),
            [Action::UndoApply(id)] if id == "abc"
        ));
        assert!(
            typed(&mut app, "/undo yes").is_empty(),
            "one yes per preview"
        );
    }

    #[test]
    fn trust_can_be_listed_and_cleared() {
        let mut app = App::new("test".into());
        assert!(matches!(
            typed(&mut app, "/trust").as_slice(),
            [Action::Request {
                method: "trust.list",
                pending: Pending::Trust,
                ..
            }]
        ));
        assert!(matches!(
            typed(&mut app, "/trust clear").as_slice(),
            [Action::Request {
                method: "trust.clear",
                pending: Pending::Trust,
                ..
            }]
        ));

        let request = Request::new("trust.list", json!({}));
        app.begin_request(request.id, Pending::Trust);
        app.on_response(Response::ok(
            request.id,
            json!({ "trusted": ["https://a.example/"] }),
        ));
        assert!(app
            .entries
            .last()
            .unwrap()
            .text
            .contains("https://a.example/"));

        let request = Request::new("trust.clear", json!({}));
        app.begin_request(request.id, Pending::Trust);
        app.on_response(Response::ok(request.id, json!({ "trusted": [] })));
        assert!(app
            .entries
            .last()
            .unwrap()
            .text
            .contains("Nothing is trusted"));
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
            preview: None,
            id,
            tool: "shell.run".into(),
            args: json!({"command": "firefox"}),
            prompt: "Run shell command: firefox".into(),
            choices: Vec::new(),
        }));

        // An unrelated key must not dismiss or approve the request.
        let actions = press(&mut app, KeyCode::Char('x'));
        assert!(actions.is_empty());
        assert!(matches!(app.overlay, Some(Overlay::Confirm(_))));

        let actions = press(&mut app, KeyCode::Char('n'));
        assert!(matches!(
            actions.as_slice(),
            [Action::Confirm { id: answer, approved: false, .. }] if *answer == id
        ));
        assert!(app.overlay.is_none());
    }

    #[test]
    fn a_prompt_with_choices_answers_with_the_key_pressed() {
        let prompt = || {
            Overlay::Confirm(ConfirmationRequest {
                preview: None,
                id: Uuid::nil(),
                tool: "egress.send".into(),
                args: json!({}),
                prompt: "Send?".into(),
                choices: vec![
                    jarvis_common::ipc::Choice {
                        key: 'y',
                        label: "send once".into(),
                    },
                    jarvis_common::ipc::Choice {
                        key: 'a',
                        label: "always".into(),
                    },
                ],
            })
        };
        let mut app = App::new("test".into());
        for (key, expected) in [('a', Some('a')), ('y', Some('y'))] {
            app.overlay = Some(prompt());
            let actions = press(&mut app, KeyCode::Char(key));
            assert!(
                matches!(actions.as_slice(), [Action::Confirm { approved: true, choice, .. }] if *choice == expected),
                "{}: {:?}",
                key,
                actions
            );
        }
        // A key that was not offered neither approves nor dismisses.
        app.overlay = Some(prompt());
        assert!(press(&mut app, KeyCode::Char('x')).is_empty());
        assert!(app.overlay.is_some());
    }

    #[test]
    fn disconnect_drops_a_confirmation_that_can_no_longer_be_sent() {
        let mut app = App::new("test".into());
        app.overlay = Some(Overlay::Confirm(ConfirmationRequest {
            preview: None,
            id: Uuid::new_v4(),
            tool: "shell.run".into(),
            args: json!({}),
            prompt: "Run?".into(),
            choices: Vec::new(),
        }));
        app.on_disconnected("gone".into());
        assert!(app.overlay.is_none());
        assert!(matches!(app.state, DaemonState::Offline(_)));
    }

    #[test]
    fn history_walks_backwards_and_forwards() {
        let mut app = App::new("test".into());
        app.session_id = Some("abc".into());
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
            preview: None,
            id: confirm_id,
            tool: "shell.run".into(),
            args: json!({"command": "firefox"}),
            prompt: "Run shell command: firefox".into(),
            choices: Vec::new(),
        }));
        let actions = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(
            actions.as_slice(),
            [Action::Cancel { id: cancel_id }] if *cancel_id == turn_id
        ));
        assert!(app.overlay.is_none());
        assert!(app.cancelling);
    }

    // ------------------------------------------------------------ /model

    fn models_reply() -> Value {
        json!({ "models": [
            { "name": "llama3.2:1b", "remote": false, "current": true },
            { "name": "qwen2.5:3b", "remote": false, "current": false },
            { "name": "cohere/north-mini-code:free", "remote": true, "current": false },
        ]})
    }

    #[test]
    fn model_commands_parse() {
        assert_eq!(parse_command("/model"), Some(Command::Model(String::new())));
        assert_eq!(
            parse_command("/model qwen"),
            Some(Command::Model("qwen".into()))
        );
    }

    #[test]
    fn model_with_a_name_switches_directly() {
        let mut app = App::new("llama3.2:1b".into());
        let actions = app.run_command(Command::Model("qwen".into()));
        match actions.as_slice() {
            [Action::Request {
                method,
                params,
                pending,
            }] => {
                assert_eq!(*method, "model.set");
                assert_eq!(params["model"], "qwen");
                assert_eq!(*pending, Pending::Model(None));
            }
            other => panic!("unexpected actions: {other:?}"),
        }
    }

    #[test]
    fn model_alone_and_ctrl_p_open_the_picker_on_the_current_model() {
        for open in [
            |app: &mut App| app.run_command(Command::Model(String::new())),
            |app: &mut App| {
                app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            },
        ] {
            let mut app = App::new("qwen2.5:3b".into());
            let actions = open(&mut app);
            assert!(matches!(
                actions.as_slice(),
                [Action::Request {
                    method: "model.list",
                    pending: Pending::Models,
                    ..
                }]
            ));
            let request = Request::new("model.list", json!({}));
            app.begin_request(request.id, Pending::Models);
            let mut reply = models_reply();
            reply["models"][0]["current"] = json!(false);
            reply["models"][1]["current"] = json!(true);
            app.on_response(Response::ok(request.id, reply));
            match &app.overlay {
                Some(Overlay::Models { rows, selected }) => {
                    assert_eq!(rows.len(), 3);
                    assert_eq!(*selected, 1, "starts on the current model");
                }
                other => panic!("no picker: {other:?}"),
            }
        }
    }

    #[test]
    fn the_picker_moves_and_enter_switches_to_the_selected_model() {
        let mut app = App::new("llama3.2:1b".into());
        let request = Request::new("model.list", json!({}));
        app.begin_request(request.id, Pending::Models);
        app.on_response(Response::ok(request.id, models_reply()));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let actions = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match actions.as_slice() {
            [Action::Request { method, params, .. }] => {
                assert_eq!(*method, "model.set");
                assert_eq!(params["model"], "cohere/north-mini-code:free");
            }
            other => panic!("unexpected actions: {other:?}"),
        }
        assert!(app.overlay.is_none());
    }

    #[test]
    fn esc_closes_the_picker_without_switching() {
        let mut app = App::new("llama3.2:1b".into());
        let request = Request::new("model.list", json!({}));
        app.begin_request(request.id, Pending::Models);
        app.on_response(Response::ok(request.id, models_reply()));
        let actions = app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(actions.is_empty());
        assert!(app.overlay.is_none());
    }

    #[test]
    fn the_header_follows_the_model_that_answers() {
        // It showed the TUI's own config, so after /remote it named the
        // wrong model.
        let mut app = App::new("llama3.2:1b".into());
        let request = Request::new("model.set", json!({}));
        app.begin_request(request.id, Pending::Tier);
        app.on_response(Response::ok(
            request.id,
            json!({ "model": "qwen2.5:3b", "tier": "local", "privacy": "local" }),
        ));
        assert_eq!(app.model, "qwen2.5:3b");

        let request = Request::new("session.new", json!({}));
        app.begin_request(request.id, Pending::NewSession);
        app.on_response(Response::ok(
            request.id,
            json!({ "session_id": "abc", "model": "cohere/north-mini-code:free", "tier": "remote" }),
        ));
        assert_eq!(app.model, "cohere/north-mini-code:free");
    }

    #[test]
    fn model_then_a_message_switches_then_sends_it() {
        // Real use: "/model qwen <question>" looked for a model named
        // "qwen <question>", found none, and sent nothing.
        let mut app = App::new("llama3.2:1b".into());
        app.input.set("/model qwen: what is 2+2?");
        let actions = press(&mut app, KeyCode::Enter);
        let request = match actions.as_slice() {
            [Action::Request {
                method,
                params,
                pending,
            }] => {
                assert_eq!(*method, "model.set");
                assert_eq!(params["model"], "qwen", "the first word is the model");
                assert_eq!(*pending, Pending::Model(Some("what is 2+2?".into())));
                Request::new("model.set", json!({}))
            }
            other => panic!("unexpected actions: {other:?}"),
        };
        app.begin_request(request.id, Pending::Model(Some("what is 2+2?".into())));
        let actions = app.on_response(Response::ok(
            request.id,
            json!({ "model": "qwen2.5:3b", "tier": "local", "privacy": "local" }),
        ));
        assert_eq!(app.model, "qwen2.5:3b");
        match actions.as_slice() {
            [Action::Request { method, params, .. }] => {
                assert_eq!(*method, "ask");
                assert_eq!(params["prompt"], "what is 2+2?");
            }
            other => panic!("the message was not sent: {other:?}"),
        }
    }

    #[test]
    fn a_failed_switch_keeps_the_message_in_the_box() {
        let mut app = App::new("llama3.2:1b".into());
        let request = Request::new("model.set", json!({}));
        app.begin_request(request.id, Pending::Model(Some("my question".into())));
        let actions = app.on_response(Response::err(request.id, "No model matches".into()));
        assert!(actions.is_empty());
        assert_eq!(app.input.text(), "my question", "not lost");
    }

    // ------------------------------------------------------- /timing, /bad

    fn timing_reply() -> Value {
        json!({ "response": "hi", "footer": "llama3.2:1b · local", "timing": {
            "total_ms": 88200, "you_ms": 0,
            "model": [{ "model": "llama3.2:1b", "wall_ms": 88100, "load_ms": 41000,
                        "prompt_ms": 45100, "gen_ms": 1200, "prompt_tokens": 2375, "gen_tokens": 30 }],
            "tools": [{ "name": "shell.run", "ms": 20 }]
        }})
    }

    #[test]
    fn timing_and_feedback_commands_parse() {
        assert_eq!(parse_command("/timing"), Some(Command::Timing));
        assert_eq!(
            parse_command("/bad it is Canberra"),
            Some(Command::Feedback("bad".into(), "it is Canberra".into()))
        );
        assert_eq!(
            parse_command("/good"),
            Some(Command::Feedback("good".into(), String::new()))
        );
    }

    #[test]
    fn timing_shows_where_the_last_turn_went() {
        let mut app = App::new("llama3.2:1b".into());
        app.run_command(Command::Timing);
        assert!(app.entries.last().unwrap().text.contains("No turn"));
        let request = Request::new("ask", json!({}));
        app.begin_request(request.id, Pending::Turn);
        app.on_response(Response::ok(request.id, timing_reply()));
        app.run_command(Command::Timing);
        let text = &app.entries.last().unwrap().text;
        for part in [
            "88.2 s",
            "llama3.2:1b",
            "loading 41.0 s",
            "reading 2375 tokens 45.1 s",
            "writing 30 tokens 1.2 s",
            "shell.run 0.0 s",
        ] {
            assert!(text.contains(part), "missing {part:?} in:\n{text}");
        }
    }

    #[test]
    fn bad_sends_feedback_on_the_last_turn() {
        let mut app = App::new("test".into());
        let actions = app.run_command(Command::Feedback("bad".into(), "wrong".into()));
        match actions.as_slice() {
            [Action::Request {
                method,
                params,
                pending,
            }] => {
                assert_eq!(*method, "feedback");
                assert_eq!(params["kind"], "bad");
                assert_eq!(params["note"], "wrong");
                assert_eq!(*pending, Pending::Feedback);
            }
            other => panic!("unexpected actions: {other:?}"),
        }
    }

    // ------------------------------------------------- progress, no hangs

    #[test]
    fn a_long_turn_says_what_it_is_doing_and_for_how_long() {
        // Real use: a 50 s remote turn of five rounds showed only a spinner.
        let mut app = App::new("nemotron".into());
        let id = Uuid::new_v4();
        app.begin_request(id, Pending::Turn);
        assert_eq!(app.busy_label(Duration::from_secs(1)), "");
        app.on_client_event(ClientEvent::Progress {
            id,
            text: "asking nemotron-3-ultra (step 3)".into(),
        });
        assert_eq!(
            app.busy_label(Duration::from_secs(12)),
            "asking nemotron-3-ultra (step 3) · 12 s"
        );
        assert_eq!(
            app.busy_label(Duration::from_secs(25)),
            "asking nemotron-3-ultra (step 3) · 25 s · Ctrl+C cancels"
        );
        // Another request's progress is not this one's.
        app.on_client_event(ClientEvent::Progress {
            id: Uuid::new_v4(),
            text: "something else".into(),
        });
        assert!(app.busy_label(Duration::from_secs(5)).starts_with("asking"));
    }

    #[test]
    fn a_request_that_never_answers_is_given_up_after_30_seconds() {
        let mut app = App::new("test".into());
        app.begin_request(Uuid::new_v4(), Pending::Status);
        app.check_stuck(Duration::from_secs(29));
        assert!(app.is_busy());
        app.check_stuck(Duration::from_secs(31));
        assert!(!app.is_busy(), "the window is usable again");
        assert!(app.entries.last().unwrap().text.contains("did not answer"));
    }

    #[test]
    fn a_long_turn_is_never_given_up_on_by_itself() {
        // A remote turn of minutes is real work; only the owner cancels it.
        let mut app = App::new("test".into());
        app.begin_request(Uuid::new_v4(), Pending::Turn);
        app.check_stuck(Duration::from_secs(600));
        assert!(app.is_busy());
    }

    #[test]
    fn giving_up_on_a_request_is_noted_for_the_diagnostics_log() {
        let mut app = App::new("test".into());
        app.begin_request(Uuid::new_v4(), Pending::Status);
        app.check_stuck(Duration::from_secs(31));
        let notes = app.take_diag();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("gave up"), "{notes:?}");
        assert!(app.take_diag().is_empty(), "taken once");
    }

    // ------------------------------------------- 6.5a: a window, a conversation

    fn request_of(actions: &[Action]) -> (&'static str, Value) {
        match actions {
            [Action::Request { method, params, .. }] => (*method, params.clone()),
            other => panic!("expected one request, got {other:?}"),
        }
    }

    fn with_session() -> App {
        let mut app = App::new("llama3.2:1b".into());
        app.session_id = Some("abc".into());
        app
    }

    fn command(app: &mut App, text: &str) -> (&'static str, Value) {
        app.input.set(text);
        let actions = press(app, KeyCode::Enter);
        let (method, params) = request_of(&actions);
        (method, app.scoped(method, params).expect("scoped"))
    }

    #[test]
    fn mode_commands_send_mode_set() {
        let mut app = with_session();
        let cases = [
            ("/mode approval assisted", json!({ "approval": "assisted" })),
            ("/approval allow-all", json!({ "approval": "allow_all" })),
            ("/sandbox off", json!({ "sandbox": false })),
            ("/mode autopilot", json!({ "autonomy": "autopilot" })),
        ];
        for (text, change) in cases {
            let (method, params) = command(&mut app, text);
            assert_eq!(method, "mode.set", "{text}");
            for (k, v) in change.as_object().unwrap() {
                assert_eq!(&params[k], v, "{text}");
            }
            assert_eq!(params["session"], "abc", "{text}");
            app.pending = None;
            app.pending_id = None;
        }
        app.input.set("/sandbox maybe");
        let actions = press(&mut app, KeyCode::Enter);
        assert!(actions.is_empty());
        assert_eq!(app.entries.last().unwrap().role, Role::Error);
    }

    #[test]
    fn backtab_cycles_autonomy() {
        let mut app = with_session();
        let (method, params) = request_of(&press(&mut app, KeyCode::BackTab));
        assert_eq!(method, "mode.set");
        assert_eq!(params["autonomy"], "autopilot");
        let request = Request::new("mode.set", json!({}));
        app.begin_request(request.id, Pending::Mode);
        app.on_response(Response::ok(
            request.id,
            json!({ "mode": { "autonomy": "autopilot", "approval": "manual", "sandbox": true },
                    "mode_label": "Autopilot · Manual · Sandbox on" }),
        ));
        let (_, params) = request_of(&press(&mut app, KeyCode::BackTab));
        assert_eq!(params["autonomy"], "plan");
    }

    #[test]
    fn approve_and_go_send_their_requests() {
        let mut app = with_session();
        let (method, params) = command(&mut app, "/approve all");
        assert_eq!((method, params["which"].clone()), ("approve", json!("all")));
        app.pending = None;
        let (_, params) = command(&mut app, "/approve");
        assert_eq!(params["which"], "list");
        app.pending = None;
        let (_, params) = command(&mut app, "/approve 2");
        assert_eq!(params["which"], "2");
        app.pending = None;
        let (method, params) = command(&mut app, "/go");
        assert_eq!(method, "ask");
        assert_eq!(params["autonomy"], "interactive");
        assert!(params["prompt"].as_str().unwrap().contains("plan above"));
        app.pending = None;
        let (_, params) = command(&mut app, "/go auto");
        assert_eq!(params["autonomy"], "autopilot");
    }

    #[test]
    fn the_header_label_follows_replies() {
        let mut app = App::new("llama3.2:1b".into());
        assert_eq!(app.mode_label, "Interactive · Manual · Sandbox on");
        let request = Request::new("session.new", json!({}));
        app.begin_request(request.id, Pending::NewSession);
        app.on_response(Response::ok(
            request.id,
            json!({ "session_id": "abc", "model": "x",
                    "mode": { "autonomy": "plan", "approval": "assisted", "sandbox": true },
                    "mode_label": "Plan · Assisted · Sandbox on" }),
        ));
        assert_eq!(app.mode_label, "Plan · Assisted · Sandbox on");
        let request = Request::new("approve", json!({}));
        app.begin_request(request.id, Pending::Approve);
        app.on_response(Response::ok(
            request.id,
            json!({ "text": "1. running: git push: done\n", "remaining": 0 }),
        ));
        assert!(app.entries.last().unwrap().text.contains("git push: done"));
    }

    #[test]
    fn a_new_window_starts_a_new_conversation() {
        let mut app = App::new("llama3.2:1b".into());
        assert_eq!(request_of(&app.on_connected()).0, "session.new");
        let mut resuming = App::new("llama3.2:1b".into());
        resuming.continue_last = true;
        assert_eq!(request_of(&resuming.on_connected()).0, "session.last");
    }

    #[test]
    fn continue_with_nothing_to_continue_starts_a_new_one() {
        let mut app = App::new("llama3.2:1b".into());
        app.continue_last = true;
        let request = Request::new("session.last", json!({}));
        app.begin_request(request.id, Pending::LastSession);
        let actions = app.on_response(Response::ok(request.id, json!({ "session_id": null })));
        assert_eq!(request_of(&actions).0, "session.new");
    }

    #[test]
    fn a_failed_continue_falls_back_to_a_new_conversation() {
        // A window whose conversation could not be opened must not be
        // left without one, refusing every prompt.
        let mut app = App::new("llama3.2:1b".into());
        app.session_id = Some("gone".into());
        app.on_connected();
        app.session_id = None;
        let request = Request::new("session.show", json!({}));
        app.begin_request(request.id, Pending::ShowSession("gone".into()));
        let actions = app.on_response(Response::err(
            request.id,
            "There is no conversation gone.".into(),
        ));
        assert_eq!(request_of(&actions).0, "session.new");
    }

    #[test]
    fn a_prompt_with_no_conversation_is_kept_not_shown_as_sent() {
        let mut app = App::new("llama3.2:1b".into());
        let request = Request::new("session.new", json!({}));
        app.begin_request(request.id, Pending::NewSession);
        app.on_response(Response::err(request.id, "database is locked".into()));
        assert_eq!(app.session_id, None);
        app.input.set("hello");
        assert!(app.submit().is_empty());
        assert_eq!(app.input.text(), "hello");
        assert!(!app.entries.iter().any(|e| e.role == Role::User));
        assert!(app.entries.last().unwrap().text.contains("/new"));
    }

    #[test]
    fn a_new_conversation_reply_without_an_id_is_not_taken() {
        let mut app = App::new("llama3.2:1b".into());
        let request = Request::new("session.new", json!({}));
        app.begin_request(request.id, Pending::NewSession);
        app.on_response(Response::ok(request.id, json!({ "model": "x" })));
        assert_eq!(app.session_id, None);
    }

    #[test]
    fn a_reconnecting_window_keeps_its_conversation() {
        let mut app = App::new("llama3.2:1b".into());
        app.session_id = Some("abc".into());
        let (method, params) = request_of(&app.on_connected());
        assert_eq!(method, "session.show");
        assert_eq!(params["session_id"], "abc");
    }

    #[test]
    fn every_conversation_request_carries_the_windows_session() {
        let mut app = App::new("llama3.2:1b".into());
        app.session_id = Some("abc".into());
        for method in [
            "ask",
            "model.list",
            "model.set",
            "tier.set",
            "feedback",
            "trust.list",
            "trust.clear",
        ] {
            let params = app.scoped(method, json!({})).expect("sent");
            assert_eq!(params["session"], "abc", "{method}");
        }
        // Requests about the daemon carry none.
        for method in ["session.list", "session.new", "status", "tools.list"] {
            assert!(
                app.scoped(method, json!({}))
                    .unwrap()
                    .get("session")
                    .is_none(),
                "{method}"
            );
        }
    }

    #[test]
    fn a_conversation_request_before_the_window_has_one_is_held() {
        let app = App::new("llama3.2:1b".into());
        assert_eq!(app.scoped("ask", json!({ "prompt": "hi" })), None);
    }

    #[test]
    fn switch_is_the_windows_choice() {
        let mut app = App::new("llama3.2:1b".into());
        app.session_id = Some("abc".into());
        app.entries.push(Entry::assistant("old"));
        let (method, params) = request_of(&app.run_command(Command::Switch("xyz".into())));
        assert_eq!(method, "session.show", "no session.switch any more");
        assert_eq!(params["session_id"], "xyz");
        let request = Request::new("session.show", json!({}));
        app.begin_request(request.id, Pending::ShowSession("xyz".into()));
        app.on_response(Response::ok(
            request.id,
            json!({ "messages": [], "model": "qwen2.5:3b", "tier": "local" }),
        ));
        assert_eq!(app.session_id.as_deref(), Some("xyz"));
        assert_eq!(
            app.model, "qwen2.5:3b",
            "the header shows that conversation's model"
        );
    }

    #[test]
    fn deleting_this_windows_conversation_starts_a_new_one() {
        let mut app = App::new("llama3.2:1b".into());
        app.session_id = Some("abc".into());
        let request = Request::new("session.delete", json!({}));
        app.begin_request(request.id, Pending::DeleteSession("abc".into()));
        let actions = app.on_response(Response::ok(request.id, json!({ "ok": true })));
        assert_eq!(request_of(&actions).0, "session.new");
        // Another window's: this one carries on.
        let request = Request::new("session.delete", json!({}));
        app.begin_request(request.id, Pending::DeleteSession("other".into()));
        assert!(app
            .on_response(Response::ok(request.id, json!({ "ok": true })))
            .is_empty());
    }
}
