// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Bayesian posterior weight computation for skill ranking.
//!
//! Uses the Wilson score interval lower bound as a conservative estimate of the true
//! success rate, blending with cosine similarity for re-ranking matched skill candidates.
//!
//! # Why Wilson Score?
//!
//! The posterior mean `α / (α + β)` overestimates quality for skills with few observations.
//! The Wilson lower bound applies a 95% one-sided confidence penalty that deflates scores
//! when the evidence is weak, preventing freshly-installed skills from outranking well-tested
//! ones purely by chance.
//!
//! # Integration with [`crate::matcher`]
//!
//! After embedding-based ranking, call [`rerank`] to blend cosine similarity with Bayesian
//! trust weight. `cosine_weight = 0.7` gives a 70% embedding / 30% trust blend.
//!
//! # Examples
//!
//! ```rust
//! use zeph_skills::trust_score::{posterior_weight, posterior_mean, PRIOR_WEIGHT};
//!
//! // No data: conservative deflation below 0.5
//! let w = posterior_weight(0, 0);
//! assert!(w < PRIOR_WEIGHT);
//!
//! // High-confidence skill: close to 1.0
//! let w = posterior_weight(100, 0);
//! assert!(w > 0.9);
//! ```

use crate::matcher::ScoredMatch;

/// Neutral prior weight for skills with no outcome data.
///
/// Corresponds to `Beta(1, 1)` (uniform prior) posterior mean = 0.5.
/// The actual [`posterior_weight`] will be slightly below this due to the Wilson confidence
/// penalty when the observation count is zero.
pub const PRIOR_WEIGHT: f64 = 0.5;

/// Conservative Bayesian estimate of the true success rate.
///
/// Uses the Wilson score interval lower bound (95% one-sided confidence) derived from
/// `Beta(α = successes + 1, β = failures + 1)` with a uniform prior.
/// Returns a value in `[0.0, 1.0]`.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::trust_score::posterior_weight;
///
/// // Perfect record with 10 observations is still slightly below 1.0 due to the bound.
/// let w = posterior_weight(10, 0);
/// assert!(w > 0.7 && w <= 1.0);
/// ```
#[must_use]
pub fn posterior_weight(successes: u32, failures: u32) -> f64 {
    let alpha = f64::from(successes) + 1.0;
    let beta_val = f64::from(failures) + 1.0;
    let mean = alpha / (alpha + beta_val);
    let n = alpha + beta_val;
    // z = 1.645 for 95% one-sided confidence
    let std_err = (mean * (1.0 - mean) / n).sqrt();
    (mean - 1.645 * std_err).clamp(0.0, 1.0)
}

/// Raw posterior mean without the Wilson confidence penalty.
///
/// Returns `α / (α + β)` where `α = successes + 1` and `β = failures + 1`.
/// Suitable for display (e.g., TUI confidence bar) where conservative deflation
/// is less desirable than an unbiased estimate. Prefer [`posterior_weight`] for
/// ranking decisions.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::trust_score::posterior_mean;
///
/// // 3 successes, 1 failure → mean = (3+1)/(3+1+1+1) = 4/6 ≈ 0.667
/// let m = posterior_mean(3, 1);
/// assert!((m - 0.667).abs() < 0.01);
/// ```
#[must_use]
pub fn posterior_mean(successes: u32, failures: u32) -> f64 {
    let alpha = f64::from(successes) + 1.0;
    let beta_val = f64::from(failures) + 1.0;
    alpha / (alpha + beta_val)
}

