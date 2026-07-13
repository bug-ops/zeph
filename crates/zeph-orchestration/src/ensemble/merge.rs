// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure, deterministic binary-majority merge of ensemble verification ballots.
//!
//! This module is the ORCH (arXiv:2602.01797) merge step, scoped to `PlanVerifier`
//! gap-severity verification. [`merge`] has no I/O, no async, and no randomness —
//! it is a pure function of its input, which is what makes the ensemble path
//! deterministic and unit-testable in isolation from any LLM provider.

use crate::verifier::Gap;

/// One ensemble member's independent judgment on a single verification task.
///
/// Produced from a successful `chat_typed::<VerificationResult>` response. Members that
/// errored or timed out never produce a `Ballot` — they are excluded from the vote entirely
/// (never cast as a fail-open `complete: true` vote).
#[derive(Debug, Clone)]
pub struct Ballot {
    /// Name of the `[[llm.providers]]` entry that produced this ballot.
    pub member: String,
    /// This member's verdict on whether the task output is complete.
    pub complete: bool,
    /// This member's own self-reported confidence (0.0-1.0).
    pub confidence: f64,
    /// Gaps this member identified. Only included in [`MergeOutcome::gaps`] when this
    /// ballot is on the winning side.
    pub gaps: Vec<Gap>,
}

/// Result of merging a set of [`Ballot`]s.
///
/// Internal to the ensemble subsystem — `agreement_ratio` and `tie_broken` are diagnostic
/// fields that must never be copied into `VerificationResult` (doing so would invert the
/// downstream `should_replan` gate; see spec 073 §4 "Never").
#[derive(Debug, Clone)]
pub struct MergeOutcome {
    /// Majority verdict on task completeness.
    pub complete: bool,
    /// Union of `gaps` from every ballot on the winning side, verbatim (no dedup).
    pub gaps: Vec<Gap>,
    /// Arithmetic mean of `confidence` from ballots on the winning side only.
    ///
    /// This is the value that becomes `VerificationResult.confidence` — never
    /// `agreement_ratio`.
    pub merged_confidence: f64,
    /// Fraction of ballots that agreed with the winning verdict. Telemetry only.
    pub agreement_ratio: f64,
    /// `true` when the vote was an exact tie, resolved by the fail-safe `complete = false`
    /// default. Only reachable with an even ballot count (partial member failure).
    pub tie_broken: bool,
}

