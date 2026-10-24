use clap::{Parser, Subcommand};
use jarvis_common::ipc::{ConfirmationRequest, Message, Request, Response};
use jarvis_core::{
    confirm::confirmation_prompt,
    init_tracing,
    orchestrator::{ConfirmationHandler, Orchestrator},
    permissions::{PermissionAction, PermissionManager},
};
use serde_json::json;
use std::io::IsTerminal as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{error, info};

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
    // MCP tools are discovered at runtime, so they cannot be validated
    // against the static list. They are named `mcp.<server>.<tool>`; accept
    // that shape so a rule can be written before the server is running.
    if !tool.starts_with("mcp.") {
        let known = jarvis_core::tools::known_tool_names();
        if !known.contains(&tool) {
            anyhow::bail!("unknown tool '{}'. Known tools: {}", tool, known.join(", "));
        }
    }
    manager.set_rule(tool, action.clone())?;
    println!("{} will {}.", tool, action.label());
    Ok(())
}

/// Print the tool catalogue, marking which tools come from an MCP server.
///
/// Telling the two apart matters: a native tool is part of Jarvis, while an
/// MCP tool is an external process with its own trust story. The caller
/// needs to see which is which without reading the source.
fn print_tools(tools: &[serde_json::Value]) {
    let names: Vec<&str> = tools.iter().filter_map(|t| t.as_str()).collect();
    if names.is_empty() {
        println!("No tools available.");
        return;
    }

    let native: Vec<&&str> = names.iter().filter(|n| !n.starts_with("mcp.")).collect();
    let mcp: Vec<&&str> = names.iter().filter(|n| n.starts_with("mcp.")).collect();

    if !native.is_empty() {
        println!("Built-in tools:");
        for name in &native {
            println!("  {}", name);
        }
    }
    if !mcp.is_empty() {
        if !native.is_empty() {
            println!();
        }
        println!("MCP tools:");
        for name in &mcp {
            // `mcp.<server>.<tool...>`
            let server = name.split('.').nth(1).unwrap_or("?");
            let rest = name.splitn(3, '.').nth(2).unwrap_or(name);
            println!("  {:<32} [mcp:{}] {}", name, server, rest);
        }
    }
    println!();
    println!("{} tool(s).", names.len());
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
    /// Search memory across all sessions
    Recall {
        query: String,
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },
    /// Backfill FTS index for existing messages
    MemoryBackfill,
    /// Show memory statistics
    MemoryStats,
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

fn get_log_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("jarvis")
        .join("jarvis.log")
}

/// Is a daemon actually listening, rather than just having left a PID file?
///
/// A successful connect is the only honest evidence: the PID file survives a
/// crash, and a shell that exited can leave a stale socket behind.
async fn daemon_responds() -> bool {
    let path = get_socket_path();
    if !path.exists() {
        return false;
    }
    tokio::net::UnixStream::connect(&path).await.is_ok()
}

