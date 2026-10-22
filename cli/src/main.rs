use clap::{Parser, Subcommand};
use jarvis_common::ipc::{Request, Response};
use jarvis_core::{init_tracing, orchestrator::Orchestrator};
use serde_json::json;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::info;

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
}

#[derive(Subcommand)]
enum SessionCommands {
    /// List all sessions
    List { #[arg(short, long, default_value = "20")] limit: usize },
    /// Create a new session
    New,
    /// Switch to a session
    Switch { session_id: String },
    /// Show current session
    Current,
    /// Delete a session
    Delete { session_id: String },
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
    
    let request_json = serde_json::to_string(&request)?;
    writer.write_all(format!("{}\n", request_json).as_bytes()).await?;
    writer.flush().await?;
    
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    
    let response: Response = serde_json::from_str(&line)?;
    Ok(response)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

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
                    println!("Error: {}", response.error.unwrap_or_else(|| "Unknown error".to_string()));
                }
                Err(_) => {
                    // Fallback to direct mode
                    info!("Daemon not running, using direct mode");
                    let orch = Orchestrator::new();
                    let response = orch.run(&prompt).await?;
                    println!("{}", response);
                }
            }
        }
        Commands::Daemon { action } => {
            match action {
                DaemonCommands::Run => {
                    info!("Starting Jarvis daemon in foreground...");
                    let config = jarvis_core::config::Config::default();
                    let daemon = jarvis_core::daemon::Daemon::new(config.clone());
                    daemon.write_pid().await?;
                    let daemon_arc = std::sync::Arc::new(tokio::sync::Mutex::new(daemon.clone()));
                    let server = jarvis_core::ipc::server::IpcServer::new(daemon_arc, daemon.get_socket_path());
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
                        println!("Daemon is running (PID: {})", std::fs::read_to_string(&pid_path)?.trim());
                        println!("Socket: {:?}", get_socket_path());
                    } else {
                        println!("Daemon is not running");
                    }
                }
            }
        }
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
        Commands::Session { action } => {
            match action {
                SessionCommands::List { limit } => {
                    let request = Request::new("session.list", json!({"limit": limit}));
                    match send_request(request).await {
                        Ok(response) => {
                            if response.ok {
                                if let Some(result) = response.result {
                                    if let Some(sessions) = result.get("sessions").and_then(|s| s.as_array()) {
                                        if sessions.is_empty() {
                                            println!("No sessions found");
                                        } else {
                                            println!("Sessions:");
                                            for s in sessions {
                                                let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                                                let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled");
                                                let count = s.get("message_count").and_then(|v| v.as_i64()).unwrap_or(0);
                                                println!("  {} - {} ({} messages)", &id[..8.min(id.len())], title, count);
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
                                    if let Some(sid) = result.get("session_id").and_then(|v| v.as_str()) {
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
                                    if let Some(sid) = result.get("session_id").and_then(|v| v.as_str()) {
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
            }
        }
    }

    Ok(())
}
