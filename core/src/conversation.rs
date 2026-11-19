//! One conversation's own state: which conversation it is, its model and
//! tier, its last turn. The orchestrator keeps only what every conversation
//! shares, and each turn is told which conversation it belongs to - so
//! nothing one window does can change another (Milestone 6.5a).

use crate::error::{JarvisError, Result};
use crate::memory::MemoryManager;
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
        })
    }

    pub fn remote(&self) -> bool {
        self.remote.load(Ordering::Relaxed)
    }

    pub(crate) fn set_remote(&self, on: bool) {
        self.remote.store(on, Ordering::Relaxed);
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
    loaded: Mutex<HashMap<String, Arc<Conversation>>>,
}

/// Settings keys: the model and tier used most recently, and the
/// conversation used most recently (`jarvis up --continue`).
const LAST_MODEL: &str = "last_model";
const LAST_REMOTE: &str = "last_remote";
const LAST_SESSION: &str = "last_session";

impl Conversations {
    pub fn new(memory: MemoryManager) -> Self {
        Conversations {
            memory,
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// The conversation `id`, or an error naming it if there is none. A
    /// deleted conversation is never recreated by asking for it.
    pub fn get(&self, id: &str) -> Result<Arc<Conversation>> {
        if let Some(conv) = self.loaded.lock().unwrap().get(id) {
            return Ok(conv.clone());
        }
        let Some((model, remote)) = self.memory.session_settings(id)? else {
            return Err(JarvisError::Config(format!(
                "There is no conversation {id}."
            )));
        };
        let mut chosen = [None, None];
        chosen[remote as usize] = model;
        let conv = Conversation::restore(id.to_string(), remote, chosen);
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

    /// A new conversation, on the model and tier used most recently.
    pub fn create(&self, orch: &Orchestrator) -> Arc<Conversation> {
        let fresh = orch.new_conversation();
        let model = self.memory.setting(LAST_MODEL).ok().flatten();
        let remote = match self.memory.setting(LAST_REMOTE).ok().flatten() {
            Some(flag) => flag == "1",
            None => fresh.remote(),
        };
        let mut chosen = [None, None];
        chosen[remote as usize] = model;
        let conv = Conversation::restore(fresh.id.clone(), remote, chosen);
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
        let remote = conv.remote();
        let model = conv.chosen()[remote as usize].clone();
        let _ = self
            .memory
            .save_session_settings(&conv.id, model.as_deref(), remote);
        if let Some(model) = &model {
            let _ = self.memory.set_setting(LAST_MODEL, model);
        }
        let _ = self
            .memory
            .set_setting(LAST_REMOTE, if remote { "1" } else { "0" });
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
    pub fn preferred(&self) -> Arc<Conversation> {
        let model = self.memory.setting(LAST_MODEL).ok().flatten();
        let remote = self.memory.setting(LAST_REMOTE).ok().flatten().as_deref() == Some("1");
        let mut chosen = [None, None];
        chosen[remote as usize] = model;
        Conversation::restore(String::new(), remote, chosen)
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
