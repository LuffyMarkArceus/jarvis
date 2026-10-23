use clap::{Parser, Subcommand};
use jarvis_common::ipc::{ConfirmationRequest, Message, Request, Response};
use jarvis_core::{
    confirm::confirmation_prompt,
    init_tracing,
    orchestrator::{ConfirmationHandler, Orchestrator},
    permissions::{PermissionAction, PermissionManager},
};
use serde_json::json;
use std::io::Write as _;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::info;

/// Asks the user, on the terminal, before a tool call runs.
///
/// Used in direct mode, where there is no daemon to relay the prompt. The
/// prompt goes to stderr so `jarvis ask ... > file` still captures only the
/// answer.
struct TerminalConfirmer;

#[async_trait::async_trait]
impl ConfirmationHandler for TerminalConfirmer {
    async fn confirm(&self, tool: &str, args: &serde_json::Value) -> bool {
        ask_yn(&confirmation_prompt(tool, args))
    }
}

/// Show a prompt and read a yes/no answer. Anything but an explicit yes is no.
fn ask_yn(prompt: &str) -> bool {
    eprintln!();
    eprintln!("{}", prompt);
    eprint!("Allow? [y/N] ");
    let _ = std::io::stderr().flush();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Answer a confirmation prompt that arrived over the daemon socket.
fn answer_confirmation(request: &ConfirmationRequest) -> bool {
    ask_yn(&request.prompt)
}

/// The short name of a permission action.
fn action_name(action: &PermissionAction) -> &'static str {
    match action {
        PermissionAction::Allow => "allow",
        PermissionAction::Ask => "ask",
        PermissionAction::Deny => "deny",
    }
}

/// Set a permission rule, rejecting tool names that do not exist.
///
/// A rule for a misspelled tool name would never match anything, so it
/// would look like it took effect while silently doing nothing.
fn set_permission(
    manager: &PermissionManager,
    tool: &str,
    action: PermissionAction,
) -> anyhow::Result<()> {
    let known = jarvis_core::tools::known_tool_names();
    if !known.contains(&tool) {
        anyhow::bail!("unknown tool '{}'. Known tools: {}", tool, known.join(", "));
    }
    manager.set_rule(tool, action.clone())?;
    println!("{} will {}.", tool, action.label());
    Ok(())
}

/// A short, single-line rendering of tool arguments.
fn compact_args(args: &serde_json::Value) -> String {
    let rendered = args.to_string();
    if rendered.chars().count() <= 60 {
        return rendered;
    }
    let mut out: String = rendered.chars().take(60).collect();
    out.push_str("...");
    out
}

#[derive(Parser)]
#[command(name = "jarvis")]
#[command(about = "Jarvis - Personal AI Assistant", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Ask Jarvis something
    Ask { prompt: String },
    /// Start the daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonCommands,
    },
    /// Show status
    Status,
    /// List available tools
    Tools,
    /// Manage sessions
    Session {
        #[command(subcommand)]
        action: SessionCommands,
    },
    /// Manage the permission policy
    Permissions {
        #[command(subcommand)]
        action: PermissionsCommands,
    },
}

#[derive(Subcommand)]
enum SessionCommands {
    /// List all sessions
    List {
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },
    /// Create a new session
    New,
    /// Switch to a session
    Switch { session_id: String },
    /// Show current session
    Current,
    /// Delete a session
    Delete { session_id: String },
    /// Show full conversation history
    Show { session_id: String },
    /// Prune empty sessions
    Prune,
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Run daemon in foreground
    Run,
    /// Start daemon in background
    Start,
    /// Stop daemon
    Stop,
    /// Check daemon status
    Status,
}

#[derive(Subcommand)]
enum PermissionsCommands {
    /// Show the current rules
    Show,
    /// Let a tool run without asking
    Allow { tool: String },
    /// Ask for confirmation before a tool runs
    Ask { tool: String },
    /// Never run a tool
    Deny { tool: String },
    /// Remove a rule, restoring the default behaviour
    Reset { tool: String },
    /// Manage per-command rules for shell.run
    Command {
        #[command(subcommand)]
        action: CommandRuleCommands,
    },
    /// Show recent tool decisions
    Audit {
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum CommandRuleCommands {
    /// Run matching commands without asking
    Allow {
        /// Glob over the whole command, e.g. "git status" or "git *"
        pattern: String,
    },
    /// Ask before running matching commands
    Ask { pattern: String },
    /// Never run matching commands
    Deny { pattern: String },
    /// Remove a command rule
    Forget { pattern: String },
}

fn get_socket_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("jarvis")
        .join("jarvis.sock")
}

fn get_pid_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("jarvis")
        .join("jarvis.pid")
}

