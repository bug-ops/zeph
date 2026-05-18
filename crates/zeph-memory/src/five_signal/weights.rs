// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_config::memory::FiveSignalConfig;

/// Normalized five-signal retrieval weights.
///
/// Constructed once at startup via [`FiveSignalWeights::normalized`]; the result is cached
/// in [`super::FiveSignalRuntime`] and reused for every recall call in the session.
///
/// When all three new weights (`w_frequency`, `w_causal`, `w_novelty`) are `0.0`, the
/// formula degenerates to the two-signal baseline, preserving exact backward compatibility.
#[derive(Debug, Clone, Copy)]
pub struct FiveSignalWeights {
    /// Recency signal weight (normalized).
    pub w_recency: f64,
    /// Semantic relevance signal weight (normalized).
    pub w_relevance: f64,
    /// Access frequency signal weight (normalized).
    pub w_frequency: f64,
    /// Causal distance signal weight (normalized).
    pub w_causal: f64,
    /// Novelty signal weight (normalized).
    pub w_novelty: f64,
}

impl FiveSignalWeights {
    /// Construct normalized weights from config.
    ///
    /// If the configured weights do not sum to `1.0` (within `1e-9`), they are normalized
    /// by dividing each by the sum. A `WARN` is logged with original and normalized values.
    /// If the sum is `0.0`, falls back to `(0.5, 0.5, 0.0, 0.0, 0.0)` with a `WARN`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::memory::FiveSignalConfig;
    /// use zeph_memory::five_signal::weights::FiveSignalWeights;
    ///
    /// let cfg = FiveSignalConfig::default();
    /// let w = FiveSignalWeights::normalized(&cfg);
    /// assert!((w.w_recency + w.w_relevance + w.w_frequency + w.w_causal + w.w_novelty - 1.0).abs() < 1e-9);
    /// ```
    #[must_use]
    pub fn normalized(cfg: &FiveSignalConfig) -> Self {
        let _span = tracing::info_span!("memory.five_signal.weights.normalize").entered();

        let sum = cfg.w_recency + cfg.w_relevance + cfg.w_frequency + cfg.w_causal + cfg.w_novelty;

        if sum.abs() < 1e-12 {
            tracing::warn!(
                "five_signal: all weights are zero; falling back to (0.5, 0.5, 0.0, 0.0, 0.0)"
            );
            return Self {
                w_recency: 0.5,
                w_relevance: 0.5,
                w_frequency: 0.0,
                w_causal: 0.0,
                w_novelty: 0.0,
            };
        }

        if (sum - 1.0).abs() > 1e-9 {
            tracing::warn!(
                original_sum = %sum,
                w_recency = %cfg.w_recency,
                w_relevance = %cfg.w_relevance,
                w_frequency = %cfg.w_frequency,
                w_causal = %cfg.w_causal,
                w_novelty = %cfg.w_novelty,
                "five_signal: weights do not sum to 1.0; normalizing"
            );
        }

        Self {
            w_recency: cfg.w_recency / sum,
            w_relevance: cfg.w_relevance / sum,
            w_frequency: cfg.w_frequency / sum,
            w_causal: cfg.w_causal / sum,
            w_novelty: cfg.w_novelty / sum,
        }
    }

    /// Returns `true` when the three new signals are inactive (`w_frequency = w_causal = w_novelty = 0`).
    ///
    /// When baseline, the five-signal path is short-circuited and the existing two-signal
    /// pipeline runs unchanged, ensuring zero overhead and exact behavioral equivalence.
    #[must_use]
    #[inline]
    pub fn is_baseline(&self) -> bool {
        self.w_frequency.abs() < 1e-12
            && self.w_causal.abs() < 1e-12
            && self.w_novelty.abs() < 1e-12
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_test::traced_test;

    #[test]
    fn default_config_normalizes_to_fifty_fifty() {
        let cfg = FiveSignalConfig::default();
        let w = FiveSignalWeights::normalized(&cfg);
        // default: w_recency=0.35, w_relevance=0.35, rest=0 → sum=0.70 → each /0.70 ≈ 0.5
        assert!((w.w_recency - 0.5).abs() < 1e-9);
        assert!((w.w_relevance - 0.5).abs() < 1e-9);
        assert!(w.is_baseline());
    }

    #[test]
    fn already_normalized_stays_unchanged() {
        let cfg = FiveSignalConfig {
            w_recency: 0.35,
            w_relevance: 0.35,
            w_frequency: 0.15,
            w_causal: 0.10,
            w_novelty: 0.05,
            ..FiveSignalConfig::default()
        };
        let w = FiveSignalWeights::normalized(&cfg);
        assert!((w.w_recency - 0.35).abs() < 1e-9);
        assert!((w.w_frequency - 0.15).abs() < 1e-9);
        assert!(!w.is_baseline());
    }

    #[test]
    fn zero_sum_falls_back_to_fifty_fifty() {
        let cfg = FiveSignalConfig {
            w_recency: 0.0,
            w_relevance: 0.0,
            ..FiveSignalConfig::default()
        };
        let w = FiveSignalWeights::normalized(&cfg);
        assert!((w.w_recency - 0.5).abs() < 1e-9);
        assert!((w.w_relevance - 0.5).abs() < 1e-9);
    }

    #[traced_test]
    #[test]
    fn unnormalized_weights_emit_warn() {
        let cfg = FiveSignalConfig {
            w_recency: 2.0,
            w_relevance: 3.0,
            ..FiveSignalConfig::default()
        };
        // sum = 5.0, not 1.0 → WARN must be emitted
        let _w = FiveSignalWeights::normalized(&cfg);
        assert!(logs_contain("weights do not sum to 1.0"));
    }

    #[traced_test]
    #[test]
    fn zero_sum_emits_warn() {
        let cfg = FiveSignalConfig {
            w_recency: 0.0,
            w_relevance: 0.0,
            ..FiveSignalConfig::default()
        };
        let _w = FiveSignalWeights::normalized(&cfg);
        assert!(logs_contain("all weights are zero"));
    }

    #[test]
    fn unnormalized_weights_are_scaled() {
        let cfg = FiveSignalConfig {
            w_recency: 2.0,
            w_relevance: 2.0,
            w_frequency: 1.0,
            w_causal: 0.0,
            w_novelty: 0.0,
            ..FiveSignalConfig::default()
        };
        // sum = 5.0 → w_recency = 0.4, w_relevance = 0.4, w_frequency = 0.2
        let w = FiveSignalWeights::normalized(&cfg);
        let total = w.w_recency + w.w_relevance + w.w_frequency + w.w_causal + w.w_novelty;
        assert!((total - 1.0).abs() < 1e-9, "sum must be 1.0, got {total}");
        assert!((w.w_frequency - 0.2).abs() < 1e-9);
    }
}
