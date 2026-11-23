//! Jarvis Daemon

use crate::cancel::Cancellation;
use crate::config::Config;
use crate::conversation::{Conversation, Conversations};
use crate::error::{JarvisError, Result};
use crate::memory::{MemoryManager, MemoryStats, RecallResult, SessionInfo};
use crate::orchestrator::{ConfirmationHandler, Orchestrator};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::info;

#[derive(Clone)]
pub struct Daemon {
    orchestrator: Arc<Orchestrator>,
    /// Every conversation, by id. Each request names the one it is for.
    conversations: Arc<Conversations>,
    config: Config,
    memory: Option<MemoryManager>,
}

impl Daemon {
    pub fn new(config: Config) -> Result<Self> {
        let orchestrator = Orchestrator::with_config(config.clone())?;
        Self::with_orchestrator(config, orchestrator)
    }

    /// A daemon around `orchestrator` - real providers from [`Daemon::new`],
    /// scripted ones in tests. Fails without its memory database: with no
    /// conversations there is nothing to serve.
    pub fn with_orchestrator(config: Config, orchestrator: Orchestrator) -> Result<Self> {
        let memory = MemoryManager::new(config.clone())?;
        let default_mode = orchestrator.default_mode();
        Ok(Self {
            orchestrator: Arc::new(orchestrator),
            conversations: Arc::new(
                Conversations::new(memory.clone()).with_default_mode(default_mode),
            ),
            config,
            memory: Some(memory),
        })
    }

    /// The conversation `session` names, or an error naming it.
    pub fn conversation(&self, session: &str) -> Result<Arc<Conversation>> {
        self.conversations.get(session)
    }

    /// A turn's answer and the footer line shown under it.
    pub async fn ask_with_footer(
        &self,
        session: &str,
        prompt: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        progress: Option<&UnboundedSender<String>>,
    ) -> Result<(String, String, crate::orchestrator::TurnTiming)> {
        self.ask_steered(session, prompt, confirm, cancel, sink, progress, None)
            .await
            .0
    }

    /// A turn, and what became of the steers sent into it: unused, and the
    /// ids of those used. Collected while the turn still holds the
    /// conversation, so another window's next turn cannot take them
    /// (6.7a review).
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    pub async fn ask_steered(
        &self,
        session: &str,
        prompt: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        progress: Option<&UnboundedSender<String>>,
        steps: Option<&UnboundedSender<jarvis_common::ipc::StepEvent>>,
    ) -> (
        Result<(String, String, crate::orchestrator::TurnTiming)>,
        (Vec<crate::conversation::Steer>, Vec<String>),
        Vec<jarvis_common::ipc::FileChange>,
    ) {
        let since = chrono::Utc::now();
        let conv = match self.conversation(session) {
            Ok(conv) => conv,
            Err(e) => return (Err(e), Default::default(), Vec::new()),
        };
        let Some(_answering) = conv.begin_turn() else {
            return (
                Err(JarvisError::Busy(
                    "This conversation is answering another question. Wait for it, or open another with /new.".into(),
                )),
                Default::default(),
                Vec::new(),
            );
        };
        let result = self
            .answer(&conv, prompt, confirm, cancel, sink, progress, steps)
            .await;
        let outcome = (conv.take_steers(), conv.take_used_steers());
        let changes = self.orchestrator.turn_changes(&conv.id, since);
        (result, outcome, changes)
    }

    #[allow(clippy::too_many_arguments)]
    async fn answer(
        &self,
        conv: &Arc<Conversation>,
        prompt: &str,
        confirm: Option<&dyn ConfirmationHandler>,
        cancel: Option<&Cancellation>,
        sink: Option<&UnboundedSender<String>>,
        progress: Option<&UnboundedSender<String>>,
        steps: Option<&UnboundedSender<jarvis_common::ipc::StepEvent>>,
    ) -> Result<(String, String, crate::orchestrator::TurnTiming)> {
        let conv = conv.clone();
        let orch = &self.orchestrator;
        let session_id = conv.id.clone();
        self.conversations.used(&conv);
        let (result, footer, timing) = orch
            .run_with_footer(&conv, prompt, confirm, cancel, sink, progress, steps)
            .await?;
        // Title a conversation that has none yet, from its first question,
        // once it has an answer. Not "exactly two messages": a steer (6.7a)
        // adds a user message mid-turn.
        if let Some(ref mem) = self.memory {
            let untitled = mem
                .session_title(&session_id)
                .ok()
                .flatten()
                .is_none_or(|t| t == "New Session");
            if untitled {
                if let Ok(messages) = mem.get_all_messages(&session_id) {
                    let answered = messages.iter().any(|m| m.role == "assistant");
                    if let Some(first) = messages.iter().find(|m| m.role == "user") {
                        let title: String = first.content.chars().take(40).collect();
                        let title = title.trim();
                        if answered && !title.is_empty() {
                            let _ = mem.update_session_title(&session_id, title);
                        }
                    }
                }
            }
        }
        Ok((result, footer, timing))
    }

