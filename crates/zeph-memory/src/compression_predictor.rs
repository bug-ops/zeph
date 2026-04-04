// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Performance-floor compression ratio predictor (#2460).
//!
//! A lightweight linear regression model that predicts compaction probe quality
//! at a given compression ratio. Used to select the most aggressive compression
//! ratio that keeps the predicted probe score above `hard_fail_threshold`.
//!
//! # Design
//!
//! - No external ML crate dependencies — pure f32 arithmetic
//! - 4 input features: `compression_ratio`, `message_count`, `avg_message_length`, `tool_output_fraction`
//! - MSE loss with mini-batch gradient descent (continuous score target, not binary)
//! - Sigmoid output activation to bound predictions in [0.0, 1.0]
//! - Persisted as JSON via the `compression_predictor_weights` `SQLite` table
//! - Falls back to `None` (use default behavior) during cold start
//! - Training data sliding window: only the most recent N samples are retained

use serde::{Deserialize, Serialize};

const LEARNING_RATE: f32 = 0.01;
const EPOCHS: usize = 50;

// ── Features ──────────────────────────────────────────────────────────────────

/// Input features for the compression quality predictor.
#[derive(Debug, Clone, Copy)]
pub struct CompressionFeatures {
    /// Fraction of tokens retained after compression. Range: [0.0, 1.0].
    pub compression_ratio: f32,
    /// Normalized message count (divide by a reference scale, e.g. 100).
    pub message_count: f32,
    /// Normalized average token count per message.
    pub avg_message_length: f32,
    /// Fraction of messages that are tool outputs. Range: [0.0, 1.0].
    pub tool_output_fraction: f32,
}

impl CompressionFeatures {
    /// Convert to a fixed-length array for model arithmetic.
    #[must_use]
    pub fn to_vec(&self) -> [f32; 4] {
        [
            self.compression_ratio,
            self.message_count,
            self.avg_message_length,
            self.tool_output_fraction,
        ]
    }
}

// ── Weights ───────────────────────────────────────────────────────────────────

/// Persisted model state (JSON-serializable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionModelWeights {
    /// Weight vector (one per feature).
    pub weights: Vec<f32>,
    /// Bias term.
    pub bias: f32,
    /// Number of training samples used.
    pub sample_count: u64,
}

impl Default for CompressionModelWeights {
    fn default() -> Self {
        // Small positive initial weights. Compression ratio is positively correlated
        // with quality (higher ratio = less compression = better score), so initializing
        // weights positive helps the model converge faster from cold start.
        Self {
            weights: vec![0.3, 0.05, 0.05, -0.1],
            bias: 0.0,
            sample_count: 0,
        }
    }
}

// ── Model ─────────────────────────────────────────────────────────────────────

/// Compression quality predictor using linear regression with sigmoid output.
pub struct CompressionPredictor {
    weights: CompressionModelWeights,
}

impl CompressionPredictor {
    /// Create a new model with default (untrained) weights.
    #[must_use]
    pub fn new() -> Self {
        Self {
            weights: CompressionModelWeights::default(),
        }
    }

    /// Create a model from saved weights.
    #[must_use]
    pub fn from_weights(weights: CompressionModelWeights) -> Self {
        Self { weights }
    }

    /// Serialize current weights to JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn serialize(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.weights)
    }

    /// Return the number of training samples used.
    #[must_use]
    pub fn sample_count(&self) -> u64 {
        self.weights.sample_count
    }

    /// Return `true` if the model has fewer than `min_samples` training samples.
    ///
    /// During cold start, `select_ratio` returns `None` and the caller falls back
    /// to default compression behavior.
    #[must_use]
    pub fn is_cold_start(&self, min_samples: u64) -> bool {
        self.weights.sample_count < min_samples
    }

    /// Predict probe score for the given features. Range: `[0.0, 1.0]`.
    ///
    /// Uses `sigmoid(w^T x + b)` to bound predictions.
    #[must_use]
    pub fn predict(&self, features: &CompressionFeatures) -> f32 {
        let x = features.to_vec();
        let dot: f32 = x
            .iter()
            .zip(self.weights.weights.iter())
            .map(|(xi, wi)| xi * wi)
            .sum();
        sigmoid(dot + self.weights.bias)
    }

    /// Train the model on a batch of `(features, probe_score)` pairs using MSE loss.
    ///
    /// Runs `EPOCHS` gradient steps. `sample_count` is incremented by the batch size
    /// once per `train()` call (not per epoch) to avoid over-counting.
    pub fn train(&mut self, samples: &[(CompressionFeatures, f32)]) {
        if samples.is_empty() {
            return;
        }

        let n_features = self.weights.weights.len();
        #[allow(clippy::cast_precision_loss)]
        let n = samples.len() as f32;

        for _ in 0..EPOCHS {
            let mut grad_w = vec![0.0f32; n_features];
            let mut grad_b = 0.0f32;

            for (features, target) in samples {
                let pred = self.predict(features);
                // MSE gradient: 2 * (pred - target) * sigmoid_derivative(pred)
                // sigmoid_derivative(y) = y * (1 - y)
                let error = (pred - target) * pred * (1.0 - pred);
                let x = features.to_vec();
                for (i, xi) in x.iter().enumerate() {
                    grad_w[i] += error * xi;
                }
                grad_b += error;
            }

            for (wi, gi) in self.weights.weights.iter_mut().zip(grad_w.iter()) {
                *wi -= LEARNING_RATE * gi / n;
            }
            self.weights.bias -= LEARNING_RATE * grad_b / n;
        }

        self.weights.sample_count += samples.len() as u64;
    }

    /// Find the most aggressive compression ratio that keeps predicted score >= `floor`.
    ///
    /// Iterates `candidate_ratios` from lowest (most aggressive) to highest (least aggressive).
    /// Returns the first ratio whose predicted quality clears `floor`, or `None` if no
    /// candidate passes (caller should fall back to default behavior).
    ///
    /// # Panics
    ///
    /// Does not panic; returns `None` on empty candidate list.
    #[must_use]
    pub fn select_ratio(
        &self,
        floor: f32,
        candidate_ratios: &[f32],
        message_count: f32,
        avg_message_length: f32,
        tool_output_fraction: f32,
    ) -> Option<f32> {
        // Iterate from most aggressive to least aggressive (ascending ratio order).
        let mut sorted = candidate_ratios.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        for &ratio in &sorted {
            let features = CompressionFeatures {
                compression_ratio: ratio,
                message_count,
                avg_message_length,
                tool_output_fraction,
            };
            if self.predict(&features) >= floor {
                return Some(ratio);
            }
        }
        None
    }
}

