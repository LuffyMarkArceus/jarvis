//! The local lane (Milestone 6.5b): the queue every call to a local model
//! waits in. `[model] slots` calls run at once - 1 on a laptop GPU that
//! holds one model, more on a machine that holds several. First come, first
//! served; a waiter that gives up waiting for now (to consider a spill)
//! keeps its place in line.

use crate::cancel::Cancellation;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

type Acquire = Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, AcquireError>> + Send>>;

pub struct LocalLane {
    semaphore: Arc<Semaphore>,
    /// The model each current holder is running, by permit id.
    running: Mutex<Vec<(u64, String)>>,
    next: AtomicU64,
}

/// What `acquire` gives back.
pub enum Admit {
    /// The call may run.
    Slot(LanePermit),
    /// The deadline passed with every slot taken. The waiter is still in
    /// line: `wait()` resumes from the same place.
    Busy(Waiter),
    Cancelled,
}

/// A slot. Dropping it frees the slot.
pub struct LanePermit {
    _permit: OwnedSemaphorePermit,
    lane: Arc<LocalLane>,
    id: u64,
    waited: Duration,
}

/// A call still in line.
pub struct Waiter {
    lane: Arc<LocalLane>,
    model: String,
    started: Instant,
    acquire: Acquire,
}

impl LocalLane {
    pub fn new(slots: usize) -> Arc<LocalLane> {
        Arc::new(LocalLane {
            semaphore: Arc::new(Semaphore::new(slots.max(1))),
            running: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        })
    }

    /// Slots free right now.
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// The model of a call holding a slot, for "another window is using X".
    pub fn running(&self) -> Option<String> {
        self.running
            .lock()
            .unwrap()
            .first()
            .map(|(_, model)| model.clone())
    }

    /// Wait for a slot for `model`, up to `deadline` (no deadline: until
    /// granted or cancelled).
    pub async fn acquire(
        self: &Arc<Self>,
        model: &str,
        deadline: Option<Duration>,
        cancel: Option<&Cancellation>,
    ) -> Admit {
        let mut waiter = Waiter {
            lane: self.clone(),
            model: model.to_string(),
            started: Instant::now(),
            acquire: Box::pin(self.semaphore.clone().acquire_owned()),
        };
        let mut cancel = cancel.cloned().unwrap_or_default();
        let limit = async {
            match deadline {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            permit = &mut waiter.acquire => match permit {
                Ok(permit) => Admit::Slot(waiter.lane.grant(permit, &waiter.model, waiter.started)),
                Err(_) => Admit::Cancelled,
            },
            _ = cancel.cancelled() => Admit::Cancelled,
            _ = limit => Admit::Busy(waiter),
        }
    }

    fn grant(
        self: &Arc<Self>,
        permit: OwnedSemaphorePermit,
        model: &str,
        started: Instant,
    ) -> LanePermit {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.running.lock().unwrap().push((id, model.to_string()));
        LanePermit {
            _permit: permit,
            lane: self.clone(),
            id,
            waited: started.elapsed(),
        }
    }
}

impl LanePermit {
    /// How long the call waited for this slot.
    pub fn waited(&self) -> Duration {
        self.waited
    }
}

impl Drop for LanePermit {
    fn drop(&mut self) {
        self.lane
            .running
            .lock()
            .unwrap()
            .retain(|(id, _)| *id != self.id);
    }
}

impl Waiter {
    /// Keep waiting, from the same place in line. `None` if cancelled.
    pub async fn wait(mut self, cancel: Option<&Cancellation>) -> Option<LanePermit> {
        let mut cancel = cancel.cloned().unwrap_or_default();
        tokio::select! {
            biased;
            permit = &mut self.acquire => permit
                .ok()
                .map(|p| self.lane.grant(p, &self.model, self.started)),
            _ = cancel.cancelled() => None,
        }
    }