/// Re-rank scored matches by blending cosine similarity with Bayesian trust weight in-place.
///
/// The final score for each match is:
/// ```text
/// score = cosine_weight * cosine_similarity + (1 - cosine_weight) * posterior_weight
/// ```
///
/// # Parameters
///
/// - `scored` — mutable slice of matches to re-rank (modified in-place).
/// - `cosine_weight` — blend factor in `[0.0, 1.0]`; `0.0` = trust only, `1.0` = cosine only.
/// - `metrics_fn` — callback that receives a match index and returns `(successes, failures)`.
///
/// # Examples
///
/// ```rust
/// use zeph_skills::trust_score::rerank;
/// use zeph_skills::matcher::ScoredMatch;
///
/// let mut matches = vec![
///     ScoredMatch { index: 0, score: 0.6 },
///     ScoredMatch { index: 1, score: 0.9 },
/// ];
/// // index 0 has a perfect record; index 1 is new
/// rerank(&mut matches, 0.5, |idx| if idx == 0 { (50, 0) } else { (0, 0) });
/// // index 0 should now outrank index 1 when trust dominates
/// assert_eq!(matches[0].index, 0);
/// ```
pub fn rerank(
    scored: &mut [ScoredMatch],
    cosine_weight: f32,
    metrics_fn: impl Fn(usize) -> (u32, u32),
) {
    let posterior_factor = 1.0 - cosine_weight;
    for m in scored.iter_mut() {
        let (successes, failures) = metrics_fn(m.index);
        #[allow(clippy::cast_possible_truncation)]
        let posterior = posterior_weight(successes, failures) as f32;
        m.score = cosine_weight * m.score + posterior_factor * posterior;
    }
    scored.sort_unstable_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posterior_weight_no_data_near_prior() {
        let w = posterior_weight(0, 0);
        // With 0 data the Wilson penalty deflates the neutral prior.
        assert!(w < PRIOR_WEIGHT);
        assert!(w >= 0.0);
    }

    #[test]
    fn posterior_weight_perfect_success_near_one() {
        let w = posterior_weight(100, 0);
        assert!(w > 0.9, "expected > 0.9, got {w}");
    }

    #[test]
    fn posterior_weight_perfect_failure_near_zero() {
        let w = posterior_weight(0, 100);
        assert!(w < 0.1, "expected < 0.1, got {w}");
    }

    #[test]
    fn posterior_weight_balanced_near_half() {
        let w = posterior_weight(50, 50);
        assert!(w > 0.3 && w < 0.5, "expected ~0.4, got {w}");
    }

    #[test]
    fn posterior_weight_monotone_increasing_with_successes() {
        // Start from a larger base so Wilson penalty doesn't clamp everything to 0.
        let failures = 5;
        let mut prev = posterior_weight(10, failures);
        for s in 11..=30 {
            let cur = posterior_weight(s, failures);
            assert!(cur > prev, "not monotone at s={s}: {cur} <= {prev}");
            prev = cur;
        }
    }

    #[test]
    fn posterior_weight_monotone_decreasing_with_failures() {
        let successes = 10;
        let mut prev = posterior_weight(successes, 5);
        for f in 6..=25 {
            let cur = posterior_weight(successes, f);
            assert!(cur < prev, "not monotone at f={f}: {cur} >= {prev}");
            prev = cur;
        }
    }

    #[test]
    fn posterior_mean_no_data_is_half() {
        let m = posterior_mean(0, 0);
        assert!((m - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn posterior_mean_nine_successes() {
        // Beta(10, 1) mean = 10/11
        let m = posterior_mean(9, 0);
        let expected = 10.0 / 11.0;
        assert!((m - expected).abs() < 1e-10, "got {m}, expected {expected}");
    }

    #[test]
    fn rerank_blends_scores() {
        let mut scored = vec![
            ScoredMatch {
                index: 0,
                score: 0.9,
            },
            ScoredMatch {
                index: 1,
                score: 0.5,
            },
        ];
        // index 0: cosine=0.9, posterior≈0 (0 success, 100 failure)
        // index 1: cosine=0.5, posterior≈1 (100 success, 0 failure)
        // cosine_weight=0.5 → index 1 should win
        rerank(
            &mut scored,
            0.5,
            |idx| {
                if idx == 0 { (0, 100) } else { (100, 0) }
            },
        );
        assert_eq!(scored[0].index, 1);
    }

    #[test]
    fn rerank_cosine_only_preserves_order() {
        let mut scored = vec![
            ScoredMatch {
                index: 0,
                score: 0.9,
            },
            ScoredMatch {
                index: 1,
                score: 0.1,
            },
        ];
        rerank(&mut scored, 1.0, |_| (0, 0));
        assert_eq!(scored[0].index, 0);
    }

    #[test]
    fn rerank_empty_slice_no_panic() {
        let mut scored: Vec<ScoredMatch> = vec![];
        rerank(&mut scored, 0.5, |_| (0, 0));
        assert!(scored.is_empty());
    }

    #[test]
    fn rerank_trust_only_ignores_cosine() {
        // cosine_weight=0.0 → posterior determines order entirely
        let mut scored = vec![
            ScoredMatch {
                index: 0,
                score: 0.99, // high cosine but bad trust
            },
            ScoredMatch {
                index: 1,
                score: 0.01, // low cosine but great trust
            },
        ];
        rerank(
            &mut scored,
            0.0,
            |idx| {
                if idx == 0 { (0, 100) } else { (100, 0) }
            },
        );
        assert_eq!(scored[0].index, 1, "trust-only: high trust should win");
    }

    #[test]
    fn posterior_weight_clamp_at_zero() {
        // very few successes, many failures — should clamp to 0.0, not negative
        let w = posterior_weight(0, 1000);
        assert!(w.abs() < f64::EPSILON);
    }

    #[test]
    fn posterior_mean_three_quarters() {
        // Beta(3+1, 1+1) = Beta(4, 2): mean = 4/6 = 2/3
        let m = posterior_mean(3, 1);
        let expected = 4.0 / 6.0;
        assert!((m - expected).abs() < 1e-10, "got {m}, expected {expected}");
    }

    #[test]
    fn posterior_weight_always_in_unit_interval() {
        for s in [0u32, 1, 5, 50, 100] {
            for f in [0u32, 1, 5, 50, 100] {
                let w = posterior_weight(s, f);
                assert!((0.0..=1.0).contains(&w), "out of [0,1] at s={s} f={f}: {w}");
            }
        }
    }

    #[test]
    fn rerank_single_element_no_panic() {
        let mut scored = vec![ScoredMatch {
            index: 0,
            score: 0.5,
        }];
        rerank(&mut scored, 0.5, |_| (10, 2));
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].index, 0);
    }
}
