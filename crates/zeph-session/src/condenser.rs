// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The [`Condenser`] contract for durable, replayable context condensation (spec §8).
//!
//! Condensation is distinct from live in-memory compaction (owned by `zeph-context`): it
//! operates at the event-log level and is recorded as a [`crate::event::SessionEvent::Condensation`]
//! event so replay can fold the same summary deterministically. This module defines the trait
//! contract and the [`INV-SP-4`](validate_non_overlap) non-overlap guard; see
//! [`crate::llm_condenser::LlmCondenser`] for the default implementation.

use zeph_common::memory::AnchoredSummary;

use crate::error::SessionError;
use crate::event::SessionEventEnvelope;
use crate::replay::ReconstructedState;

/// The outcome of one condensation pass: the `seq` range it replaced and the resulting summary.
#[derive(Debug, Clone)]
pub struct CondensationResult {
    /// `[inclusive, inclusive]` seq range replaced by `summary`.
    pub replaced_range: (u64, u64),
    pub summary: AnchoredSummary,
    pub tokens_before: u32,
    pub tokens_after: u32,
}

/// Computes whether and how to durably condense a session's event log.
///
/// Implementors MUST respect INV-SP-4 (spec §8.3): the range returned by [`Self::condense`] must
/// start strictly after the caller's `last_condensed_seq` — see [`validate_non_overlap`].
pub trait Condenser: Send + Sync {
    /// Returns `true` if the reconstructed context has grown enough (relative to
    /// `budget_used_fraction`, the fraction of the context budget currently consumed) to warrant
    /// a condensation pass.
    fn should_condense(
        &self,
        state: &ReconstructedState,
        budget_used_fraction: f64,
    ) -> impl Future<Output = bool> + Send;

    /// Condense `events` (typically the tail since `last_condensed_seq`), producing a
    /// [`CondensationResult`] whose `replaced_range` starts strictly after `last_condensed_seq`.
    ///
    /// # Errors
    ///
    /// Returns an error if summarization fails or the computed range would violate INV-SP-4.
    fn condense(
        &self,
        events: &[SessionEventEnvelope],
        last_condensed_seq: u64,
    ) -> impl Future<Output = Result<CondensationResult, SessionError>> + Send;
}

/// Enforce INV-SP-4: a proposed `(lo, hi)` range must start strictly after `last_condensed_seq`.
///
/// Callers (the `Condenser` implementation and `zeph-agent-persistence`'s live-compaction hook)
/// must call this immediately before emitting a `Condensation`/`Compaction` event, using the
/// `last_condensed_seq` read from `acp_sessions` at the start of the computation — not a
/// stale/cached value — to close the read-then-write race the invariant depends on.
///
/// `last_condensed_seq == 0` is treated as the sentinel "nothing has ever been condensed" rather
/// than "seq 0 was already condensed" — event logs are 0-indexed (a session's first event is
/// `seq == 0`, matching `acp_sessions.last_condensed_seq`'s migration-106 `DEFAULT 0`), so
/// without this carve-out the very first condensation of every session — which necessarily wants
/// to start at `lo == 0` — would be permanently rejected as "overlapping" the default. Verified
/// empirically: `LlmCondenser::condense`'s own doc-mandated caller pattern hit exactly this
/// before the carve-out was added (spec-068 D-11 end-to-end wiring). Residual gap: a
/// condensation whose range happens to end exactly at `hi == 0` (only possible when
/// `keep_recent` leaves just one message event past position 0) leaves `last_condensed_seq == 0`
/// again, indistinguishable from "never condensed" — narrow enough in practice that resolving it
/// properly needs an `Option<u64>` schema change; tracked as a follow-up, not blocking here.
///
/// # Errors
///
/// Returns [`SessionError::CondensationOverlap`] if `last_condensed_seq > 0 && lo <=
/// last_condensed_seq`.
pub fn validate_non_overlap(
    last_condensed_seq: u64,
    proposed_range: (u64, u64),
) -> Result<(), SessionError> {
    let (lo, hi) = proposed_range;
    if last_condensed_seq > 0 && lo <= last_condensed_seq {
        return Err(SessionError::CondensationOverlap(format!(
            "proposed range ({lo}, {hi}) overlaps or precedes last_condensed_seq={last_condensed_seq}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inv_sp4_no_overlap() {
        // First condensation over (1, 10) is fine when nothing has been condensed yet.
        validate_non_overlap(0, (1, 10)).unwrap();

        // A second condensation must start strictly after the first one's end.
        validate_non_overlap(10, (11, 20)).unwrap();

        // Overlapping or regressive ranges are rejected.
        assert!(validate_non_overlap(10, (5, 15)).is_err());
        assert!(validate_non_overlap(10, (10, 20)).is_err());
    }

    /// Regression test for the sentinel carve-out: the very first condensation of a session
    /// necessarily starts at `seq == 0` (event logs are 0-indexed) and must not be rejected just
    /// because `last_condensed_seq`'s default is also `0`.
    #[test]
    fn first_ever_condensation_may_start_at_seq_zero() {
        validate_non_overlap(0, (0, 5)).unwrap();
    }
}
