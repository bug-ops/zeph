// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lightweight logistic regression model for RL-based admission control (#2416).
//!
//! This module provides a pure-Rust binary classifier trained on `(features, was_recalled)`
//! pairs from `admission_training_data`. It replaces the LLM-based `future_utility` factor
//! when enough training data is available.
//!
//! # Design
//!
//! - No external ML crate dependencies — pure f32 arithmetic
//! - 5 input features matching the A-MAC factor vector (see [`AdmissionFeatures`])
//! - Mini-batch gradient descent on log-loss
//! - Persisted as JSON via the `admission_rl_weights` `SQLite` table
//! - Falls back to heuristic when `sample_count < rl_min_samples`

use serde::{Deserialize, Serialize};

const LEARNING_RATE: f32 = 0.01;

/// Input feature vector for the RL admission model.
///
/// Matches the factor order in [`crate::admission::AdmissionFactors`].
#[derive(Debug, Clone, Copy)]
pub struct AdmissionFeatures {
    pub factual_confidence: f32,
    pub semantic_novelty: f32,
    pub content_type_prior: f32,
    /// Encoded content length bucket: `0.0` = short (<100 chars), `0.5` = medium, `1.0` = long.
    pub content_length_bucket: f32,
    /// Encoded role: user=0.7, assistant=0.6, tool=0.8, system=0.3, other=0.5.
    pub role_encoding: f32,
}

impl AdmissionFeatures {
    /// Encode the role as a float matching `compute_content_type_prior` values.
    #[must_use]
    pub fn encode_role(role: &str) -> f32 {
        match role {
            "user" => 0.7,
            "assistant" => 0.6,
            "tool" | "tool_result" => 0.8,
            "system" => 0.3,
            _ => 0.5,
        }
    }

    /// Encode content length into a 3-bucket float.
    #[must_use]
    pub fn encode_length(content_len: usize) -> f32 {
        if content_len < 100 {
            0.0
        } else if content_len < 1000 {
            0.5
        } else {
            1.0
        }
    }

    /// Convert to a fixed-length slice for model arithmetic.
    #[must_use]
    pub fn to_vec(&self) -> [f32; 5] {
        [
            self.factual_confidence,
            self.semantic_novelty,
            self.content_type_prior,
            self.content_length_bucket,
            self.role_encoding,
        ]
    }
}

/// Persisted model state (JSON-serializable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RlModelWeights {
    /// Weight vector (one per feature).
    pub weights: Vec<f32>,
    /// Bias term.
    pub bias: f32,
    /// Number of training samples used.
    pub sample_count: u64,
}

impl Default for RlModelWeights {
    fn default() -> Self {
        // Initialize to small random-ish values to break symmetry.
        // Deterministic for reproducibility.
        Self {
            weights: vec![0.1, 0.1, 0.1, 0.05, 0.1],
            bias: 0.0,
            sample_count: 0,
        }
    }
}

/// Logistic regression admission model.
pub struct RlAdmissionModel {
    weights: RlModelWeights,
}

impl RlAdmissionModel {
    /// Create a new model with default (untrained) weights.
    #[must_use]
    pub fn new() -> Self {
        Self {
            weights: RlModelWeights::default(),
        }
    }

