// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-process wakeup registry for parked promises and timers.
//!
//! A [`NotifyRegistry`] is the rendezvous between a task awaiting an external completion (a
//! [`DurablePromise`](crate::DurablePromise) or a `sleep_until`) and the task that satisfies it (a
//! [`DurableHandle::resolve`](crate::DurableHandle::resolve) call, or the
//! [`DurableTimerService`](crate::DurableTimerService)). Both reach the *same* registry through the
//! shared [`LocalBackend`](crate::LocalBackend), so an in-process resolution wakes the waiter
//! immediately rather than waiting out the database poll interval.
//!
//! The registry is a pure in-memory optimization layered on top of the durable database state: the
//! database row is always the source of truth (resolution is committed before the wake fires), and a
//! waiter that registered before a cross-process resolution — or that could not register because the
//! parked cap was reached — still observes the result through its periodic poll. Losing a wakeup
//! therefore costs at most one extra poll interval; it never costs correctness.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use uuid::Uuid;

/// A keyed map of one-shot [`Notify`] handles shared between waiters and resolvers.
///
/// Keyed by the raw [`Uuid`] of a [`PromiseId`](crate::PromiseId) or
/// [`TimerId`](crate::TimerId). The inner `Mutex` only ever guards map insertion and removal — never
/// an `.await` — so it cannot deadlock the async runtime: a waiter clones its `Arc<Notify>` out and
/// awaits it with the lock released.
#[derive(Debug, Default)]
pub(crate) struct NotifyRegistry {
    inner: Mutex<HashMap<Uuid, Arc<Notify>>>,
}

impl NotifyRegistry {
    /// Register interest in `key`, returning the shared [`Notify`] to await on.
    ///
    /// `cap` bounds the number of distinct parked keys (the `max_parked_promises` budget). When the
    /// registry already holds `cap` distinct keys and `key` is not among them, registration is
    /// declined (`None`) and the caller falls back to pure database polling. An already-registered
    /// `key` always returns its existing handle regardless of the cap, so a re-poll never trips it.
    pub(crate) fn register(&self, key: Uuid, cap: Option<usize>) -> Option<Arc<Notify>> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = guard.get(&key) {
            return Some(existing.clone());
        }
        if let Some(cap) = cap
            && guard.len() >= cap
        {
            return None;
        }
        let notify = Arc::new(Notify::new());
        guard.insert(key, notify.clone());
        Some(notify)
    }

    /// Wake every task parked on `key` and drop the registration.
    ///
    /// Called once the backing row has been committed to a satisfied state. Uses
    /// [`Notify::notify_waiters`] so only tasks already parked at the call are woken; a waiter that
    /// registers afterward re-reads the committed state and returns without parking.
    pub(crate) fn wake(&self, key: Uuid) {
        let notify = {
            let mut guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.remove(&key)
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }

    /// Drop a registration without waking, for a waiter that has observed the result and is leaving.
    pub(crate) fn cancel(&self, key: Uuid) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&key);
    }

    /// Current number of distinct parked keys (for the parked-cap gauge and tests).
    #[cfg(test)]
    pub(crate) fn parked(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn register_is_idempotent_per_key() {
        let reg = NotifyRegistry::default();
        let key = Uuid::now_v7();
        let a = reg.register(key, None).unwrap();
        let b = reg.register(key, None).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "the same key shares one Notify");
        assert_eq!(reg.parked(), 1);
    }

    #[test]
    fn cap_declines_new_keys_but_admits_existing() {
        let reg = NotifyRegistry::default();
        let first = Uuid::now_v7();
        assert!(reg.register(first, Some(1)).is_some());
        // A second distinct key is declined at the cap → caller polls.
        assert!(reg.register(Uuid::now_v7(), Some(1)).is_none());
        // The already-parked key is still served (a re-poll must not be locked out).
        assert!(reg.register(first, Some(1)).is_some());
    }

    #[tokio::test]
    async fn wake_releases_a_parked_waiter() {
        let reg = Arc::new(NotifyRegistry::default());
        let key = Uuid::now_v7();
        let notify = reg.register(key, None).unwrap();
        let parked = {
            let notify = notify.clone();
            tokio::spawn(async move { notify.notified().await })
        };
        // Give the spawned task a moment to park before we wake it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        reg.wake(key);
        tokio::time::timeout(Duration::from_secs(1), parked)
            .await
            .expect("waiter is woken")
            .expect("task joins");
        assert_eq!(reg.parked(), 0, "wake drops the registration");
    }
}
