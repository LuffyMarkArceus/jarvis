//! Jarvis Core - Orchestrator, tools, memory, and IPC

pub mod config;
pub mod confirm;
pub mod context;
pub mod daemon;
pub mod error;
pub mod ipc;
pub mod memory;
pub mod model;
pub mod orchestrator;
pub mod permissions;
pub mod router;
pub mod tools;

pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "jarvis_core=info,jarvis_cli=info".into()),
        )
        .init();
}

/// Initialise tracing that writes to a file instead of stdout.
///
/// The daemon detaches from the terminal, so stdout is not a usable log
/// sink. This gives the daemon a durable log for debugging agent turns.
pub fn init_tracing_to_file(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "jarvis_core=info,jarvis_cli=info".into()),
        )
        .with_ansi(false)
        .with_writer(move || file.try_clone().expect("log file clone"))
        .init();
    Ok(())
}
