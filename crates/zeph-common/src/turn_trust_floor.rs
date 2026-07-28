// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared, monotonically-downgradable trust floor for a single agent turn (#6701).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::SkillTrustLevel;

/// Per-turn trust floor shared between a `ToolExecutor`'s trust gate and any skill-body
/// resolution path that can downgrade it mid-turn (e.g. an explicit `invoke_skill` of a
/// Quarantined skill).
///
/// Wraps an `Arc<AtomicU8>` — cloning shares the same underlying cell, so every holder
/// observes the same value. Two operations mutate it:
///
/// - [`set`](Self::set): turn-start assignment. Replaces the floor unconditionally — trust
///   may go up or down relative to the previous turn. Call once per turn, before any tool
///   dispatch.
/// - [`fold`](Self::fold): monotonic downgrade. The floor becomes `min(current, level)` —
///   it can never raise trust. Models "this turn's trust degraded because
///   prompt-injected/quarantined content was actually read" — the only way back up is a
///   fresh [`set`](Self::set) at the next turn boundary.
///
/// # Examples
///
/// ```rust
/// use zeph_common::{SkillTrustLevel, TurnTrustFloor};
///
/// let floor = TurnTrustFloor::new(SkillTrustLevel::Trusted);
/// floor.fold(SkillTrustLevel::Quarantined);
/// assert_eq!(floor.get(), SkillTrustLevel::Quarantined);
///
/// // fold never raises trust, even toward a higher-trust argument.
/// floor.fold(SkillTrustLevel::Trusted);
/// assert_eq!(floor.get(), SkillTrustLevel::Quarantined);
///
/// // set is a full turn-start reset — it can raise trust again.
/// floor.set(SkillTrustLevel::Trusted);
/// assert_eq!(floor.get(), SkillTrustLevel::Trusted);
/// ```
#[derive(Clone, Debug)]
pub struct TurnTrustFloor(Arc<AtomicU8>);

impl TurnTrustFloor {
    /// Creates a new floor initialized to `initial`.
    #[must_use]
    pub fn new(initial: SkillTrustLevel) -> Self {
        Self(Arc::new(AtomicU8::new(initial.severity())))
    }

    /// Turn-start assignment: replaces the floor unconditionally.
    ///
    /// Call once per turn, before any tool dispatch — never mid-turn, or a genuine
    /// mid-turn downgrade (see [`fold`](Self::fold)) could be silently undone.
    pub fn set(&self, level: SkillTrustLevel) {
        self.0.store(level.severity(), Ordering::Relaxed);
    }

    /// Monotonic downgrade: the floor becomes `min(current, level)`.
    ///
    /// Can never raise trust. The `Result` from the underlying CAS is intentionally
    /// discarded — the closure always returns `Some`, so the update always succeeds.
    pub fn fold(&self, level: SkillTrustLevel) {
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                let current = SkillTrustLevel::from_severity(cur);
                Some(current.min_trust(level).severity())
            });
    }

    /// Returns the current floor value.
    #[must_use]
    pub fn get(&self) -> SkillTrustLevel {
        SkillTrustLevel::from_severity(self.0.load(Ordering::Relaxed))
    }
}

impl Default for TurnTrustFloor {
    /// Defaults to [`SkillTrustLevel::Trusted`] — the same starting point
    /// `TrustGateExecutor::new` used before this type existed.
    fn default() -> Self {
        Self::new(SkillTrustLevel::Trusted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_replaces_unconditionally_in_either_direction() {
        let floor = TurnTrustFloor::new(SkillTrustLevel::Quarantined);
        floor.set(SkillTrustLevel::Trusted);
        assert_eq!(floor.get(), SkillTrustLevel::Trusted);
        floor.set(SkillTrustLevel::Blocked);
        assert_eq!(floor.get(), SkillTrustLevel::Blocked);
    }

    #[test]
    fn fold_lowers_trust() {
        let floor = TurnTrustFloor::new(SkillTrustLevel::Trusted);
        floor.fold(SkillTrustLevel::Quarantined);
        assert_eq!(floor.get(), SkillTrustLevel::Quarantined);
    }

    #[test]
    fn fold_never_raises_trust() {
        let floor = TurnTrustFloor::new(SkillTrustLevel::Quarantined);
        floor.fold(SkillTrustLevel::Trusted);
        assert_eq!(
            floor.get(),
            SkillTrustLevel::Quarantined,
            "fold must never raise trust above the current floor"
        );
    }

    #[test]
    fn fold_to_blocked_is_sticky_until_a_fresh_set() {
        let floor = TurnTrustFloor::new(SkillTrustLevel::Verified);
        floor.fold(SkillTrustLevel::Blocked);
        floor.fold(SkillTrustLevel::Trusted);
        assert_eq!(floor.get(), SkillTrustLevel::Blocked);
        floor.set(SkillTrustLevel::Trusted);
        assert_eq!(
            floor.get(),
            SkillTrustLevel::Trusted,
            "only a fresh turn-start set recovers trust, never fold"
        );
    }

    #[test]
    fn clone_shares_the_same_underlying_cell() {
        let floor = TurnTrustFloor::new(SkillTrustLevel::Trusted);
        let clone = floor.clone();
        clone.fold(SkillTrustLevel::Quarantined);
        assert_eq!(
            floor.get(),
            SkillTrustLevel::Quarantined,
            "clones must observe writes through any other clone"
        );
    }

    #[test]
    fn default_starts_trusted() {
        assert_eq!(TurnTrustFloor::default().get(), SkillTrustLevel::Trusted);
    }
}