    /// How long it has waited so far.
    pub fn waited(&self) -> Duration {
        self.started.elapsed()
    }

    /// What it is waiting behind.
    pub fn running(&self) -> Option<String> {
        self.lane.running()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::TurnRegistry;

    fn slot(admit: Admit) -> LanePermit {
        match admit {
            Admit::Slot(p) => p,
            _ => panic!("expected a slot"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_free_slot_is_granted_at_once() {
        let lane = LocalLane::new(1);
        let permit = slot(lane.acquire("qwen", None, None).await);
        assert_eq!(lane.available(), 0);
        assert_eq!(lane.running().as_deref(), Some("qwen"));
        drop(permit);
        assert_eq!(lane.available(), 1);
        assert_eq!(lane.running(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn waiters_are_served_first_come_first_served() {
        let lane = LocalLane::new(1);
        let holder = slot(lane.acquire("qwen", None, None).await);
        let order = Arc::new(Mutex::new(Vec::new()));
        let (release_tx, _) = tokio::sync::broadcast::channel::<()>(8);
        let mut tasks = Vec::new();
        for name in ["B", "C", "D"] {
            let (lane, order) = (lane.clone(), order.clone());
            let mut release = release_tx.subscribe();
            tasks.push(tokio::spawn(async move {
                let permit = slot(lane.acquire(name, None, None).await);
                order.lock().unwrap().push(name);
                let _ = release.recv().await;
                drop(permit);
            }));
            // Let each one queue before the next.
            tokio::task::yield_now().await;
        }
        drop(holder);
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let _ = release_tx.send(());
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec!["B", "C", "D"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_deadline_returns_busy_and_keeps_the_place() {
        let lane = LocalLane::new(1);
        let holder = slot(lane.acquire("qwen", None, None).await);
        let Admit::Busy(b) = lane.acquire("B", Some(Duration::from_secs(5)), None).await else {
            panic!("expected busy");
        };
        assert!(b.waited() >= Duration::from_secs(5));
        assert_eq!(b.running().as_deref(), Some("qwen"));
        // C queues behind B, with no deadline.
        let c_lane = lane.clone();
        let c = tokio::spawn(async move { slot(c_lane.acquire("C", None, None).await) });
        tokio::task::yield_now().await;
        drop(holder);
        let b_permit = tokio::time::timeout(Duration::from_secs(10), b.wait(None))
            .await
            .expect("B kept its place and is served first")
            .expect("B is served");
        assert_eq!(lane.running().as_deref(), Some("B"));
        assert!(!c.is_finished(), "C waits behind B");
        drop(b_permit);
        let c_permit = c.await.unwrap();
        assert_eq!(lane.running().as_deref(), Some("C"));
        drop(c_permit);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_waiter_frees_nothing_and_loses_nothing() {
        let lane = LocalLane::new(1);
        let holder = slot(lane.acquire("qwen", None, None).await);
        let registry = TurnRegistry::default();
        let id = uuid::Uuid::new_v4();
        let (cancel, _guard) = registry.begin(id);
        let b_lane = lane.clone();
        let b = tokio::spawn(async move {
            matches!(
                b_lane.acquire("B", None, Some(&cancel)).await,
                Admit::Cancelled
            )
        });
        tokio::task::yield_now().await;
        registry.cancel(id);
        let cancelled = tokio::time::timeout(Duration::from_secs(10), b)
            .await
            .expect("a cancel ends the wait at once");
        assert!(cancelled.unwrap(), "B was cancelled");
        drop(holder);
        assert_eq!(lane.available(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn two_slots_run_two_calls() {
        let lane = LocalLane::new(2);
        let _a = slot(lane.acquire("qwen", None, None).await);
        let _b = slot(lane.acquire("llama", None, None).await);
        assert!(matches!(
            lane.acquire("C", Some(Duration::from_secs(5)), None).await,
            Admit::Busy(_)
        ));
    }
}