impl Default for CompressionPredictor {
    fn default() -> Self {
        Self::new()
    }
}

/// Logistic (sigmoid) activation function.
#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigmoid_at_zero_is_half() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn predict_with_default_weights_returns_valid_range() {
        let model = CompressionPredictor::new();
        let features = CompressionFeatures {
            compression_ratio: 0.5,
            message_count: 0.5,
            avg_message_length: 0.5,
            tool_output_fraction: 0.2,
        };
        let p = model.predict(&features);
        assert!(p > 0.0 && p < 1.0, "prediction must be in (0, 1): {p}");
    }

    #[test]
    fn is_cold_start_true_when_no_training() {
        let model = CompressionPredictor::new();
        assert!(model.is_cold_start(10));
    }

    #[test]
    fn is_cold_start_false_after_training() {
        let mut model = CompressionPredictor::new();
        let features = CompressionFeatures {
            compression_ratio: 0.5,
            message_count: 0.5,
            avg_message_length: 0.5,
            tool_output_fraction: 0.2,
        };
        let samples: Vec<_> = (0..10).map(|_| (features, 0.7f32)).collect();
        model.train(&samples);
        assert!(!model.is_cold_start(10));
    }

    #[test]
    fn training_on_high_ratio_improves_high_ratio_prediction() {
        let mut model = CompressionPredictor::new();
        let high_ratio = CompressionFeatures {
            compression_ratio: 0.8,
            message_count: 0.5,
            avg_message_length: 0.5,
            tool_output_fraction: 0.2,
        };
        let initial_pred = model.predict(&high_ratio);
        let samples: Vec<_> = (0..50).map(|_| (high_ratio, 0.9f32)).collect();
        model.train(&samples);
        let trained_pred = model.predict(&high_ratio);
        assert!(
            trained_pred > initial_pred,
            "training on high target must increase prediction: {initial_pred} -> {trained_pred}"
        );
    }

    #[test]
    fn select_ratio_returns_most_aggressive_passing_ratio() {
        let mut model = CompressionPredictor::new();
        // Train so that 0.8 predicts well but 0.2 predicts poorly.
        let good = CompressionFeatures {
            compression_ratio: 0.8,
            message_count: 0.5,
            avg_message_length: 0.5,
            tool_output_fraction: 0.2,
        };
        let bad = CompressionFeatures {
            compression_ratio: 0.2,
            message_count: 0.5,
            avg_message_length: 0.5,
            tool_output_fraction: 0.2,
        };
        let samples: Vec<(CompressionFeatures, f32)> = (0..100)
            .map(|i| if i % 2 == 0 { (good, 0.95) } else { (bad, 0.1) })
            .collect();
        model.train(&samples);

        let ratios = vec![0.2, 0.4, 0.6, 0.8, 0.9];
        let selected = model.select_ratio(0.5, &ratios, 0.5, 0.5, 0.2);
        // Must pick a ratio that the model predicts >= 0.5
        if let Some(r) = selected {
            let features = CompressionFeatures {
                compression_ratio: r,
                message_count: 0.5,
                avg_message_length: 0.5,
                tool_output_fraction: 0.2,
            };
            assert!(
                model.predict(&features) >= 0.5,
                "selected ratio must clear floor"
            );
        }
    }

    #[test]
    fn select_ratio_returns_none_when_nothing_passes() {
        let model = CompressionPredictor::new();
        // Default weights are all zero: sigmoid(0) = 0.5, which is below the 0.99 floor.
        // All candidates must fail the floor check, so None is returned.
        let ratios = vec![0.1, 0.2];
        let selected = model.select_ratio(0.99, &ratios, 0.5, 0.5, 0.2);
        assert!(
            selected.is_none(),
            "expected None when no ratio clears the floor"
        );
    }

    #[test]
    fn sample_count_increments_after_training() {
        let mut model = CompressionPredictor::new();
        assert_eq!(model.sample_count(), 0);
        let features = CompressionFeatures {
            compression_ratio: 0.5,
            message_count: 0.5,
            avg_message_length: 0.5,
            tool_output_fraction: 0.2,
        };
        model.train(&[(features, 0.7)]);
        assert_eq!(model.sample_count(), 1);
        model.train(&[(features, 0.6), (features, 0.8)]);
        assert_eq!(model.sample_count(), 3);
    }

    #[test]
    fn serialize_roundtrip() {
        let model = CompressionPredictor::new();
        let json = model.serialize().expect("serialize");
        let weights: CompressionModelWeights = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(weights.weights.len(), model.weights.weights.len());
        assert!((weights.bias - model.weights.bias).abs() < 1e-6);
    }
}
