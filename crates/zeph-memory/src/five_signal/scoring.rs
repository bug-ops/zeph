// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use crate::five_signal::weights::FiveSignalWeights;
use crate::types::MessageId;

/// Per-candidate raw signals before weighting.
#[derive(Debug, Clone, Copy, Default)]
pub struct CandidateSignals {
    /// Recency signal ∈ `[0.0, 1.0]`.
    pub recency: f64,
    /// Semantic relevance signal ∈ `[0.0, 1.0]`.
    pub relevance: f64,
    /// Normalized access frequency ∈ `[0.0, 1.0]`.
    pub frequency: f64,
    /// Causal distance converted to score ∈ `[0.0, 1.0]` via `1/distance`.
    pub causal: f64,
    /// Novelty ∈ `(0.0, 1.0]`.
    pub novelty: f64,
}

/// Apply five-signal scoring to a ranked candidate list.
///
/// Replaces each candidate's existing score with the weighted combination:
/// `score = w_recency × recency + w_relevance × relevance
///        + w_frequency × frequency + w_causal × causal + w_novelty × novelty`
///
/// Signals not provided in `signals_map` default to `0.0`.
/// After scoring, `ranked` is re-sorted descending by the new score.
///
/// # Parameters
///
/// - `ranked`: mutable slice of `(MessageId, score)` pairs to re-score in-place.
/// - `weights`: pre-normalized five-signal weights.
/// - `signals_map`: per-candidate signal values; missing candidates use zeros for new signals.
/// - `base_scores`: the original (recency+relevance) scores per candidate, used when the
///   caller has not pre-split recency from relevance. When a candidate appears in
///   `signals_map` its `recency` and `relevance` fields override this value.
///
/// # Examples
///
/// ```
/// use zeph_memory::five_signal::scoring::{apply_five_signal_scoring, CandidateSignals};
/// use zeph_memory::five_signal::weights::FiveSignalWeights;
/// use zeph_config::memory::FiveSignalConfig;
/// use zeph_memory::types::MessageId;
///
/// let mut cfg = FiveSignalConfig::default();
/// cfg.w_recency = 0.5;
/// cfg.w_relevance = 0.5;
/// cfg.w_frequency = 0.0;
/// cfg.w_causal = 0.0;
/// cfg.w_novelty = 0.0;
/// let weights = FiveSignalWeights::normalized(&cfg);
///
/// let id1 = MessageId(1);
/// let id2 = MessageId(2);
/// let mut ranked = vec![(id1, 0.8), (id2, 0.6)];
/// let signals = std::collections::HashMap::from([
///     (id1, CandidateSignals { recency: 0.8, relevance: 0.8, ..Default::default() }),
///     (id2, CandidateSignals { recency: 0.6, relevance: 0.6, ..Default::default() }),
/// ]);
///
/// apply_five_signal_scoring(&mut ranked, &weights, &signals);
/// assert_eq!(ranked[0].0, id1);
/// ```
pub fn apply_five_signal_scoring<S: std::hash::BuildHasher>(
    ranked: &mut [(MessageId, f64)],
    weights: &FiveSignalWeights,
    signals_map: &HashMap<MessageId, CandidateSignals, S>,
) {
    let _span = tracing::info_span!("memory.five_signal.scoring").entered();

    for (msg_id, score) in ranked.iter_mut() {
        let s = signals_map.get(msg_id).copied().unwrap_or_default();
        *score = weights.w_recency * s.recency
            + weights.w_relevance * s.relevance
            + weights.w_frequency * s.frequency
            + weights.w_causal * s.causal
            + weights.w_novelty * s.novelty;
    }

    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::memory::FiveSignalConfig;

    fn baseline_weights() -> FiveSignalWeights {
        let cfg = FiveSignalConfig {
            w_recency: 0.5,
            w_relevance: 0.5,
            ..FiveSignalConfig::default()
        };
        FiveSignalWeights::normalized(&cfg)
    }

    #[test]
    fn baseline_order_preserved() {
        let w = baseline_weights();
        let id1 = MessageId(1);
        let id2 = MessageId(2);
        let mut ranked = vec![(id1, 0.8_f64), (id2, 0.6_f64)];
        let signals = HashMap::from([
            (
                id1,
                CandidateSignals {
                    recency: 0.8,
                    relevance: 0.8,
                    ..Default::default()
                },
            ),
            (
                id2,
                CandidateSignals {
                    recency: 0.6,
                    relevance: 0.6,
                    ..Default::default()
                },
            ),
        ]);
        apply_five_signal_scoring(&mut ranked, &w, &signals);
        assert_eq!(ranked[0].0, id1, "higher score should rank first");
    }

    #[test]
    fn frequency_signal_flips_order() {
        let cfg = FiveSignalConfig {
            w_recency: 0.35,
            w_relevance: 0.35,
            w_frequency: 0.30,
            ..FiveSignalConfig::default()
        };
        let w = FiveSignalWeights::normalized(&cfg);

        let id1 = MessageId(1); // lower relevance, high frequency
        let id2 = MessageId(2); // higher relevance, zero frequency

        let mut ranked = vec![(id2, 0.9_f64), (id1, 0.7_f64)];
        let signals = HashMap::from([
            (
                id1,
                CandidateSignals {
                    recency: 0.5,
                    relevance: 0.7,
                    frequency: 1.0, // max frequency
                    ..Default::default()
                },
            ),
            (
                id2,
                CandidateSignals {
                    recency: 0.5,
                    relevance: 0.9,
                    frequency: 0.0,
                    ..Default::default()
                },
            ),
        ]);
        apply_five_signal_scoring(&mut ranked, &w, &signals);
        assert_eq!(ranked[0].0, id1, "frequency should flip the ranking");
    }

    #[test]
    fn missing_candidate_scores_zero() {
        let w = baseline_weights();
        let id1 = MessageId(1);
        let mut ranked = vec![(id1, 0.5_f64)];
        // Empty signals map → all signals default to 0
        apply_five_signal_scoring(&mut ranked, &w, &HashMap::new());
        assert!((ranked[0].1).abs() < 1e-9, "missing signals → score 0.0");
    }

    #[test]
    fn zero_new_weights_equals_two_signal_baseline() {
        // When w_frequency = w_causal = w_novelty = 0.0, the five-signal formula degenerates
        // to: score = w_recency * recency + w_relevance * relevance — exactly the two-signal result.
        let w = baseline_weights(); // 0.5 recency, 0.5 relevance, rest = 0
        assert!(w.is_baseline());

        let id1 = MessageId(1);
        let id2 = MessageId(2);

        let recency1 = 0.7_f64;
        let relevance1 = 0.8_f64;
        let recency2 = 0.9_f64;
        let relevance2 = 0.3_f64;

        // Expected two-signal scores
        let expected1 = 0.5 * recency1 + 0.5 * relevance1;
        let expected2 = 0.5 * recency2 + 0.5 * relevance2;

        let mut ranked = vec![(id1, 0.0_f64), (id2, 0.0_f64)];
        let signals = HashMap::from([
            (
                id1,
                CandidateSignals {
                    recency: recency1,
                    relevance: relevance1,
                    frequency: 0.0,
                    causal: 0.0,
                    novelty: 0.0,
                },
            ),
            (
                id2,
                CandidateSignals {
                    recency: recency2,
                    relevance: relevance2,
                    frequency: 0.0,
                    causal: 0.0,
                    novelty: 0.0,
                },
            ),
        ]);
        apply_five_signal_scoring(&mut ranked, &w, &signals);

        // After sort descending: id1 (0.75) > id2 (0.6)
        assert_eq!(ranked[0].0, id1);
        assert!(
            (ranked[0].1 - expected1).abs() < 1e-9,
            "five-signal must equal two-signal baseline"
        );
        assert!(
            (ranked[1].1 - expected2).abs() < 1e-9,
            "five-signal must equal two-signal baseline"
        );
    }
}
