// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session-wide cumulative subagent spawn budget (issue #6545).

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::SubAgentError;

/// Session-wide cumulative counter of subagent spawns.
///
/// Bounds the *total* number of subagents spawned over a session's lifetime, independent of
/// [`SubAgentManager::spawn`](crate::manager::SubAgentManager::spawn)'s existing
/// `max_concurrent` (in-flight) and `max_spawn_depth` (recursion) guardrails — a shallow,
/// low-concurrency but high-frequency sequential delegation loop trips neither of those.
///
/// # Ownership, not a shared handle
///
/// Deliberately a plain `AtomicUsize` newtype — no `Arc`, no `Clone`. Nothing in this crate
/// needs a shared, cloned handle to a budget: `SubAgentManager` owns one instance as the origin
/// of truth, and `zeph-core`'s `OrchestrationState` owns an independent fallback instance used
/// only when no manager is wired. Both the manager-side spawn path and the ACP `/subagent
/// spawn` chokepoint (which never touches `SubAgentManager` at all) reach whichever instance
/// applies through an accessor (`Agent::session_budget` in `zeph-core`) that hands out a plain
/// `&SessionSpawnBudget` reference, never a copy.
///
/// This also makes a would-be TOCTOU hazard structurally impossible rather than merely
/// documented: such a hazard could only arise if `SubAgentManager` were ever shared (e.g.
/// behind an `Arc`) across concurrent tasks, and a future refactor down that path would have to
/// deliberately reintroduce `Clone`/`Arc` here — a change reviewable on its own, rather than one
/// hiding behind an innocuous `.clone()` call at some unrelated call site.
///
/// # Concurrency
///
/// `AtomicUsize` provides interior mutability so [`check`](Self::check) and
/// [`record_spawn`](Self::record_spawn) work through a shared `&self` reference (every access
/// goes through `&self` accessors, never `&mut self`), satisfying NFR-001's atomic-counter
/// requirement. Every call path that reaches either method — `SubAgentManager::spawn`/`resume`,
/// the orchestration scheduler, and the ACP chokepoint — is serialized behind `&mut Agent` on
/// the single agent task, so `Relaxed` ordering suffices. The check/consume split (budget is
/// checked at the spawn guard but only consumed at the true commit point, so a rejected or
/// transiently retried spawn never burns budget it never used) introduces no reachable race
/// under that serialization.
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::SessionSpawnBudget;
///
/// let budget = SessionSpawnBudget::default();
/// assert_eq!(budget.spawned(), 0);
///
/// budget.check(1).expect("budget not yet exhausted");
/// budget.record_spawn();
/// assert_eq!(budget.spawned(), 1);
/// assert!(budget.check(1).is_err(), "cap of 1 must now be exhausted");
///
/// // `0` is the unlimited sentinel: check() always succeeds regardless of count.
/// assert!(budget.check(0).is_ok());
/// ```
#[derive(Default)]
pub struct SessionSpawnBudget(AtomicUsize);

impl SessionSpawnBudget {
    /// Check the budget without consuming it.
    ///
    /// `max == 0` is the unlimited sentinel and always succeeds, so callers never need to
    /// duplicate the sentinel check themselves (mirrors
    /// [`DelegationMode::permits_explicit`](zeph_config::DelegationMode::permits_explicit)'s
    /// anti-drift rationale for a check shared across multiple chokepoints).
    ///
    /// # Errors
    ///
    /// Returns [`SubAgentError::SessionSpawnLimit`] when the cumulative spawn count has
    /// already reached `max`.
    pub fn check(&self, max: usize) -> Result<(), SubAgentError> {
        if max == 0 {
            return Ok(());
        }
        let spawned = self.0.load(Ordering::Relaxed);
        if spawned >= max {
            return Err(SubAgentError::SessionSpawnLimit { spawned, max });
        }
        Ok(())
    }

    /// Record a successful spawn, incrementing the cumulative count by one.
    ///
    /// Must be called only at a spawn's true commit point — see the check/consume split
    /// described in the type-level concurrency note.
    pub fn record_spawn(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// Current cumulative spawn count.
    #[must_use]
    pub fn spawned(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for SessionSpawnBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionSpawnBudget")
            .field(&self.spawned())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mints_independent_counters() {
        let a = SessionSpawnBudget::default();
        let b = SessionSpawnBudget::default();
        a.record_spawn();
        assert_eq!(a.spawned(), 1);
        assert_eq!(
            b.spawned(),
            0,
            "default() must not share state across instances"
        );
    }

    #[test]
    fn zero_is_unlimited_sentinel() {
        let budget = SessionSpawnBudget::default();
        for _ in 0..1000 {
            budget.record_spawn();
        }
        assert!(budget.check(0).is_ok());
    }

    #[test]
    fn check_does_not_consume() {
        let budget = SessionSpawnBudget::default();
        budget.check(5).unwrap();
        budget.check(5).unwrap();
        assert_eq!(budget.spawned(), 0, "check() must be read-only");
    }

    #[test]
    fn cap_reached_returns_session_spawn_limit() {
        let budget = SessionSpawnBudget::default();
        budget.record_spawn();
        let err = budget.check(1).unwrap_err();
        assert!(matches!(
            err,
            SubAgentError::SessionSpawnLimit { spawned: 1, max: 1 }
        ));
    }

    #[test]
    fn debug_prints_count() {
        let budget = SessionSpawnBudget::default();
        budget.record_spawn();
        let debug = format!("{budget:?}");
        assert!(
            debug.contains('1'),
            "Debug output must surface the count: {debug}"
        );
    }
}