    /// The conversation requests go to now.
    /// A new conversation, on the model used last: `(id, model, remote)`.
    pub async fn new_session(&self) -> Result<(String, String, bool)> {
        let orch = &self.orchestrator;
        let conv = self.conversations.create(orch);
        let (_, model, _) = orch.model_info(&conv);
        Ok((conv.id.clone(), model, conv.remote()))
    }

    /// `/approve`: run, list or drop what Autopilot left in a conversation.
    /// It takes the conversation's turn, like a question does. Returns the
    /// text and how many are still waiting.
    pub async fn approve(
        &self,
        session: &str,
        which: crate::orchestrator::Approve,
    ) -> Result<(String, usize)> {
        let conv = self.conversation(session)?;
        let Some(_answering) = conv.begin_turn() else {
            return Err(JarvisError::Busy(
                "This conversation is answering another question. Wait for it, or open another with /new.".into(),
            ));
        };
        let text = self.orchestrator.approve(&conv, which).await?;
        Ok((text, conv.deferred().len()))
    }

    /// A steer for the conversation's running turn: true if it took it.
    pub fn steer(&self, session: &str, id: &str, text: &str) -> Result<bool> {
        Ok(self
            .conversation(session)?
            .steer(crate::conversation::Steer {
                id: id.to_string(),
                text: text.to_string(),
            }))
    }

    /// A conversation's mode.
    pub fn mode_of(&self, session: &str) -> Result<crate::modes::Mode> {
        Ok(self.conversation(session)?.mode())
    }

    /// Whether commands can run in the sandbox here.
    pub fn sandbox_ready(&self) -> bool {
        crate::sandbox::ready(&self.config)
    }

    /// The mode a new conversation starts in, for `status`.
    pub fn default_mode(&self) -> crate::modes::Mode {
        self.orchestrator.default_mode()
    }

    /// Change part of a conversation's mode: any of `autonomy`, `approval`
    /// and `sandbox` in `change`. Its next turn uses it.
    pub fn set_mode(
        &self,
        session: &str,
        change: &serde_json::Value,
    ) -> Result<crate::modes::Mode> {
        let conv = self.conversation(session)?;
        let mut mode = conv.mode();
        let bad = |what: &str, value: &serde_json::Value| {
            JarvisError::Config(format!("{value} is not a {what}"))
        };
        if let Some(v) = change.get("autonomy") {
            mode.autonomy = serde_json::from_value(v.clone())
                .map_err(|_| bad("autonomy: plan, interactive or autopilot", v))?;
        }
        if let Some(v) = change.get("approval") {
            mode.approval = serde_json::from_value(v.clone())
                .map_err(|_| bad("approval: manual, assisted or allow_all", v))?;
        }
        if let Some(v) = change.get("sandbox") {
            mode.sandbox = v
                .as_bool()
                .ok_or_else(|| bad("sandbox setting: true or false", v))?;
        }
        conv.set_mode(mode);
        self.conversations.saved(&conv);
        Ok(mode)
    }

    /// The conversation used most recently, for `--continue`.
    pub fn last_session(&self) -> Option<String> {
        self.conversations.last()
    }

    pub async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        if let Some(ref mem) = self.memory {
            mem.list_sessions(limit)
        } else {
            Ok(vec![])
        }
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        if self
            .conversation(session_id)
            .is_ok_and(|conv| conv.answering())
        {
            return Err(JarvisError::Busy(format!(
                "Conversation {session_id} is answering a question; delete it after."
            )));
        }
        if let Some(ref mem) = self.memory {
            mem.delete_session(session_id)?;
        }
        // A window still showing it gets "no conversation" on its next
        // request, not a silently recreated one.
        self.conversations.forget(session_id);
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
            mem.prune_empty_sessions(&self.conversations.open())
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

    pub async fn list_tools(&self) -> Vec<String> {
        let orch = &self.orchestrator;
        orch.list_tools()
    }

    /// `(provider, model, base_url)` a conversation's turns go to.
    pub async fn model_info(&self, session: &str) -> Result<(String, String, String)> {
        let conv = self.conversation(session)?;
        let orch = &self.orchestrator;
        let (provider, model, url) = orch.model_info(&conv);
        Ok((provider.to_string(), model, url))
    }

    /// What a new conversation would start with, for `status`.
    pub async fn default_model(&self) -> (String, String, String) {
        let orch = &self.orchestrator;
        let preferred = self.conversations.preferred(&self.orchestrator);
        let (provider, model, url) = orch.model_info(&preferred);
        (provider.to_string(), model, url)
    }

    /// Switch a conversation's tier; only that conversation changes.
    pub async fn set_remote(&self, session: &str, on: bool) -> Result<()> {
        let conv = self.conversation(session)?;
        self.orchestrator.set_remote(&conv, on)?;
        self.conversations.saved(&conv);
        Ok(())
    }

    /// `/bad` or `/good` about a conversation's last turn.
    pub async fn feedback(&self, session: &str, kind: &str, note: &str) -> Result<()> {
        let conv = self.conversation(session)?;
        self.orchestrator.feedback(&conv, kind, note)
    }

