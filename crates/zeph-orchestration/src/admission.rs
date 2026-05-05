// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Provider-level admission control for sub-agent dispatch.
//!
//! `AdmissionGate` wraps per-provider `tokio::sync::Semaphore` instances, limiting
//! the number of concurrently dispatched sub-agents that target a given LLM provider.
//! This prevents provider rate-limit cascades (HTTP 429) and unbounded cost spikes
//! when many parallel tasks fan out to the same provider.
//!
//! # Usage
//!
//! ```rust,ignore
//! use zeph_orchestration::admission::AdmissionGate;
//!
//! // Build from provider entries (only those with max_concurrent set).
//! let gates = AdmissionGate::new(&[
//!     ("quality".to_string(), 3),
//!     ("fast".to_string(), 8),
//! ]);
//!
//! // Before dispatching a sub-agent, try to acquire a permit.
//! let permit = gates.try_acquire("quality"); // None when at capacity
//! // Drop permit when the sub-agent completes.
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Per-provider concurrency limiter for sub-agent dispatch.
///
/// Constructed once at [`DagScheduler`](crate::scheduler::DagScheduler) creation
/// time from the `[[llm.providers]]` entries that have `max_concurrent` set.
/// Providers without a limit are absent from the internal map — `try_acquire`
/// returns `None` (unlimited) for them by convention of the caller.
///
/// Permits are `OwnedSemaphorePermit` so they can be stored in `RunningTask`
/// without lifetime coupling to the gate itself. A permit is automatically
/// released when it is dropped (i.e., when the `RunningTask` entry is removed).
#[derive(Debug)]
pub struct AdmissionGate {
    semaphores: HashMap<String, Arc<Semaphore>>,
}

impl AdmissionGate {
    /// Build an `AdmissionGate` from a slice of `(provider_name, max_concurrent)` pairs.
    ///
    /// Only providers with `max_concurrent > 0` get a semaphore; zero-limit entries
    /// are silently ignored (the caller should filter to `max_concurrent.is_some()`
    /// before passing entries here).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_orchestration::admission::AdmissionGate;
    ///
    /// let gate = AdmissionGate::new(&[("quality".to_string(), 3)]);
    /// ```
    #[must_use]
    pub fn new(providers: &[(String, usize)]) -> Self {
        let semaphores = providers
            .iter()
            .filter(|(_, limit)| *limit > 0)
            .map(|(name, limit)| (name.clone(), Arc::new(Semaphore::new(*limit))))
            .collect();
        Self { semaphores }
    }

    /// Attempt a non-blocking acquire for the named provider.
    ///
    /// Returns `Some(permit)` when a slot is available, `None` when the provider
    /// is at capacity or has no configured limit (caller treats `None` as "proceed
    /// without a permit" only when the provider has no gate — the distinction is
    /// made by `DagScheduler` which checks `admission_gate` presence first).
    ///
    /// The returned `OwnedSemaphorePermit` releases its slot automatically on drop.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_orchestration::admission::AdmissionGate;
    ///
    /// let gate = AdmissionGate::new(&[("quality".to_string(), 1)]);
    ///
    /// let permit = gate.try_acquire("quality");
    /// assert!(permit.is_some(), "first acquire must succeed");
    ///
    /// // Second acquire while first permit is held must fail.
    /// let second = gate.try_acquire("quality");
    /// assert!(second.is_none(), "second acquire must fail when at capacity");
    ///
    /// drop(permit); // releases the slot
    ///
    /// let third = gate.try_acquire("quality");
    /// assert!(third.is_some(), "acquire after release must succeed");
    /// ```
    #[must_use]
    pub fn try_acquire(&self, provider: &str) -> Option<OwnedSemaphorePermit> {
        let sem = self.semaphores.get(provider)?;
        Arc::clone(sem).try_acquire_owned().ok()
    }

    /// Returns `true` when this gate has a concurrency limit configured for `provider`.
    #[must_use]
    pub fn has_gate(&self, provider: &str) -> bool {
        self.semaphores.contains_key(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_acquired_and_released() {
        let gate = AdmissionGate::new(&[("quality".to_string(), 2)]);

        let p1 = gate.try_acquire("quality");
        let p2 = gate.try_acquire("quality");
        assert!(p1.is_some());
        assert!(p2.is_some());

        // At capacity — third must fail.
        let p3 = gate.try_acquire("quality");
        assert!(p3.is_none());

        // Release one slot.
        drop(p1);
        let p4 = gate.try_acquire("quality");
        assert!(p4.is_some());
    }

    #[test]
    fn try_acquire_returns_none_when_at_capacity() {
        let gate = AdmissionGate::new(&[("limited".to_string(), 1)]);

        let permit = gate.try_acquire("limited");
        assert!(permit.is_some());

        let second = gate.try_acquire("limited");
        assert!(second.is_none(), "must return None at capacity");
    }

    #[test]
    fn unknown_provider_returns_none() {
        let gate = AdmissionGate::new(&[("known".to_string(), 3)]);
        assert!(gate.try_acquire("unknown").is_none());
        assert!(!gate.has_gate("unknown"));
    }

    #[test]
    fn has_gate_reflects_configured_providers() {
        let gate = AdmissionGate::new(&[("quality".to_string(), 3)]);
        assert!(gate.has_gate("quality"));
        assert!(!gate.has_gate("fast"));
    }

    #[test]
    fn zero_limit_entry_is_ignored() {
        // 0-limit entries must not create a semaphore (Semaphore::new(0) is valid but useless).
        let gate = AdmissionGate::new(&[("zero".to_string(), 0)]);
        assert!(!gate.has_gate("zero"));
        // try_acquire on a gate-less provider returns None (same as unknown).
        assert!(gate.try_acquire("zero").is_none());
    }

    #[test]
    fn new_empty_providers_ok() {
        let gate = AdmissionGate::new(&[]);
        assert!(!gate.has_gate("any"));
    }

    #[test]
    fn permit_released_after_drop_makes_slot_available() {
        let gate = AdmissionGate::new(&[("q".to_string(), 1)]);
        {
            let _permit = gate.try_acquire("q").expect("should acquire");
            assert!(gate.try_acquire("q").is_none(), "must be at capacity");
        } // permit dropped here
        assert!(
            gate.try_acquire("q").is_some(),
            "slot must be available after drop"
        );
    }
}
