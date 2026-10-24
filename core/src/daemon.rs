//! Jarvis Daemon

use crate::cancel::Cancellation;
use crate::config::Config;
use crate::error::{JarvisError, Result};
use crate::memory::{MemoryManager, MemoryStats, RecallResult, SessionInfo};
use crate::orchestrator::{ConfirmationHandler, Orchestrator};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use tracing::info;
use uuid::Uuid;

#[derive(Clone)]
pub struct Daemon {
    orchestrator: Arc<Mutex<Orchestrator>>,
    config: Config,
    memory: Option<MemoryManager>,
}

impl Daemon {
    pub fn new(config: Config) -> Self {
        let memory = MemoryManager::new(config.clone()).ok();
        let mut orchestrator = Orchestrator::with_config(config.clone());

        // A restart should resume the conversation the user was having. Only
        // trust an id that still exists in the database, though: pruning or
        // `session delete` can remove a session and leave this file behind.
        if let Some(saved) = read_active_session(&config.general.data_dir) {
            let known = memory
                .as_ref()
                .map(|m| m.session_exists(&saved).unwrap_or(false))
                .unwrap_or(false);
            if known {
                info!("Resuming session {}", saved);
                orchestrator.set_session(saved);
            } else {
                info!("Ignoring active session {}: not in the database", saved);
            }
        }

        Self {
            orchestrator: Arc::new(Mutex::new(orchestrator)),
            config: config.clone(),
            memory,
        }
    }

    /// Handle one turn with no way to reach a human (headless callers).
    pub async fn ask(&self, prompt: &str) -> Result<String> {
        self.ask_with(prompt, None).await
    }

    /// Handle one turn, asking `confirm` before an `ask`-policy tool runs.
    pub async fn ask_with(
        &self,
        prompt: &str,
        confirm: Option<&dyn ConfirmationHandler>,
    ) -> Result<String> {
        self.ask_streaming(prompt, confirm, None, None).await
    }

    /// Handle one turn with cancellation and optional token streaming.
    ///
    /// The orchestrator lock is held for the whole turn (turns are
    /// serialised globally), which is exactly why cancellation cannot travel
    /// through this object: it is delivered by the transport-side registry
    /// instead.
    pub async fn ask_streaming(
        &self,
        prompt: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
    ) -> Result<String> {
        let orch = self.orchestrator.lock().await;
        let session_id = orch.session_id().to_string();
        let result = orch.run_with_options(prompt, confirm, cancel, sink).await?;
        // Auto-title if this is a new session with few messages
        if let Some(ref mem) = self.memory {
            if let Ok(count) = mem.get_message_count(&session_id) {
                if count == 2 {
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
        // Deleting the session in use would leave the daemon writing new
        // messages into an id that `sessions` no longer contains, and the
        // next restart would try to resume a conversation that is gone.
        if self.session_id().await == session_id {
            self.new_session().await?;
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

    pub async fn recall_memory(&self, query: &str, limit: usize) -> Result<Vec<RecallResult>> {
        if let Some(ref mem) = self.memory {
            mem.recall(query, limit)
        } else {
            Ok(vec![])
        }
    }

    pub async fn backfill_memory(&self) -> Result<usize> {
        if let Some(ref mem) = self.memory {
            mem.backfill_fts()
        } else {
            Ok(0)
        }
    }

    pub async fn memory_stats(&self) -> Result<MemoryStats> {
        if let Some(ref mem) = self.memory {
            mem.stats()
        } else {
            Ok(MemoryStats {
                total_messages: 0,
                total_sessions: 0,
                db_size_bytes: 0,
            })
        }
    }

    pub fn list_tools(&self) -> Vec<String> {
        crate::tools::known_tool_names()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    pub async fn session_id(&self) -> String {
        let orch = self.orchestrator.lock().await;
        orch.session_id().to_string()
    }

    fn get_active_session_path(&self) -> PathBuf {
        active_session_path(&self.config.general.data_dir)
    }

    pub fn save_active_session(&self, session_id: &str) -> Result<()> {
        std::fs::create_dir_all(&self.config.general.data_dir)?;
        std::fs::write(self.get_active_session_path(), session_id).map_err(JarvisError::Io)?;
        Ok(())
    }

    pub fn load_active_session(&self) -> Option<String> {
        read_active_session(&self.config.general.data_dir)
    }

    pub fn get_socket_path(&self) -> PathBuf {
        self.config.general.data_dir.join("jarvis.sock")
    }

    pub fn get_pid_path(&self) -> PathBuf {
        self.config.general.data_dir.join("jarvis.pid")
    }

    pub fn get_log_path(&self) -> PathBuf {
        self.config.general.data_dir.join("jarvis.log")
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

fn active_session_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("active_session")
}

/// Read the session id saved by the last `session.new` / `session.switch`.
///
/// Returns `None` for a missing, empty, or whitespace-only file; a corrupt
/// file must not turn into a session id containing newlines.
fn read_active_session(data_dir: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(active_session_path(data_dir)).ok()?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.general.data_dir = dir.to_path_buf();
        config.memory.db_path = dir.join("memory.db");
        config
    }

    #[tokio::test]
    async fn restart_resumes_the_saved_session() {
        let dir = std::env::temp_dir().join(format!("jarvis-daemon-test-{}", Uuid::new_v4()));
        let config = test_config(&dir);

        let first = Daemon::new(config.clone());
        let saved = first.new_session().await.expect("new session");
        first
            .memory
            .as_ref()
            .expect("memory")
            .add_message(&saved, "user", "remember this")
            .expect("add message");

        let second = Daemon::new(config);
        assert_eq!(second.session_id().await, saved);
        assert_eq!(
            second.load_active_session().as_deref(),
            Some(saved.as_str())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_saved_session_missing_from_the_database_is_ignored() {
        let dir = std::env::temp_dir().join(format!("jarvis-daemon-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(active_session_path(&dir), Uuid::new_v4().to_string()).expect("save id");

        let daemon = Daemon::new(test_config(&dir));
        assert_ne!(
            Some(daemon.session_id().await),
            daemon.load_active_session(),
            "a stale id must not become the running session"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deleting_the_current_session_starts_a_new_one() {
        let dir = std::env::temp_dir().join(format!("jarvis-daemon-test-{}", Uuid::new_v4()));
        let daemon = Daemon::new(test_config(&dir));

        let current = daemon.session_id().await;
        daemon.delete_session(&current).await.expect("delete");

        let after = daemon.session_id().await;
        assert_ne!(after, current);
        assert_eq!(
            daemon.load_active_session().as_deref(),
            Some(after.as_str())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
