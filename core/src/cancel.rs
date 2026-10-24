//! Cooperative cancellation for one turn.
//!
//! A turn is cancelled by id: the transport (`ipc::server`) registers a
//! [`TurnRegistry`] entry when it starts handling an `ask`, and a client can
//! cancel that id even while the connection is busy writing chunks. The
//! orchestrator and model client observe a [`Cancellation`] and stop at the
//! next await point.
//!
//! Cancellation is cooperative: a tool that is already running is allowed to
//! finish. Turning a half-executed command into a "cancelled" report would be
//! a lie about what happened, and killing children safely needs its own
//! handling.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use uuid::Uuid;

/// A signal that the current turn should stop.
///
/// Cloning is cheap and every clone observes the same signal.
#[derive(Clone, Debug, Default)]
pub struct Cancellation {
    receiver: Option<watch::Receiver<bool>>,
}

impl Cancellation {
    /// A signal that is never set. Useful for callers that do not support
    /// cancellation so the turn loop does not need two shapes.
    pub fn never() -> Self {
        Self { receiver: None }
    }

    pub fn is_cancelled(&self) -> bool {
        self.receiver
            .as_ref()
            .map(|receiver| *receiver.borrow())
            .unwrap_or(false)
    }

    /// Wait until cancelled. Returns immediately when already cancelled and
    /// never returns for [`Cancellation::never`].
    ///
    /// Safe to call repeatedly: once set, every call returns at once.
    pub async fn cancelled(&mut self) {
        let Some(receiver) = self.receiver.as_mut() else {
            std::future::pending::<()>().await;
            return;
        };
        if *receiver.borrow() {
            return;
        }
        // The sender is dropped when the turn ends; that means the turn is
        // over, so there is nothing left to cancel.
        let _ = receiver.changed().await;
    }
}

/// The set of turns that can currently be cancelled.
///
/// Shared by every connection: the connection that is busy running a turn
/// cannot read a cancel message, so a second connection (or a control
/// message on another one) has to be able to reach it.
#[derive(Clone, Default)]
pub struct TurnRegistry {
    turns: Arc<Mutex<HashMap<Uuid, watch::Sender<bool>>>>,
}

impl TurnRegistry {
    /// Register a turn and get its signal. The entry is removed when the
    /// returned guard is dropped, including on error.
    pub fn begin(&self, id: Uuid) -> (Cancellation, TurnGuard) {
        let (sender, receiver) = watch::channel(false);
        self.turns
            .lock()
            .expect("turn registry poisoned")
            .insert(id, sender);
        (
            Cancellation {
                receiver: Some(receiver),
            },
            TurnGuard {
                id,
                registry: self.clone(),
            },
        )
    }

    /// Cancel a turn by the id of the request that started it.
    ///
    /// Unknown ids are ignored: the turn may have finished between the
    /// client deciding to cancel and the message arriving.
    pub fn cancel(&self, id: Uuid) -> bool {
        let turns = self.turns.lock().expect("turn registry poisoned");
        match turns.get(&id) {
            Some(sender) => sender.send(true).is_ok(),
            None => false,
        }
    }

    fn remove(&self, id: Uuid) {
        self.turns
            .lock()
            .expect("turn registry poisoned")
            .remove(&id);
    }
}

/// Removes a turn from the registry when it ends.
pub struct TurnGuard {
    id: Uuid,
    registry: TurnRegistry,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.registry.remove(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelling_one_turn_does_not_cancel_another() {
        let registry = TurnRegistry::default();
        let first = Uuid::new_v4();
        let (a, _guard_a) = registry.begin(first);
        let (_b, _guard_b) = registry.begin(Uuid::new_v4());

        assert!(!a.is_cancelled());
        assert!(registry.cancel(first));
        assert!(a.is_cancelled());
    }

    #[test]
    fn cancelling_an_unknown_turn_is_ignored() {
        let registry = TurnRegistry::default();
        assert!(!registry.cancel(Uuid::new_v4()));
    }

    #[tokio::test]
    async fn the_guard_removes_the_turn() {
        let registry = TurnRegistry::default();
        let id = Uuid::new_v4();
        {
            let (_cancel, _guard) = registry.begin(id);
            assert!(registry.cancel(id));
        }
        assert!(!registry.cancel(id), "a finished turn must not linger");
    }

    #[tokio::test]
    async fn waiting_for_cancellation_wakes_up() {
        let registry = TurnRegistry::default();
        let id = Uuid::new_v4();
        let (cancel, _guard) = registry.begin(id);
        registry.cancel(id);
        // Already cancelled: the wait must not block.
        let mut cancel = cancel;
        cancel.cancelled().await;
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn never_is_never_cancelled() {
        let cancel = Cancellation::never();
        assert!(!cancel.is_cancelled());
    }
}