/// Merge independent ensemble ballots into a single deterministic outcome.
///
/// Binary majority vote on [`Ballot::complete`]. On an exact tie (only reachable with an
/// even `ballots.len()` after partial member failure/exclusion) the fail-safe verdict is
/// `complete = false` — preferring a replan over silently accepting an uncertain result.
///
/// `gaps` is the verbatim union of the winning side's gaps (no dedup/reconciliation).
/// `merged_confidence` is the arithmetic mean of the winning side's self-reported
/// `confidence` values.
///
/// Pure function: no I/O, no async, no randomness. Callers are responsible for ensuring
/// `ballots` is non-empty (the ensemble quorum check guarantees this in practice) — an
/// empty slice returns a degenerate `complete: false` outcome rather than panicking.
///
/// # Examples
///
/// ```rust
/// use zeph_orchestration::ensemble::{Ballot, merge};
///
/// let ballots = vec![
///     Ballot { member: "a".into(), complete: true, confidence: 0.9, gaps: vec![] },
///     Ballot { member: "b".into(), complete: true, confidence: 0.8, gaps: vec![] },
///     Ballot { member: "c".into(), complete: false, confidence: 0.5, gaps: vec![] },
/// ];
/// let outcome = merge(&ballots);
/// assert!(outcome.complete);
/// assert!((outcome.merged_confidence - 0.85).abs() < 1e-9);
/// ```
#[must_use]
pub fn merge(ballots: &[Ballot]) -> MergeOutcome {
    let total = ballots.len();
    if total == 0 {
        return MergeOutcome {
            complete: false,
            gaps: Vec::new(),
            merged_confidence: 0.0,
            agreement_ratio: 0.0,
            tie_broken: false,
        };
    }

    let votes_true = ballots.iter().filter(|b| b.complete).count();
    let votes_false = total - votes_true;

    let (winner, tie_broken) = if votes_true > total / 2 {
        (true, false)
    } else if votes_false > total / 2 {
        (false, false)
    } else {
        // Exact tie: only reachable with an even ballot count. Fail-safe toward replan.
        (false, true)
    };

    let winning: Vec<&Ballot> = ballots.iter().filter(|b| b.complete == winner).collect();
    let winning_count = winning.len();

    let gaps: Vec<Gap> = winning
        .iter()
        .flat_map(|b| b.gaps.iter().cloned())
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let merged_confidence = if winning_count == 0 {
        0.0
    } else {
        winning.iter().map(|b| b.confidence).sum::<f64>() / winning_count as f64
    };

    #[allow(clippy::cast_precision_loss)]
    let agreement_ratio = winning_count as f64 / total as f64;

    MergeOutcome {
        complete: winner,
        gaps,
        merged_confidence,
        agreement_ratio,
        tie_broken,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verifier::GapSeverity;

    fn ballot(member: &str, complete: bool, confidence: f64, gaps: Vec<Gap>) -> Ballot {
        Ballot {
            member: member.to_string(),
            complete,
            confidence,
            gaps,
        }
    }

    fn gap(desc: &str, severity: GapSeverity) -> Gap {
        Gap {
            description: desc.to_string(),
            severity,
        }
    }

    #[test]
    fn unanimous_complete() {
        let ballots = vec![
            ballot("a", true, 0.9, vec![]),
            ballot("b", true, 0.8, vec![]),
            ballot("c", true, 0.95, vec![]),
        ];
        let outcome = merge(&ballots);
        assert!(outcome.complete);
        assert!(outcome.gaps.is_empty());
        assert!((outcome.merged_confidence - 0.883_333_333_333_333_3).abs() < 1e-9);
        assert!((outcome.agreement_ratio - 1.0).abs() < 1e-9);
        assert!(!outcome.tie_broken);
    }

    #[test]
    fn unanimous_incomplete() {
        let g = gap("missing tests", GapSeverity::Critical);
        let ballots = vec![
            ballot("a", false, 0.5, vec![g.clone()]),
            ballot("b", false, 0.6, vec![g.clone()]),
            ballot("c", false, 0.4, vec![g]),
        ];
        let outcome = merge(&ballots);
        assert!(!outcome.complete);
        assert_eq!(outcome.gaps.len(), 3);
        assert!((outcome.merged_confidence - 0.5).abs() < 1e-9);
        assert!((outcome.agreement_ratio - 1.0).abs() < 1e-9);
    }

    #[test]
    fn two_of_three_split_complete_wins() {
        let ballots = vec![
            ballot("a", true, 0.9, vec![]),
            ballot("b", true, 0.7, vec![]),
            ballot("c", false, 0.3, vec![gap("gap", GapSeverity::Minor)]),
        ];
        let outcome = merge(&ballots);
        assert!(outcome.complete);
        assert!(outcome.gaps.is_empty(), "loser's gaps must not appear");
        assert!((outcome.merged_confidence - 0.8).abs() < 1e-9);
        assert!((outcome.agreement_ratio - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn two_of_three_split_incomplete_wins() {
        let g = gap("critical gap", GapSeverity::Critical);
        let ballots = vec![
            ballot("a", false, 0.4, vec![g.clone()]),
            ballot("b", false, 0.6, vec![g]),
            ballot("c", true, 0.95, vec![]),
        ];
        let outcome = merge(&ballots);
        assert!(!outcome.complete);
        assert_eq!(outcome.gaps.len(), 2);
        assert!((outcome.merged_confidence - 0.5).abs() < 1e-9);
        assert!((outcome.agreement_ratio - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn exact_tie_is_fail_safe_incomplete() {
        // Even ballot count (2) after a member was excluded from a configured N=3.
        let ballots = vec![
            ballot("a", true, 0.9, vec![]),
            ballot("b", false, 0.5, vec![]),
        ];
        let outcome = merge(&ballots);
        assert!(!outcome.complete, "tie must fail-safe to complete=false");
        assert!(outcome.tie_broken);
        assert!((outcome.merged_confidence - 0.5).abs() < 1e-9);
        assert!((outcome.agreement_ratio - 0.5).abs() < 1e-9);
    }

    #[test]
    fn single_ballot_edge_case() {
        let ballots = vec![ballot("a", true, 0.77, vec![])];
        let outcome = merge(&ballots);
        assert!(outcome.complete);
        assert!((outcome.merged_confidence - 0.77).abs() < 1e-9);
        assert!((outcome.agreement_ratio - 1.0).abs() < 1e-9);
        assert!(!outcome.tie_broken);
    }

    #[test]
    fn empty_ballots_edge_case() {
        // Unreachable in practice (quorum check guarantees >= 1 ballot before merge() is
        // called), but must not panic.
        let outcome = merge(&[]);
        assert!(!outcome.complete);
        assert!(outcome.gaps.is_empty());
        assert!((outcome.merged_confidence).abs() < 1e-9);
        assert!((outcome.agreement_ratio).abs() < 1e-9);
        assert!(!outcome.tie_broken);
    }

    /// S4 regression: unanimous incomplete+critical must NOT be inverted relative to a
    /// disagreeing panel. `merged_confidence` must be the mean of winning-side self-reported
    /// confidence, never `agreement_ratio` — using `agreement_ratio` here would make the
    /// unanimous case (`agreement_ratio=1.0`) look MORE confident than it should, suppressing
    /// replan when it should trigger it.
    #[test]
    fn s4_regression_unanimous_vs_split_confidence_not_agreement_ratio() {
        let critical_gap = gap("critical gap", GapSeverity::Critical);

        // 3-of-3 unanimous incomplete, low self-reported confidence.
        let unanimous = vec![
            ballot("a", false, 0.3, vec![critical_gap.clone()]),
            ballot("b", false, 0.35, vec![critical_gap.clone()]),
            ballot("c", false, 0.25, vec![critical_gap.clone()]),
        ];
        let unanimous_outcome = merge(&unanimous);
        assert!((unanimous_outcome.agreement_ratio - 1.0).abs() < 1e-9);
        // merged_confidence must be the LOW mean of self-reported values, not the HIGH
        // agreement_ratio of 1.0.
        assert!((unanimous_outcome.merged_confidence - 0.3).abs() < 1e-9);
        assert!(unanimous_outcome.merged_confidence < unanimous_outcome.agreement_ratio);

        // Hand-compute should_replan using the same gate as scheduler_loop.rs.
        let threshold = 0.7_f64;
        let should_replan_unanimous = !unanimous_outcome.complete
            && unanimous_outcome.merged_confidence < threshold
            && unanimous_outcome
                .gaps
                .iter()
                .any(|g| g.severity == GapSeverity::Critical);
        assert!(
            should_replan_unanimous,
            "unanimous low-confidence incomplete verdict must trigger replan, not be \
             suppressed by a high agreement_ratio"
        );

        // 2-of-3 split incomplete, for contrast — same shape of assertion.
        let split = vec![
            ballot("a", false, 0.3, vec![critical_gap.clone()]),
            ballot("b", false, 0.35, vec![critical_gap]),
            ballot("c", true, 0.9, vec![]),
        ];
        let split_outcome = merge(&split);
        assert!((split_outcome.agreement_ratio - 2.0 / 3.0).abs() < 1e-9);
        assert!((split_outcome.merged_confidence - 0.325).abs() < 1e-9);
    }

    /// M6 regression: an excluded (errored/timed-out) member must never contribute a
    /// spurious `0.0` to the confidence mean — it must simply be absent from `ballots`.
    #[test]
    fn m6_regression_excluded_member_not_zero_padded() {
        // Simulates: N=3 configured, member "c" errored and was never turned into a Ballot.
        let ballots = vec![
            ballot("a", true, 0.9, vec![]),
            ballot("b", true, 0.8, vec![]),
        ];
        let outcome = merge(&ballots);
        assert!(outcome.complete);
        // Mean of {0.9, 0.8} = 0.85, NOT mean of {0.9, 0.8, 0.0} = 0.5667.
        assert!((outcome.merged_confidence - 0.85).abs() < 1e-9);
    }
}