    /// Load a conversation's local model in the background (a window
    /// opening it is about to ask).
    pub async fn warm_up(&self, session: &str) {
        if let Ok(conv) = self.conversation(session) {
            self.orchestrator.warm_up(&conv);
        }
    }

    /// Load the model a new conversation would use (on start).
    pub async fn warm_up_default(&self) {
        let preferred = self.conversations.preferred(&self.orchestrator);
        self.orchestrator.warm_up(&preferred);
    }

    /// The models a conversation can switch to.
    pub async fn models(&self, session: &str) -> Result<Vec<crate::orchestrator::ModelChoice>> {
        let conv = self.conversation(session)?;
        Ok(self.orchestrator.models(&conv))
    }

    /// Switch a conversation's model; only that conversation changes, and
    /// new conversations start with it.
    pub async fn set_model(
        &self,
        session: &str,
        query: &str,
    ) -> Result<crate::orchestrator::ModelChoice> {
        let conv = self.conversation(session)?;
        let chosen = self.orchestrator.set_model(&conv, query)?;
        self.conversations.saved(&conv);
        Ok(chosen)
    }

    /// What a conversation trusts (`[t]`).
    pub async fn trusted(&self, session: &str) -> Result<Vec<String>> {
        let conv = self.conversation(session)?;
        Ok(self.orchestrator.trusted(&conv))
    }

    pub async fn clear_trust(&self, session: &str) -> Result<()> {
        let conv = self.conversation(session)?;
        self.orchestrator.clear_trust(&conv);
        Ok(())
    }

    pub async fn uses_remote(&self, session: &str) -> Result<bool> {
        let conv = self.conversation(session)?;
        Ok(self.orchestrator.uses_remote(&conv))
    }

    /// The privacy notice for a conversation's tier.
    pub async fn privacy(&self, session: &str) -> Result<String> {
        let conv = self.conversation(session)?;
        let orch = &self.orchestrator;
        Ok(orch.destination(&conv).notice().to_string())
    }

    /// Whether a new conversation starts on the remote tier, for `status`.
    pub fn default_remote(&self) -> bool {
        self.conversations.preferred(&self.orchestrator).remote()
    }

    /// The footer under a stored answer: the model that wrote it and the
    /// tier that model runs on (as configured now).
    pub fn answer_footer(&self, model: &str) -> String {
        let remote = self.config.remote.as_ref().is_some_and(|r| {
            r.model.default_model == model || r.model.models.iter().any(|m| m == model)
        });
        format!("{model} · {}", if remote { "remote" } else { "local" })
    }

    /// The privacy notice for a new conversation's tier, for `status`.
    pub async fn default_privacy(&self) -> String {
        let preferred = self.conversations.preferred(&self.orchestrator);
        let orch = &self.orchestrator;
        orch.destination(&preferred).notice().to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_config(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.general.data_dir = dir.to_path_buf();
        config.memory.db_path = dir.join("memory.db");
        config.memory.recall_enabled = false;
        // Nothing listens here: no test loads a model on the owner's GPU.
        config.model.base_url = "http://127.0.0.1:9".into();
        config.model.models = vec!["qwen2.5:3b".into()];
        config
    }

    #[tokio::test]
    async fn a_new_conversation_starts_on_the_model_used_last() {
        let dir = std::env::temp_dir().join(format!("jarvis-daemon-test-{}", Uuid::new_v4()));
        let daemon = Daemon::new(test_config(&dir)).expect("daemon");
        let (first, _, _) = daemon.new_session().await.unwrap();
        daemon.set_model(&first, "qwen").await.unwrap();
        let (_, model, remote) = daemon.new_session().await.unwrap();
        assert_eq!(model, "qwen2.5:3b");
        assert!(!remote);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_last_session_is_the_one_last_asked_in_and_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("jarvis-daemon-test-{}", Uuid::new_v4()));
        let daemon = Daemon::new(test_config(&dir)).expect("daemon");
        assert_eq!(daemon.last_session(), None, "a fresh install has none");
        let (a, _, _) = daemon.new_session().await.unwrap();
        let (b, _, _) = daemon.new_session().await.unwrap();
        // The model cannot be reached; the turn still happened in `b`.
        let _ = daemon
            .ask_with_footer(&b, "hello", None, None, None, None)
            .await;
        assert_eq!(daemon.last_session().as_deref(), Some(b.as_str()));
        let again = Daemon::new(test_config(&dir)).expect("daemon");
        assert_eq!(again.last_session().as_deref(), Some(b.as_str()));
        let _ = a;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_deleted_conversation_is_never_the_last_one() {
        let dir = std::env::temp_dir().join(format!("jarvis-daemon-test-{}", Uuid::new_v4()));
        let daemon = Daemon::new(test_config(&dir)).expect("daemon");
        let (a, _, _) = daemon.new_session().await.unwrap();
        let _ = daemon
            .ask_with_footer(&a, "hello", None, None, None, None)
            .await;
        daemon.delete_session(&a).await.unwrap();
        assert_eq!(daemon.last_session(), None);
        assert!(daemon.conversation(&a).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
