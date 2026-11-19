//! One conversation's own state: which conversation it is, its model and
//! tier, its last turn. The orchestrator keeps only what every conversation
//! shares, and each turn is told which conversation it belongs to - so
//! nothing one window does can change another (Milestone 6.5a).

use std::collections::HashSet;
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
