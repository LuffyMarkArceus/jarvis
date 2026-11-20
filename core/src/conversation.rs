//! One conversation's own state: which conversation it is, its model and
//! tier, its last turn. The orchestrator keeps only what every conversation
//! shares, and each turn is told which conversation it belongs to - so
//! nothing one window does can change another (Milestone 6.5a).

use crate::error::{JarvisError, Result};
use crate::memory::{ConversationSettings, MemoryManager};
use crate::modes::Mode;
use crate::orchestrator::Orchestrator;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// The turn `/bad` and a rephrase are judged against.
pub(crate) struct LastTurn {
    pub(crate) id: String,
    pub(crate) ended: Instant,
    pub(crate) words: HashSet<String>,
}

/// One conversation. Cheap to share (`Arc`); its settings change through
/// `&self`, so a turn and a model switch need no exclusive access.
pub struct Conversation {
    /// The `sessions` row its messages are stored under.
    pub id: String,
    /// Its turns go to the `[remote]` tier.
    remote: AtomicBool,
    /// The model chosen on each tier, `[local, remote]`; `None` is the
    /// tier's `default_model`.
    chosen: Mutex<[Option<String>; 2]>,
    /// Its last logged turn, for `/bad` and spotting a rephrase.
    pub(crate) last_turn: Mutex<Option<LastTurn>>,
    /// A turn is running: a conversation answers one question at a time,
    /// or two windows on it would interleave its history (6.5b).
    answering: AtomicBool,
    /// Autonomy, approval and the sandbox (Milestone 6.6).
    mode: Mutex<Mode>,
}

/// A conversation's settings at one moment: a turn takes them when it
/// starts and keeps them, so a switch made meanwhile applies from the next
/// turn (6.5b).
#[derive(Debug, Clone, Default)]
pub struct Pinned {
    pub remote: bool,
    pub chosen: [Option<String>; 2],
    /// Its mode (Milestone 6.6).
    pub mode: Mode,
}

/// Held while a conversation answers; dropping it (however the turn ends)
/// lets it answer again.
pub struct Answering(Arc<Conversation>);

impl Drop for Answering {
    fn drop(&mut self) {
        self.0.answering.store(false, Ordering::Release);
    }
}

impl Conversation {
    pub fn new(id: String, remote: bool) -> Arc<Conversation> {
        Self::restore(id, remote, [None, None])
    }

    /// A conversation as stored: its tier and the model chosen on each tier.
    pub fn restore(id: String, remote: bool, chosen: [Option<String>; 2]) -> Arc<Conversation> {
        Arc::new(Conversation {
            id,
            remote: AtomicBool::new(remote),
            chosen: Mutex::new(chosen),
            last_turn: Mutex::new(None),
            answering: AtomicBool::new(false),
            mode: Mutex::new(Mode::default()),
        })
    }

    /// Start a turn, or `None` if one is already running.
    pub fn begin_turn(self: &Arc<Self>) -> Option<Answering> {
        self.answering
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then(|| Answering(self.clone()))
    }

    pub fn answering(&self) -> bool {
        self.answering.load(Ordering::Acquire)
    }

    pub fn remote(&self) -> bool {
        self.remote.load(Ordering::Relaxed)
    }

    pub(crate) fn set_remote(&self, on: bool) {
        self.remote.store(on, Ordering::Relaxed);
    }