/// Wait until the socket answers, or the timeout passes.
async fn wait_for_daemon(timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if daemon_responds().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until nothing answers on the socket, or the timeout passes.
async fn wait_for_daemon_to_stop(timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !daemon_responds().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn send_request(request: Request) -> anyhow::Result<Response> {
    Ok(send_request_streaming(request, false).await?.0)
}

/// Send a request and optionally print assistant chunks as they arrive.
///
/// Returns the final response and whether anything was streamed, so the caller
/// can avoid printing the answer twice. Streaming is only requested for
/// interactive `ask`; other commands have no chunks.
async fn send_request_streaming(
    request: Request,
    stream: bool,
) -> anyhow::Result<(Response, bool)> {
    let socket_path = get_socket_path();
    if !socket_path.exists() {
        anyhow::bail!("Daemon not running");
    }
    let stream_socket = UnixStream::connect(&socket_path).await?;
    let (reader, mut writer) = stream_socket.into_split();
    let request_id = request.id;

    writer
        .write_all(format!("{}\n", Message::Request(request).encode()).as_bytes())
        .await?;
    writer.flush().await?;

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut printed = false;
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
            Message::Chunk(chunk) => {
                if stream && chunk.id == request_id {
                    print!("{}", chunk.text);
                    let _ = std::io::stdout().flush();
                    printed = true;
                }
            }
            Message::Response(response) => return Ok((response, printed)),
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
            let interactive = std::io::stdout().is_terminal();
            match send_request_streaming(request, interactive).await {
                Ok((response, printed)) => {
                    if printed {
                        println!();
                    }
                    if response.ok {
                        let result = response.result.as_ref();
                        let cancelled = result
                            .and_then(|r| r.get("cancelled"))
                            .and_then(|c| c.as_bool())
                            .unwrap_or(false);
                        if cancelled {
                            // Keep any partial answer the user already saw.
                            println!("[cancelled]");
                            return Ok(());
                        }
                        if let Some(resp) = result
                            .and_then(|r| r.get("response"))
                            .and_then(|r| r.as_str())
                        {
                            if !printed {
                                println!("{}", resp);
                            }
                            return Ok(());
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
                    result = server.start() => {
                        if let Err(e) = result {
                            // The daemon is useless without a listener, so
                            // say why and exit instead of appearing alive.
                            error!("IPC server stopped: {}", e);
                            let _ = daemon.remove_pid().await;
                            std::process::exit(1);
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        info!("Shutting down daemon...");
                        let _ = daemon.remove_pid().await;
                        let _ = std::fs::remove_file(daemon.get_socket_path());
                    }
                }
            }
            DaemonCommands::Start => {
                // A second daemon would unlink the live one's socket and
                // leave that process unreachable, so refuse outright.
                if daemon_responds().await {
                    println!("Daemon is already running.");
                } else {
                    println!("Starting daemon in background...");
                    std::process::Command::new(std::env::current_exe()?)
                        .arg("daemon")
                        .arg("run")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()?;

                    // "Started" has to mean the socket answers. Reporting
                    // success because a process was spawned is how a dead
                    // daemon ends up looking healthy.
                    if wait_for_daemon(Duration::from_secs(5)).await {
                        println!("Daemon started");
                    } else {
                        println!("Daemon did not start. Check the log:");
                        println!("  {}", get_log_path().display());
                        std::process::exit(1);
                    }
                }
            }
            DaemonCommands::Stop => {
                let pid_path = get_pid_path();
                let socket_path = get_socket_path();

                let mut killed = false;
                if pid_path.exists() {
                    let pid = std::fs::read_to_string(&pid_path)?.trim().to_string();
                    println!("Stopping daemon (PID: {})...", pid);
                    let _ = std::process::Command::new("kill").arg(&pid).status();
                    killed = true;
                }

                let gone = wait_for_daemon_to_stop(Duration::from_secs(3)).await;
                let _ = std::fs::remove_file(&pid_path);
                if gone {
                    // Only remove the socket once nothing is listening on it.
                    let _ = std::fs::remove_file(&socket_path);
                }

                if gone && (killed || !socket_path.exists()) {
                    println!("Daemon stopped");
                } else if !gone {
                    println!("A daemon is still answering on {}", socket_path.display());
                } else {
                    println!("Daemon not running");
                }
            }
            DaemonCommands::Status => {
                let pid_path = get_pid_path();
                if daemon_responds().await {
                    let pid = std::fs::read_to_string(&pid_path)
                        .map(|p| p.trim().to_string())
                        .unwrap_or_else(|_| "unknown".to_string());
                    println!("Daemon is running (PID: {})", pid);
                    println!("Socket: {:?}", get_socket_path());
                } else if pid_path.exists() {
                    println!("Daemon is not running (stale PID file)");
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
                            match result.get("tools").and_then(|t| t.as_array()) {
                                Some(tools) => print_tools(tools),
                                None => println!("{}", serde_json::to_string_pretty(&result)?),
                            }
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
                            match result.get("tools").and_then(|t| t.as_array()) {
                                Some(tools) => print_tools(tools),
                                None => println!("{}", serde_json::to_string_pretty(&result)?),
                            }
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
        Commands::Recall { query, limit } => {
            let request = Request::new("memory.recall", json!({"query": query, "limit": limit}));
            match send_request(request).await {
                Ok(response) => {
                    if response.ok {
                        if let Some(result) = response.result {
                            if let Some(results) = result.get("results").and_then(|r| r.as_array())
                            {
                                if results.is_empty() {
                                    println!("No memories found.");
                                    return Ok(());
                                }
                                for r in results {
                                    let session =
                                        r.get("session_id").and_then(|s| s.as_str()).unwrap_or("?");
                                    let role =
                                        r.get("role").and_then(|s| s.as_str()).unwrap_or("?");
                                    let snip =
                                        r.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
                                    let sid = if session.len() > 8 {
                                        &session[..8]
                                    } else {
                                        session
                                    };
                                    println!("[{}] {}: {}", sid, role, snip);
                                }
                                return Ok(());
                            }
                        }
                    }
                    println!("Error: {}", response.error.unwrap_or_default());
                }
                Err(_) => println!("Daemon not running"),
            }
        }
        Commands::MemoryBackfill => {
            let request = Request::new("memory.backfill", json!({}));
            match send_request(request).await {
                Ok(response) => {
                    if response.ok {
                        if let Some(result) = response.result {
                            if let Some(count) = result.get("indexed").and_then(|c| c.as_u64()) {
                                println!("Indexed {} messages.", count);
                                return Ok(());
                            }
                        }
                    }
                    println!("Error: {}", response.error.unwrap_or_default());
                }
                Err(_) => println!("Daemon not running"),
            }
        }
        Commands::MemoryStats => {
            let request = Request::new("memory.stats", json!({}));
            match send_request(request).await {
                Ok(response) => {
                    if response.ok {
                        if let Some(result) = response.result {
                            let msgs = result
                                .get("total_messages")
                                .and_then(|c| c.as_u64())
                                .unwrap_or(0);
                            let sessions = result
                                .get("total_sessions")
                                .and_then(|c| c.as_u64())
                                .unwrap_or(0);
                            let size = result
                                .get("db_size_bytes")
                                .and_then(|c| c.as_u64())
                                .unwrap_or(0);
                            println!("Messages: {}", msgs);
                            println!("Sessions: {}", sessions);
                            println!("DB size: {} bytes ({:.2} KB)", size, size as f64 / 1024.0);
                            return Ok(());
                        }
                    }
                    println!("Error: {}", response.error.unwrap_or_default());
                }
                Err(_) => println!("Daemon not running"),
            }
        }
    }

    Ok(())
}
