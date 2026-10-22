//! Jarvis Daemon

use crate::config::Config;
use crate::error::Result;
use crate::orchestrator::Orchestrator;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

#[derive(Clone)]
pub struct Daemon {
    orchestrator: Arc<Mutex<Orchestrator>>,
    config: Config,
}

impl Daemon {
    pub fn new(config: Config) -> Self {
        let orchestrator = Orchestrator::with_config(config.clone());
        Self {
            orchestrator: Arc::new(Mutex::new(orchestrator)),
            config,
        }
    }

    pub async fn ask(&self, prompt: &str) -> Result<String> {
        let orch = self.orchestrator.lock().await;
        orch.run(prompt).await
    }

    pub fn list_tools(&self) -> Vec<String> {
        // Can't easily call async, return empty for now or store separately
        vec!["filesystem.list_dir".to_string(), "shell.run".to_string(), "system.info".to_string()]
    }

    pub fn session_id(&self) -> String {
        "daemon-session".to_string()
    }

    pub fn get_socket_path(&self) -> PathBuf {
        self.config.general.data_dir.join("jarvis.sock")
    }

    pub fn get_pid_path(&self) -> PathBuf {
        self.config.general.data_dir.join("jarvis.pid")
    }

    pub async fn write_pid(&self) -> Result<()> {
        let pid = std::process::id();
        std::fs::create_dir_all(&self.config.general.data_dir)?;
        std::fs::write(self.get_pid_path(), pid.to_string())?;
        info!("PID {} written", pid);
        Ok(())
    }

    pub async fn remove_pid(&self) -> Result<()> {
        if self.get_pid_path().exists() {
            std::fs::remove_file(self.get_pid_path())?;
        }
        Ok(())
    }
}
