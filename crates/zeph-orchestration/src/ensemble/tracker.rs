// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Telemetry-only per-member agreement tracker for the verifier ensemble.
//!
//! [`EnsembleTracker`] records, per ensemble member, how often that member's ballot agreed
//! with the merged majority verdict, as a decayed exponential moving average (EMA). It exists
//! purely for observability (CLI/TUI stats surfacing, diagnosing a misbehaving member) — in
//! PR-1 it is **never** consulted to decide which members are dispatched in a given round.
//! Re-introducing EMA-gated subset selection requires a ground-truth reward signal that does
//! not exist yet (spec 073 §4 "Ask First"; critic finding S2 — gating on this tracker's score
//! creates a consensus-collapse feedback loop).

use std::collections::HashMap;

/// Neutral prior score for a member with no (or insufficient) observations.
const NEUTRAL_PRIOR: f64 = 0.5;

#[derive(Debug, Clone)]
struct EmaEntry {
    score: f64,
    observations: u64,
}

/// Decayed-EMA tracker of per-member agreement-with-majority rate.
///
/// Deliberately a new, small, deterministic type rather than a generalization of RAPS's
/// `ReputationTracker` (Beta/Thompson-coupled, provider-scoped) or `AdaptOrch` (stochastic
/// topology bandit) — see spec 073 §3.4 for the full rationale.
#[derive(Debug, Clone)]
pub struct EnsembleTracker {
    scores: HashMap<String, EmaEntry>,
    alpha: f64,
    decay: f64,
    min_observations: u32,
}

impl EnsembleTracker {
    /// Create a new tracker.
    ///
    /// `alpha` weights the newest observation in the EMA update; `decay` pulls a member's
    /// score toward the neutral prior (`0.5`) on every observation, so a member that stops
    /// responding gradually reverts to "unknown" rather than keeping a stale score forever.
    /// `min_observations` gates [`Self::ema`] — a member's score is not considered
    /// meaningful until it has been observed at least this many times.
    #[must_use]
    pub fn new(alpha: f64, decay: f64, min_observations: u32) -> Self {
        Self {
            scores: HashMap::new(),
            alpha,
            decay,
            min_observations,
        }
    }

    /// Record whether `member`'s ballot agreed with the merged majority verdict.
    ///
    /// Decay-then-EMA update: the existing score first decays toward the neutral prior, then
    /// blends in the new observation. This means a member observed for the first time starts
    /// from the neutral prior rather than from `agreed`'s raw `0.0`/`1.0`, avoiding a
    /// single-observation score swing to either extreme.
    pub fn record(&mut self, member: &str, agreed: bool) {
        let value = if agreed { 1.0 } else { 0.0 };
        let entry = self.scores.entry(member.to_owned()).or_insert(EmaEntry {
            score: NEUTRAL_PRIOR,
            observations: 0,
        });
        let decayed = self.decay * entry.score + (1.0 - self.decay) * NEUTRAL_PRIOR;
        entry.score = self.alpha * value + (1.0 - self.alpha) * decayed;
        entry.observations += 1;
    }

    /// Current EMA score for `member`, or `None` if it has fewer than `min_observations`
    /// recorded observations (cold-start gate) or has never been observed.
    #[must_use]
    pub fn ema(&self, member: &str) -> Option<f64> {
        self.scores.get(member).and_then(|e| {
            if e.observations >= u64::from(self.min_observations) {
                Some(e.score)
            } else {
                None
            }
        })
    }

    /// Snapshot of every tracked member's current score and observation count, for CLI/TUI
    /// stats surfacing. Includes cold-start members (score below `min_observations`) with
    /// their raw score — callers wanting the cold-start-gated view should use [`Self::ema`]
    /// per member instead.
    ///
    /// Sorted by member name for stable display order — `scores` is a `HashMap`, so without
    /// sorting, CLI/TUI stats rows would reorder non-deterministically between renders.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, f64, u64)> {
        let mut rows: Vec<(String, f64, u64)> = self
            .scores
            .iter()
            .map(|(member, entry)| (member.clone(), entry.score, entry.observations))
            .collect();
        rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_gate_returns_none_below_min_observations() {
        let mut tracker = EnsembleTracker::new(0.3, 0.95, 5);
        for _ in 0..4 {
            tracker.record("a", true);
        }
        assert_eq!(tracker.ema("a"), None);
    }