    /// Its settings now.
    pub fn pin(&self) -> Pinned {
        Pinned {
            remote: self.remote(),
            chosen: self.chosen(),
            mode: self.mode(),
        }
    }

    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    pub fn set_mode(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub fn chosen(&self) -> [Option<String>; 2] {
        self.chosen.lock().unwrap().clone()
    }

    pub(crate) fn choose(&self, remote: bool, model: String) {
        self.chosen.lock().unwrap()[remote as usize] = Some(model);
    }
}

/// The daemon's conversations: found by id, loaded from `memory.db` when
/// not yet in memory, created starting from the model used last, and saved
/// when switched.
pub struct Conversations {
    memory: MemoryManager,
    /// For a conversation stored before modes: `[modes]` in the config.
    default_mode: Mutex<Mode>,
    loaded: Mutex<HashMap<String, Arc<Conversation>>>,
}

/// Settings keys: the model and tier used most recently, and the
/// conversation used most recently (`jarvis up --continue`).
const LAST_LOCAL_MODEL: &str = "last_local_model";
const LAST_REMOTE_MODEL: &str = "last_remote_model";
const LAST_SESSION: &str = "last_session";

impl Conversations {
    pub fn new(memory: MemoryManager) -> Self {
        Conversations {
            memory,
            default_mode: Mutex::new(Mode::default()),
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// Conversations stored before modes existed get `mode`.
    pub fn with_default_mode(self, mode: Mode) -> Self {
        *self.default_mode.lock().unwrap() = mode;
        self
    }

    /// The conversation `id`, or an error naming it if there is none. A
    /// deleted conversation is never recreated by asking for it.
    pub fn get(&self, id: &str) -> Result<Arc<Conversation>> {
        if let Some(conv) = self.loaded.lock().unwrap().get(id) {
            return Ok(conv.clone());
        }
        let Some(stored) = self.memory.session_settings(id)? else {
            return Err(JarvisError::Config(format!(
                "There is no conversation {id}."
            )));
        };
        let conv = Conversation::restore(
            id.to_string(),
            stored.remote,
            [stored.local, stored.remote_model],
        );
        conv.set_mode(stored.mode.unwrap_or(*self.default_mode.lock().unwrap()));
        // Whoever loaded it first wins: two windows on one conversation
        // must share one `Conversation`, or a model switch in one would miss
        // the other.
        Ok(self
            .loaded
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(conv)
            .clone())
    }

    /// A new conversation: on the local tier unless `[remote] default` is
    /// set (owner, 2026-11-19), with the model used last on each tier.
    pub fn create(&self, orch: &Orchestrator) -> Arc<Conversation> {
        let fresh = orch.new_conversation();
        let conv = Conversation::restore(fresh.id.clone(), fresh.remote(), self.last_models());
        conv.set_mode(orch.default_mode());
        self.saved(&conv);
        self.loaded
            .lock()
            .unwrap()
            .insert(conv.id.clone(), conv.clone());
        conv
    }

    /// Store `conv`'s model and tier, and make them the ones new
    /// conversations start with.
    pub fn saved(&self, conv: &Conversation) {
        let [local, remote_model] = conv.chosen();
        let _ = self.memory.save_session_settings(
            &conv.id,
            &ConversationSettings {
                local: local.clone(),
                remote_model: remote_model.clone(),
                remote: conv.remote(),
                mode: Some(conv.mode()),
            },
        );
        for (key, model) in [(LAST_LOCAL_MODEL, local), (LAST_REMOTE_MODEL, remote_model)] {
            if let Some(model) = model {
                let _ = self.memory.set_setting(key, &model);
            }
        }
    }

    /// The model used last on each tier, `[local, remote]`.
    fn last_models(&self) -> [Option<String>; 2] {
        [LAST_LOCAL_MODEL, LAST_REMOTE_MODEL].map(|key| self.memory.setting(key).ok().flatten())
    }

    /// `conv` was just used: `--continue` opens it.
    pub fn used(&self, conv: &Conversation) {
        let _ = self.memory.set_setting(LAST_SESSION, &conv.id);
    }

    /// The conversation used most recently, if it still exists.
    pub fn last(&self) -> Option<String> {
        let id = self.memory.setting(LAST_SESSION).ok().flatten()?;
        self.memory
            .session_exists(&id)
            .unwrap_or(false)
            .then_some(id)
    }

    /// The settings a new conversation would start with - for `status` and
    /// for loading that model on start. Not a conversation: never stored,
    /// never sent a turn.
    pub fn preferred(&self, orch: &Orchestrator) -> Arc<Conversation> {
        let conv = Conversation::restore(String::new(), orch.starts_remote(), self.last_models());
        conv.set_mode(orch.default_mode());
        conv
    }

    /// The conversations in use since the daemon started: `/prune` leaves
    /// them alone, since a window just opened is an empty one.
    pub fn open(&self) -> Vec<String> {
        self.loaded.lock().unwrap().keys().cloned().collect()
    }

    /// Forget a deleted conversation.
    pub fn forget(&self, id: &str) {
        self.loaded.lock().unwrap().remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_conversation_starts_on_the_tier_it_was_given() {
        let c = Conversation::new("a".into(), true);
        assert_eq!(c.id, "a");
        assert!(c.remote());
        assert_eq!(c.chosen(), [None, None]);
    }
}
