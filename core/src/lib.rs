//! Jarvis Core - Orchestrator, tools, memory, and IPC

pub mod config;
pub mod daemon;
pub mod error;
pub mod ipc;
pub mod memory;
pub mod model;
pub mod orchestrator;
pub mod permissions;
pub mod tools;

pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "jarvis_core=info,jarvis_cli=info".into()),
        )
        .init();
}