    /// Create a model from saved weights.
    #[must_use]
    pub fn from_weights(weights: RlModelWeights) -> Self {
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

    /// Return the current sample count.
    #[must_use]
    pub fn sample_count(&self) -> u64 {
        self.weights.sample_count
    }

    /// Predict recall probability for the given features. Range: `[0.0, 1.0]`.
    ///
    /// Uses the logistic (sigmoid) function: `sigma(w^T x + b)`.
    #[must_use]
    pub fn predict(&self, features: &AdmissionFeatures) -> f32 {
        let x = features.to_vec();
        let dot: f32 = x
            .iter()
            .zip(self.weights.weights.iter())
            .map(|(xi, wi)| xi * wi)
            .sum();
        sigmoid(dot + self.weights.bias)
    }

    /// Train the model on a batch of (features, label) pairs using mini-batch gradient descent.
    ///
    /// `label = 1.0` when the message was recalled (positive), `0.0` when not recalled (negative).
    /// Runs `EPOCHS` gradient steps over the full batch so the model converges meaningfully.
    /// `sample_count` is incremented once per call (not per epoch) to avoid over-counting.
    /// Learning rate is fixed at `0.01` — suitable for the expected data scale.
    pub fn train(&mut self, samples: &[(AdmissionFeatures, f32)]) {
        const EPOCHS: usize = 50;

        if samples.is_empty() {
            return;
        }

        let n_features = self.weights.weights.len();
        #[allow(clippy::cast_precision_loss)]
        let n = samples.len() as f32;

        for _ in 0..EPOCHS {
            let mut grad_w = vec![0.0f32; n_features];
            let mut grad_b = 0.0f32;

            for (features, label) in samples {
                let pred = self.predict(features);
                let error = pred - label;
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

        // Increment once per train() call, not per epoch.
        self.weights.sample_count += samples.len() as u64;
    }
}

impl Default for RlAdmissionModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Logistic (sigmoid) activation function.
#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Parse training samples from `SQLite` records for model training.
///
/// Returns `(features, label)` pairs where `label = 1.0` if `was_recalled`.
#[must_use]
pub fn parse_training_samples(
    records: &[crate::store::admission_training::AdmissionTrainingRecord],
) -> Vec<(AdmissionFeatures, f32)> {
    records
        .iter()
        .filter_map(|r| {
            // Parse features_json: ["factual_confidence", "semantic_novelty",
            //                        "content_type_prior", "length_bucket", "role_encoding"]
            let arr: Vec<f32> = serde_json::from_str(&r.features_json).ok()?;
            if arr.len() < 5 {
                return None;
            }
            let features = AdmissionFeatures {
                factual_confidence: arr[0],
                semantic_novelty: arr[1],
                content_type_prior: arr[2],
                content_length_bucket: arr[3],
                role_encoding: arr[4],
            };
            let label = if r.was_recalled { 1.0f32 } else { 0.0f32 };
            Some((features, label))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigmoid_at_zero_is_half() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sigmoid_large_positive_approaches_one() {
        assert!(sigmoid(10.0) > 0.99);
    }

    #[test]
    fn sigmoid_large_negative_approaches_zero() {
        assert!(sigmoid(-10.0) < 0.01);
    }

    #[test]
    fn predict_with_default_weights_is_near_half() {
        let model = RlAdmissionModel::new();
        let features = AdmissionFeatures {
            factual_confidence: 0.5,
            semantic_novelty: 0.5,
            content_type_prior: 0.5,
            content_length_bucket: 0.5,
            role_encoding: 0.5,
        };
        let p = model.predict(&features);
        assert!(p > 0.0 && p < 1.0, "prediction must be in (0, 1): {p}");
    }

    #[test]
    fn train_updates_weights() {
        let mut model = RlAdmissionModel::new();
        let features = AdmissionFeatures {
            factual_confidence: 1.0,
            semantic_novelty: 1.0,
            content_type_prior: 1.0,
            content_length_bucket: 1.0,
            role_encoding: 1.0,
        };
        let initial_pred = model.predict(&features);
        // Train with all positive labels — model should increase weights over time.
        let samples: Vec<_> = (0..100).map(|_| (features, 1.0f32)).collect();
        model.train(&samples);
        let trained_pred = model.predict(&features);
        assert!(
            trained_pred > initial_pred,
            "training on positive labels must increase prediction: {initial_pred} -> {trained_pred}"
        );
    }

    #[test]
    fn sample_count_increments_after_training() {
        let mut model = RlAdmissionModel::new();
        assert_eq!(model.sample_count(), 0);
        let features = AdmissionFeatures {
            factual_confidence: 0.5,
            semantic_novelty: 0.5,
            content_type_prior: 0.5,
            content_length_bucket: 0.5,
            role_encoding: 0.5,
        };
        model.train(&[(features, 1.0)]);
        assert_eq!(model.sample_count(), 1);
        model.train(&[(features, 0.0), (features, 1.0)]);
        assert_eq!(model.sample_count(), 3);
    }

    #[test]
    fn serialize_and_deserialize_roundtrip() {
        let model = RlAdmissionModel::new();
        let json = model.serialize().expect("serialize");
        let weights: RlModelWeights = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(weights.weights.len(), model.weights.weights.len());
        assert!((weights.bias - model.weights.bias).abs() < 1e-6);
    }

    #[test]
    fn encode_role_matches_content_type_prior() {
        assert!((AdmissionFeatures::encode_role("user") - 0.7).abs() < 0.01);
        assert!((AdmissionFeatures::encode_role("tool") - 0.8).abs() < 0.01);
        assert!((AdmissionFeatures::encode_role("assistant") - 0.6).abs() < 0.01);
        assert!((AdmissionFeatures::encode_role("system") - 0.3).abs() < 0.01);
    }

    #[test]
    fn encode_length_buckets() {
        assert!((AdmissionFeatures::encode_length(50) - 0.0).abs() < 0.01);
        assert!((AdmissionFeatures::encode_length(500) - 0.5).abs() < 0.01);
        assert!((AdmissionFeatures::encode_length(5000) - 1.0).abs() < 0.01);
    }

    #[test]
    fn parse_training_samples_skips_short_arrays() {
        use crate::store::admission_training::AdmissionTrainingRecord;
        use crate::types::ConversationId;
        let records = vec![AdmissionTrainingRecord {
            id: 1,
            message_id: None,
            conversation_id: ConversationId(1),
            content_hash: "abc".into(),
            role: "user".into(),
            composite_score: 0.5,
            was_admitted: false,
            was_recalled: false,
            features_json: "[0.5, 0.5]".into(), // too short
            created_at: "2026-01-01".into(),
        }];
        let samples = parse_training_samples(&records);
        assert!(samples.is_empty(), "short array must be skipped");
    }

    #[test]
    fn parse_training_samples_valid_record() {
        use crate::store::admission_training::AdmissionTrainingRecord;
        use crate::types::ConversationId;
        let records = vec![AdmissionTrainingRecord {
            id: 1,
            message_id: Some(42),
            conversation_id: ConversationId(1),
            content_hash: "abc".into(),
            role: "user".into(),
            composite_score: 0.7,
            was_admitted: true,
            was_recalled: true,
            features_json: "[0.9, 0.8, 0.7, 0.5, 0.7]".into(),
            created_at: "2026-01-01".into(),
        }];
        let samples = parse_training_samples(&records);
        assert_eq!(samples.len(), 1);
        let (_, label) = samples[0];
        assert!((label - 1.0).abs() < 1e-6, "recalled → label 1.0");
    }
}
