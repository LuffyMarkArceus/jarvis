//! Jarvis Daemon

use crate::config::Config;
use crate::error::{JarvisError, Result};
use crate::memory::{MemoryManager, SessionInfo};
use crate::orchestrator::Orchestrator;
use crate::permissions::PermissionManager;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;
use uuid::Uuid;

#[derive(Clone)]
pub struct Daemon {
    orchestrator: Arc<Mutex<Orchestrator>>,
    config: Config,
    memory: Option<MemoryManager>,
    #[allow(dead_code)]
    permissions: Arc<PermissionManager>,
}

impl Daemon {
    pub fn new(config: Config) -> Self {
        let orchestrator = Orchestrator::with_config(config.clone());
        let memory = MemoryManager::new(config.clone()).ok();
        let permissions = PermissionManager::new(&config).unwrap_or_else(|_| {
            // Create a dummy if it fails
            PermissionManager::new(&config).expect("Failed to init permissions")
        });
        Self {
            orchestrator: Arc::new(Mutex::new(orchestrator)),
            config: config.clone(),
            memory,
            permissions: Arc::new(permissions),
        }
    }

    pub async fn ask(&self, prompt: &str) -> Result<String> {
        let orch = self.orchestrator.lock().await;
        let session_id = orch.session_id().to_string();
        let result = orch.run(prompt).await?;
        // Auto-title if this is a new session with few messages
        if let Some(ref mem) = self.memory {
            if let Ok(count) = mem.get_message_count(&session_id) {
                if count == 2 {
                    // First exchange (user + assistant) - generate title from first user message
                    if let Ok(messages) = mem.get_recent_messages(&session_id, 2) {
                        if let Some(first) = messages.first() {
                            if first.role == "user" {
                                let title = first
                                    .content
                                    .chars()
                                    .take(40)
                                    .collect::<String>()
                                    .trim()
                                    .to_string();
                                let title = if title.is_empty() {
                                    "New Session".to_string()
                                } else {
                                    title
                                };
                                let _ = mem.update_session_title(&session_id, &title);
                            } else {
                                let title = prompt
                                    .chars()
                                    .take(40)
                                    .collect::<String>()
                                    .trim()
                                    .to_string();
                                let title = if title.is_empty() {
                                    "New Session".to_string()
                                } else {
                                    title
                                };
                                let _ = mem.update_session_title(&session_id, &title);
                            }
                        }
                    }
                }
            }
        }
        Ok(result)
    }

    pub async fn set_session(&self, session_id: &str) -> Result<()> {
        let mut orch = self.orchestrator.lock().await;
        orch.set_session(session_id.to_string());
        if let Some(ref mem) = self.memory {
            let _ = mem.create_session(session_id);
        }
        self.save_active_session(session_id)?;
        Ok(())
    }

    pub async fn new_session(&self) -> Result<String> {
        let session_id = Uuid::new_v4().to_string();
        let mut orch = self.orchestrator.lock().await;
        orch.set_session(session_id.clone());
        if let Some(ref mem) = self.memory {
            let _ = mem.create_session(&session_id);
        }
        self.save_active_session(&session_id)?;
        Ok(session_id)
    }

    pub async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        if let Some(ref mem) = self.memory {
            mem.list_sessions(limit)
        } else {
            Ok(vec![])
        }
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        if let Some(ref mem) = self.memory {
            mem.delete_session(session_id)?;
        }
        Ok(())
    }

    pub async fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        if let Some(ref mem) = self.memory {
            mem.update_session_title(session_id, title)?;
        }
        Ok(())
    }

    pub async fn get_session_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::memory::Message>> {
        if let Some(ref mem) = self.memory {
            mem.get_all_messages(session_id)
        } else {
            Ok(vec![])
        }
    }

    pub async fn prune_empty_sessions(&self) -> Result<usize> {
        if let Some(ref mem) = self.memory {
            mem.prune_empty_sessions()
        } else {
            Ok(0)
        }
    }

    pub fn list_tools(&self) -> Vec<String> {
        vec![
            "filesystem.list_dir".to_string(),
            "filesystem.read".to_string(),
            "filesystem.write".to_string(),
            "filesystem.edit".to_string(),
            "shell.run".to_string(),
            "system.info".to_string(),
        ]
    }

    pub async fn session_id(&self) -> String {
        let orch = self.orchestrator.lock().await;
        orch.session_id().to_string()
    }

    fn get_active_session_path(&self) -> PathBuf {
        self.config.general.data_dir.join("active_session")
    }

    pub fn save_active_session(&self, session_id: &str) -> Result<()> {
        std::fs::create_dir_all(&self.config.general.data_dir)?;
        std::fs::write(self.get_active_session_path(), session_id).map_err(JarvisError::Io)?;
        Ok(())
    }

    pub fn load_active_session(&self) -> Option<String> {
        if let Ok(content) = std::fs::read_to_string(self.get_active_session_path()) {
            let trimmed = content.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        None
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