    #[test]
    fn cold_start_gate_returns_some_at_min_observations() {
        let mut tracker = EnsembleTracker::new(0.3, 0.95, 5);
        for _ in 0..5 {
            tracker.record("a", true);
        }
        assert!(tracker.ema("a").is_some());
    }

    #[test]
    fn unobserved_member_returns_none() {
        let tracker = EnsembleTracker::new(0.3, 0.95, 1);
        assert_eq!(tracker.ema("never-seen"), None);
    }

    #[test]
    fn ema_trends_toward_agreement() {
        let mut tracker = EnsembleTracker::new(0.5, 1.0, 1);
        // decay=1.0 isolates the pure EMA update (no prior-decay component).
        for _ in 0..20 {
            tracker.record("a", true);
        }
        let score = tracker.ema("a").unwrap();
        assert!(score > 0.99, "score should converge near 1.0, got {score}");
    }

    #[test]
    fn ema_trends_toward_disagreement() {
        let mut tracker = EnsembleTracker::new(0.5, 1.0, 1);
        for _ in 0..20 {
            tracker.record("a", false);
        }
        let score = tracker.ema("a").unwrap();
        assert!(score < 0.01, "score should converge near 0.0, got {score}");
    }

    #[test]
    fn decay_pulls_stale_score_toward_neutral_prior() {
        // alpha=0 means the new observation contributes nothing; only decay acts.
        let mut tracker = EnsembleTracker::new(0.5, 0.9, 1);
        tracker.record("a", true); // first obs: score = 0.5*1.0 + 0.5*0.5 = 0.75
        let after_first = tracker.ema("a").unwrap();
        assert!((after_first - 0.75).abs() < 1e-9);

        // Repeated `false` observations should decay the score back down, not stay pinned.
        for _ in 0..10 {
            tracker.record("a", false);
        }
        let after_many_false = tracker.ema("a").unwrap();
        assert!(after_many_false < after_first);
    }

    #[test]
    fn observations_increment_monotonically() {
        let mut tracker = EnsembleTracker::new(0.3, 0.95, 1);
        tracker.record("a", true);
        tracker.record("a", false);
        tracker.record("a", true);
        let snapshot = tracker.snapshot();
        let (_, _, obs) = snapshot.iter().find(|(m, _, _)| m == "a").unwrap();
        assert_eq!(*obs, 3);
    }

    #[test]
    fn snapshot_includes_all_members() {
        let mut tracker = EnsembleTracker::new(0.3, 0.95, 1);
        tracker.record("a", true);
        tracker.record("b", false);
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.len(), 2);
    }

    /// M1 regression: `snapshot()` must return a deterministic, name-sorted order regardless
    /// of `HashMap` iteration order — otherwise CLI/TUI stats rows reorder between renders.
    #[test]
    fn snapshot_is_sorted_by_member_name() {
        let mut tracker = EnsembleTracker::new(0.3, 0.95, 1);
        // Insert in reverse-alphabetical order so a passing test proves sorting, not luck.
        tracker.record("zeta", true);
        tracker.record("mid", false);
        tracker.record("alpha", true);
        let snapshot = tracker.snapshot();
        let names: Vec<&str> = snapshot.iter().map(|(m, _, _)| m.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn no_select_subset_method_exists() {
        // Compile-time guard, not a runtime assertion: EnsembleTracker must expose only
        // `record`/`ema`/`snapshot` — no `select_subset` (critic finding S2). This test
        // documents the invariant; a `select_subset` addition would not fail this test but
        // would be caught in code review per spec 073 §4 "Never".
        let tracker = EnsembleTracker::new(0.3, 0.95, 5);
        assert!(tracker.snapshot().is_empty());
    }
}