async fn send_request(request: Request) -> anyhow::Result<Response> {
    let socket_path = get_socket_path();
    if !socket_path.exists() {
        anyhow::bail!("Daemon not running");
    }
    let stream = UnixStream::connect(&socket_path).await?;
    let (reader, mut writer) = stream.into_split();

    writer
        .write_all(format!("{}\n", Message::Request(request).encode()).as_bytes())
        .await?;
    writer.flush().await?;

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            anyhow::bail!("Daemon closed the connection");
        }
        match Message::decode(&line)? {
            // The daemon needs a decision before it can answer. Prompt, send
            // the answer back, and keep reading for the real response.
            Message::ConfirmationRequest(request) => {
                let approved = answer_confirmation(&request);
                let reply = Message::ConfirmationReply(jarvis_common::ipc::ConfirmationReply {
                    id: request.id,
                    approved,
                });
                writer
                    .write_all(format!("{}\n", reply.encode()).as_bytes())
                    .await?;
                writer.flush().await?;
            }
            Message::Response(response) => return Ok(response),
            other => {
                info!("ignoring unexpected message from daemon: {:?}", other);
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // The daemon detaches from the terminal, so stdout is not a usable log
    // sink. Log to a file so agent turns are debuggable after the fact.
    if let Commands::Daemon {
        action: DaemonCommands::Run,
    } = cli.command
    {
        let config = jarvis_core::config::Config::load();
        let log_path = config.general.data_dir.join("jarvis.log");
        if let Err(e) = jarvis_core::init_tracing_to_file(&log_path) {
            eprintln!("warning: could not open log file: {}", e);
        }
    } else {
        init_tracing();
    }
    match cli.command {
        Commands::Ask { prompt } => {
            // Try to connect to daemon first
            let request = Request::new("ask", json!({"prompt": prompt}));
            match send_request(request).await {
                Ok(response) => {
                    if response.ok {
                        if let Some(result) = response.result {
                            if let Some(resp) = result.get("response").and_then(|r| r.as_str()) {
                                println!("{}", resp);
                                return Ok(());
                            }
                        }
                    }
                    println!(
                        "Error: {}",
                        response
                            .error
                            .unwrap_or_else(|| "Unknown error".to_string())
                    );
                }
                Err(_) => {
                    // Fallback to direct mode
                    info!("Daemon not running, using direct mode");
                    let orch = Orchestrator::new();
                    let response = orch.run_with(&prompt, Some(&TerminalConfirmer)).await?;
                    println!("{}", response);
                }
            }
        }
        Commands::Daemon { action } => match action {
            DaemonCommands::Run => {
                info!("Starting Jarvis daemon in foreground...");
                let config = jarvis_core::config::Config::load();
                let daemon = jarvis_core::daemon::Daemon::new(config.clone());
                daemon.write_pid().await?;
                let daemon_arc = std::sync::Arc::new(tokio::sync::Mutex::new(daemon.clone()));
                let server =
                    jarvis_core::ipc::server::IpcServer::new(daemon_arc, daemon.get_socket_path());
                tokio::select! {
                    _ = server.start() => {},
                    _ = tokio::signal::ctrl_c() => {
                        info!("Shutting down daemon...");
                        let _ = daemon.remove_pid().await;
                    }
                }
            }
            DaemonCommands::Start => {
                println!("Starting daemon in background...");
                std::process::Command::new(std::env::current_exe()?)
                    .arg("daemon")
                    .arg("run")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                println!("Daemon started");
            }
            DaemonCommands::Stop => {
                let pid_path = get_pid_path();
                if pid_path.exists() {
                    let pid = std::fs::read_to_string(&pid_path)?;
                    println!("Stopping daemon (PID: {})...", pid.trim());
                    let _ = std::process::Command::new("kill").arg(pid.trim()).spawn();
                    std::fs::remove_file(&pid_path)?;
                    let socket_path = get_socket_path();
                    if socket_path.exists() {
                        let _ = std::fs::remove_file(&socket_path);
                    }
                    println!("Daemon stopped");
                } else {
                    println!("Daemon not running");
                }
            }
            DaemonCommands::Status => {
                let pid_path = get_pid_path();
                if pid_path.exists() {
                    println!(
                        "Daemon is running (PID: {})",
                        std::fs::read_to_string(&pid_path)?.trim()
                    );
                    println!("Socket: {:?}", get_socket_path());
                } else {
                    println!("Daemon is not running");
                }
            }
        },
        Commands::Status => {
            // Try daemon first
            let request = Request::new("status", json!({}));
            match send_request(request).await {
                Ok(response) => {
                    if response.ok {
                        if let Some(result) = response.result {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        }
                    }
                }
                Err(_) => {
                    println!("Jarvis CLI v0.1.0 - Direct mode (daemon not running)");
                    println!("Ollama: http://127.0.0.1:11434");
                }
            }
        }
        Commands::Tools => {
            let request = Request::new("tools.list", json!({}));
            match send_request(request).await {
                Ok(response) => {
                    if response.ok {
                        if let Some(result) = response.result {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        }
                    }
                }
                Err(_) => {
                    println!("Daemon not running, cannot list tools");
                }
            }
        }
        Commands::Permissions { action } => {
            let config = jarvis_core::config::Config::load();
            let manager = PermissionManager::new(&config)?;
            match action {
                PermissionsCommands::Show => {
                    println!("Policy file: {}", manager.get_config_path().display());
                    println!();
                    println!("{:<22} {:<6} EFFECT", "TOOL", "ACTION");
                    for rule in manager.rules() {
                        println!(
                            "{:<22} {:<6} {}",
                            rule.tool,
                            action_name(&rule.action),
                            rule.action.label()
                        );
                        for p in rule.patterns.iter().flatten() {
                            println!(
                                "  {:<20} {:<6} {}",
                                format!("\"{}\"", p.pattern),
                                action_name(&p.action),
                                p.action.label()
                            );
                        }
                    }
                    println!();
                    println!("A tool with no rule defaults to 'ask'.");
                    println!(
                        "For shell.run, if several patterns match, the most restrictive wins."
                    );
                }
                PermissionsCommands::Command { action } => match action {
                    CommandRuleCommands::Allow { pattern } => {
                        manager.set_pattern("shell.run", &pattern, PermissionAction::Allow)?;
                        println!("Commands matching \"{}\" will run without asking.", pattern);
                    }
                    CommandRuleCommands::Ask { pattern } => {
                        manager.set_pattern("shell.run", &pattern, PermissionAction::Ask)?;
                        println!("Commands matching \"{}\" will ask first.", pattern);
                    }
                    CommandRuleCommands::Deny { pattern } => {
                        manager.set_pattern("shell.run", &pattern, PermissionAction::Deny)?;
                        println!("Commands matching \"{}\" will never run.", pattern);
                    }
                    CommandRuleCommands::Forget { pattern } => {
                        if manager.remove_pattern("shell.run", &pattern)? {
                            println!("Removed the rule for \"{}\".", pattern);
                        } else {
                            println!("There was no rule for \"{}\".", pattern);
                        }
                    }
                },
                PermissionsCommands::Allow { tool } => {
                    set_permission(&manager, &tool, PermissionAction::Allow)?;
                }
                PermissionsCommands::Ask { tool } => {
                    set_permission(&manager, &tool, PermissionAction::Ask)?;
                }
                PermissionsCommands::Deny { tool } => {
                    set_permission(&manager, &tool, PermissionAction::Deny)?;
                }
                PermissionsCommands::Reset { tool } => {
                    if manager.remove_rule(&tool)? {
                        println!(
                            "Removed the rule for {}. It now falls back to the default (ask).",
                            tool
                        );
                    } else {
                        println!("There was no rule for {}.", tool);
                    }
                }
                PermissionsCommands::Audit { limit } => {
                    let entries = manager.get_audit_log(limit)?;
                    if entries.is_empty() {
                        println!("No tool decisions recorded yet.");
                    } else {
                        println!("{:<20} {:<16} ARGS", "TOOL", "DECISION");
                        for entry in entries {
                            println!(
                                "{:<20} {:<16} {}",
                                entry.tool,
                                entry.decision,
                                compact_args(&entry.args)
                            );
                        }
                    }
                }
            }
        }
        Commands::Session { action } => match action {
            SessionCommands::List { limit } => {
                let request = Request::new("session.list", json!({"limit": limit}));
                let current_req = Request::new("session.current", json!({}));
                match send_request(request).await {
                    Ok(response) => {
                        let current_id = if let Ok(cur) = send_request(current_req).await {
                            if cur.ok {
                                if let Some(r) = cur.result {
                                    r.get("session_id")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string())
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        if response.ok {
                            if let Some(result) = response.result {
                                if let Some(sessions) =
                                    result.get("sessions").and_then(|s| s.as_array())
                                {
                                    if sessions.is_empty() {
                                        println!("No sessions found");
                                    } else {
                                        println!("Sessions:");
                                        for s in sessions {
                                            let id =
                                                s.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                                            let title = s
                                                .get("title")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("Untitled");
                                            let count = s
                                                .get("message_count")
                                                .and_then(|v| v.as_i64())
                                                .unwrap_or(0);
                                            let marker = if let Some(ref cur) = current_id {
                                                if cur == id {
                                                    "*"
                                                } else {
                                                    " "
                                                }
                                            } else {
                                                " "
                                            };
                                            // Try to get relative time from updated_at
                                            let updated = s
                                                .get("updated_at")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("");
                                            // Parse and format relative time (simple)
                                            let time_str = if let Ok(dt) =
                                                chrono::DateTime::parse_from_rfc3339(updated)
                                            {
                                                let dt_utc = dt.with_timezone(&chrono::Utc);
                                                let now = chrono::Utc::now();
                                                let diff = now.signed_duration_since(dt_utc);
                                                if diff.num_seconds() < 60 {
                                                    "just now".to_string()
                                                } else if diff.num_minutes() < 60 {
                                                    format!("{}m ago", diff.num_minutes())
                                                } else if diff.num_hours() < 24 {
                                                    format!("{}h ago", diff.num_hours())
                                                } else if diff.num_days() < 7 {
                                                    format!("{}d ago", diff.num_days())
                                                } else {
                                                    dt_utc.format("%Y-%m-%d").to_string()
                                                }
                                            } else {
                                                updated.to_string()
                                            };
                                            println!(
                                                "{} {} - {} ({} messages, {})",
                                                marker,
                                                &id[..8.min(id.len())],
                                                title,
                                                count,
                                                time_str
                                            );
                                        }
                                    }
                                }
                            }
                        } else {
                            println!("Error: {}", response.error.unwrap_or_default());
                        }
                    }
                    Err(_) => println!("Daemon not running"),
                }
            }
            SessionCommands::New => {
                let request = Request::new("session.new", json!({}));
                match send_request(request).await {
                    Ok(response) => {
                        if response.ok {
                            if let Some(result) = response.result {
                                if let Some(sid) = result.get("session_id").and_then(|v| v.as_str())
                                {
                                    println!("Created new session: {}", sid);
                                }
                            }
                        } else {
                            println!("Error: {}", response.error.unwrap_or_default());
                        }
                    }
                    Err(_) => println!("Daemon not running"),
                }
            }
            SessionCommands::Switch { session_id } => {
                let request = Request::new("session.switch", json!({"session_id": session_id}));
                match send_request(request).await {
                    Ok(response) => {
                        if response.ok {
                            println!("Switched to session: {}", session_id);
                        } else {
                            println!("Error: {}", response.error.unwrap_or_default());
                        }
                    }
                    Err(_) => println!("Daemon not running"),
                }
            }
            SessionCommands::Current => {
                let request = Request::new("session.current", json!({}));
                match send_request(request).await {
                    Ok(response) => {
                        if response.ok {
                            if let Some(result) = response.result {
                                if let Some(sid) = result.get("session_id").and_then(|v| v.as_str())
                                {
                                    println!("Current session: {}", sid);
                                }
                            }
                        } else {
                            println!("Error: {}", response.error.unwrap_or_default());
                        }
                    }
                    Err(_) => println!("Daemon not running"),
                }
            }
            SessionCommands::Delete { session_id } => {
                let request = Request::new("session.delete", json!({"session_id": session_id}));
                match send_request(request).await {
                    Ok(response) => {
                        if response.ok {
                            println!("Deleted session: {}", session_id);
                        } else {
                            println!("Error: {}", response.error.unwrap_or_default());
                        }
                    }
                    Err(_) => println!("Daemon not running"),
                }
            }
            SessionCommands::Show { session_id } => {
                let request = Request::new("session.show", json!({"session_id": session_id}));
                match send_request(request).await {
                    Ok(response) => {
                        if response.ok {
                            if let Some(result) = response.result {
                                if let Some(messages) =
                                    result.get("messages").and_then(|m| m.as_array())
                                {
                                    if messages.is_empty() {
                                        println!("No messages in this session");
                                    } else {
                                        for msg in messages {
                                            let role = msg
                                                .get("role")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("?");
                                            let content = msg
                                                .get("content")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("");
                                            println!("[{}]: {}", role, content);
                                            println!("---");
                                        }
                                    }
                                }
                            }
                        } else {
                            println!("Error: {}", response.error.unwrap_or_default());
                        }
                    }
                    Err(_) => println!("Daemon not running"),
                }
            }
            SessionCommands::Prune => {
                let request = Request::new("session.prune", json!({}));
                match send_request(request).await {
                    Ok(response) => {
                        if response.ok {
                            if let Some(result) = response.result {
                                if let Some(pruned) = result.get("pruned").and_then(|v| v.as_u64())
                                {
                                    println!("Pruned {} empty session(s)", pruned);
                                }
                            }
                        } else {
                            println!("Error: {}", response.error.unwrap_or_default());
                        }
                    }
                    Err(_) => println!("Daemon not running"),
                }
            }
        },
    }

    Ok(())
}
