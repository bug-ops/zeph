// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Config snapshot for a single experiment arm.

use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
pub use zeph_llm::provider::GenerationOverrides;

use super::types::{ParameterKind, Variation, VariationValue};

/// Snapshot of all tunable parameters for a single experiment arm.
///
/// `ConfigSnapshot` is the bridge between Zeph's runtime `Config` and the
/// variation engine. Each experiment arm is defined by a snapshot derived from
/// the baseline config with exactly one parameter changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: f64,
    pub frequency_penalty: f64,
    pub presence_penalty: f64,
    pub retrieval_top_k: f64,
    pub similarity_threshold: f64,
    pub temporal_decay: f64,
}

impl Default for ConfigSnapshot {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40.0,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
            retrieval_top_k: 5.0,
            similarity_threshold: 0.35,
            temporal_decay: 30.0,
        }
    }
}

impl ConfigSnapshot {
    /// Create a snapshot from the current runtime config.
    ///
    /// LLM generation parameters come from `config.llm.candle.generation` when
    /// a Candle provider is configured. All other providers do not expose
    /// generation params in config — defaults are used for the experiment baseline.
    /// Memory parameters are read from `config.memory.semantic`.
    #[must_use]
    pub fn from_config(config: &crate::config::Config) -> Self {
        let (temperature, top_p, top_k) = config.llm.candle.as_ref().map_or_else(
            || {
                tracing::debug!(
                    provider = %config.llm.provider,
                    "LLM generation params not available for this provider; \
                    using defaults for experiment baseline (temperature=0.7, top_p=0.9, top_k=40)"
                );
                (0.7, 0.9, 40.0)
            },
            |c| {
                (
                    c.generation.temperature,
                    c.generation.top_p.unwrap_or(0.9),
                    #[allow(clippy::cast_precision_loss)]
                    c.generation.top_k.map_or(40.0, |k| k as f64),
                )
            },
        );

        Self {
            temperature,
            top_p,
            top_k,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
            #[allow(clippy::cast_precision_loss)]
            retrieval_top_k: config.memory.semantic.recall_limit as f64,
            similarity_threshold: f64::from(config.memory.cross_session_score_threshold),
            temporal_decay: f64::from(config.memory.semantic.temporal_decay_half_life_days),
        }
    }

    /// Apply a single variation and return a new snapshot with that parameter changed.
    #[must_use]
    pub fn apply(&self, variation: &Variation) -> Self {
        let mut snapshot = self.clone();
        snapshot.set(variation.parameter, variation.value.as_f64());
        snapshot
    }

    /// Return the single `Variation` that differs between `self` and `other`, or `None`
    /// if zero or more than one parameter differs.
    ///
    /// Integer parameters (`TopK`, `RetrievalTopK`) produce a [`VariationValue::Int`] variant.
    #[must_use]
    pub fn diff(&self, other: &ConfigSnapshot) -> Option<Variation> {
        let kinds = [
            ParameterKind::Temperature,
            ParameterKind::TopP,
            ParameterKind::TopK,
            ParameterKind::FrequencyPenalty,
            ParameterKind::PresencePenalty,
            ParameterKind::RetrievalTopK,
            ParameterKind::SimilarityThreshold,
            ParameterKind::TemporalDecay,
        ];
        let mut result = None;
        for kind in kinds {
            let a = self.get(kind);
            let b = other.get(kind);
            if (a - b).abs() > f64::EPSILON {
                if result.is_some() {
                    return None; // more than one diff
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let value = if kind.is_integer() {
                    VariationValue::Int(b.round() as i64)
                } else {
                    VariationValue::Float(OrderedFloat(b))
                };
                result = Some(Variation {
                    parameter: kind,
                    value,
                });
            }
        }
        result
    }

    /// Get the value of a parameter by kind.
    #[must_use]
    pub fn get(&self, kind: ParameterKind) -> f64 {
        #[allow(unreachable_patterns)]
        match kind {
            ParameterKind::Temperature => self.temperature,
            ParameterKind::TopP => self.top_p,
            ParameterKind::TopK => self.top_k,
            ParameterKind::FrequencyPenalty => self.frequency_penalty,
            ParameterKind::PresencePenalty => self.presence_penalty,
            ParameterKind::RetrievalTopK => self.retrieval_top_k,
            ParameterKind::SimilarityThreshold => self.similarity_threshold,
            ParameterKind::TemporalDecay => self.temporal_decay,
            _ => 0.0,
        }
    }

    /// Set the value of a parameter by kind.
    pub fn set(&mut self, kind: ParameterKind, value: f64) {
        #[allow(unreachable_patterns)]
        match kind {
            ParameterKind::Temperature => self.temperature = value,
            ParameterKind::TopP => self.top_p = value,
            ParameterKind::TopK => self.top_k = value,
            ParameterKind::FrequencyPenalty => self.frequency_penalty = value,
            ParameterKind::PresencePenalty => self.presence_penalty = value,
            ParameterKind::RetrievalTopK => self.retrieval_top_k = value,
            ParameterKind::SimilarityThreshold => self.similarity_threshold = value,
            ParameterKind::TemporalDecay => self.temporal_decay = value,
            _ => {}
        }
    }

    /// Extract LLM-relevant parameter overrides for use by the experiment engine.
    ///
    /// Uses `.round() as usize` for `top_k` to avoid truncation from floating-point noise.
    #[must_use]
    pub fn to_generation_overrides(&self) -> GenerationOverrides {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        GenerationOverrides {
            temperature: Some(self.temperature),
            top_p: Some(self.top_p),
            top_k: Some(self.top_k.round() as usize),
            frequency_penalty: Some(self.frequency_penalty),
            presence_penalty: Some(self.presence_penalty),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::field_reassign_with_default,
        clippy::semicolon_if_nothing_returned,
        clippy::type_complexity
    )]

    use super::*;
    use ordered_float::OrderedFloat;

    #[test]
    fn default_snapshot_fields() {
        let s = ConfigSnapshot::default();
        assert!((s.temperature - 0.7).abs() < f64::EPSILON);
        assert!((s.top_p - 0.9).abs() < f64::EPSILON);
        assert!((s.top_k - 40.0).abs() < f64::EPSILON);
        assert!((s.frequency_penalty - 0.0).abs() < f64::EPSILON);
        assert!((s.presence_penalty - 0.0).abs() < f64::EPSILON);
        assert!((s.retrieval_top_k - 5.0).abs() < f64::EPSILON);
        assert!((s.similarity_threshold - 0.35).abs() < 1e-6);
        assert!((s.temporal_decay - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_changes_single_param() {
        let baseline = ConfigSnapshot::default();
        let variation = Variation {
            parameter: ParameterKind::Temperature,
            value: VariationValue::Float(OrderedFloat(1.0)),
        };
        let applied = baseline.apply(&variation);
        assert!((applied.temperature - 1.0).abs() < f64::EPSILON);
        assert!((applied.top_p - 0.9).abs() < f64::EPSILON); // unchanged
    }

    #[test]
    fn apply_with_int_value() {
        let baseline = ConfigSnapshot::default();
        let variation = Variation {
            parameter: ParameterKind::TopK,
            value: VariationValue::Int(50),
        };
        let applied = baseline.apply(&variation);
        assert!((applied.top_k - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn diff_returns_single_changed_param() {
        let a = ConfigSnapshot::default();
        let mut b = ConfigSnapshot::default();
        b.temperature = 1.0;
        let variation = a.diff(&b);
        assert!(variation.is_some());
        let v = variation.unwrap();
        assert_eq!(v.parameter, ParameterKind::Temperature);
        assert!((v.value.as_f64() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn diff_returns_none_for_identical_snapshots() {
        let a = ConfigSnapshot::default();
        let b = ConfigSnapshot::default();
        assert!(a.diff(&b).is_none());
    }

    #[test]
    fn diff_returns_none_for_multiple_changes() {
        let a = ConfigSnapshot::default();
        let mut b = ConfigSnapshot::default();
        b.temperature = 1.0;
        b.top_p = 0.5;
        assert!(a.diff(&b).is_none());
    }

    #[test]
    fn get_all_kinds() {
        let s = ConfigSnapshot {
            temperature: 0.1,
            top_p: 0.2,
            top_k: 3.0,
            frequency_penalty: 0.4,
            presence_penalty: 0.5,
            retrieval_top_k: 6.0,
            similarity_threshold: 0.7,
            temporal_decay: 8.0,
        };
        assert!((s.get(ParameterKind::Temperature) - 0.1).abs() < f64::EPSILON);
        assert!((s.get(ParameterKind::TopP) - 0.2).abs() < f64::EPSILON);
        assert!((s.get(ParameterKind::TopK) - 3.0).abs() < f64::EPSILON);
        assert!((s.get(ParameterKind::FrequencyPenalty) - 0.4).abs() < f64::EPSILON);
        assert!((s.get(ParameterKind::PresencePenalty) - 0.5).abs() < f64::EPSILON);
        assert!((s.get(ParameterKind::RetrievalTopK) - 6.0).abs() < f64::EPSILON);
        assert!((s.get(ParameterKind::SimilarityThreshold) - 0.7).abs() < f64::EPSILON);
        assert!((s.get(ParameterKind::TemporalDecay) - 8.0).abs() < f64::EPSILON);
    }

    #[test]
    fn set_all_kinds() {
        let mut s = ConfigSnapshot::default();
        s.set(ParameterKind::Temperature, 1.1);
        s.set(ParameterKind::TopP, 0.8);
        s.set(ParameterKind::TopK, 20.0);
        s.set(ParameterKind::FrequencyPenalty, -0.5);
        s.set(ParameterKind::PresencePenalty, 0.3);
        s.set(ParameterKind::RetrievalTopK, 10.0);
        s.set(ParameterKind::SimilarityThreshold, 0.5);
        s.set(ParameterKind::TemporalDecay, 60.0);
        assert!((s.temperature - 1.1).abs() < f64::EPSILON);
        assert!((s.top_p - 0.8).abs() < f64::EPSILON);
        assert!((s.top_k - 20.0).abs() < f64::EPSILON);
        assert!((s.frequency_penalty + 0.5).abs() < f64::EPSILON);
        assert!((s.presence_penalty - 0.3).abs() < f64::EPSILON);
        assert!((s.retrieval_top_k - 10.0).abs() < f64::EPSILON);
        assert!((s.similarity_threshold - 0.5).abs() < f64::EPSILON);
        assert!((s.temporal_decay - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn to_generation_overrides_rounds_top_k() {
        let mut s = ConfigSnapshot::default();
        // top_k = 39.9 must round to 40, not truncate to 39
        s.top_k = 39.9;
        let overrides = s.to_generation_overrides();
        assert_eq!(overrides.top_k, Some(40));
    }

    #[test]
    fn to_generation_overrides_contains_all_llm_fields() {
        let s = ConfigSnapshot::default();
        let overrides = s.to_generation_overrides();
        assert!(overrides.temperature.is_some());
        assert!(overrides.top_p.is_some());
        assert!(overrides.top_k.is_some());
        assert!(overrides.frequency_penalty.is_some());
        assert!(overrides.presence_penalty.is_some());
    }

    #[test]
    fn diff_integer_param_produces_int_value() {
        let a = ConfigSnapshot::default();
        let mut b = ConfigSnapshot::default();
        b.top_k = 50.0;
        let variation = a.diff(&b).expect("should have one diff");
        assert_eq!(variation.parameter, ParameterKind::TopK);
        assert!(
            matches!(variation.value, VariationValue::Int(50)),
            "expected Int(50), got {:?}",
            variation.value
        );
    }

    #[test]
    fn diff_retrieval_top_k_produces_int_value() {
        let a = ConfigSnapshot::default();
        let mut b = ConfigSnapshot::default();
        b.retrieval_top_k = 10.0;
        let variation = a.diff(&b).expect("should have one diff");
        assert_eq!(variation.parameter, ParameterKind::RetrievalTopK);
        assert!(matches!(variation.value, VariationValue::Int(10)));
    }

    #[test]
    fn diff_all_eight_kinds() {
        let fields: &[(ParameterKind, fn(&mut ConfigSnapshot))] = &[
            (ParameterKind::Temperature, |s| s.temperature = 1.5),
            (ParameterKind::TopP, |s| s.top_p = 0.5),
            (ParameterKind::TopK, |s| s.top_k = 20.0),
            (ParameterKind::FrequencyPenalty, |s| {
                s.frequency_penalty = 0.5;
            }),
            (ParameterKind::PresencePenalty, |s| s.presence_penalty = 0.5),
            (ParameterKind::RetrievalTopK, |s| s.retrieval_top_k = 10.0),
            (ParameterKind::SimilarityThreshold, |s| {
                s.similarity_threshold = 0.8;
            }),
            (ParameterKind::TemporalDecay, |s| s.temporal_decay = 60.0),
        ];
        for (kind, mutate) in fields {
            let a = ConfigSnapshot::default();
            let mut b = ConfigSnapshot::default();
            mutate(&mut b);
            let v = a
                .diff(&b)
                .unwrap_or_else(|| panic!("expected diff for {kind:?}"));
            assert_eq!(v.parameter, *kind);
        }
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let s = ConfigSnapshot {
            temperature: 1.2,
            top_p: 0.85,
            top_k: 50.0,
            frequency_penalty: -0.1,
            presence_penalty: 0.2,
            retrieval_top_k: 7.0,
            similarity_threshold: 0.4,
            temporal_decay: 45.0,
        };
        let json = serde_json::to_string(&s).unwrap();
        let s2: ConfigSnapshot = serde_json::from_str(&json).unwrap();
        assert!((s2.temperature - s.temperature).abs() < f64::EPSILON);
        assert!((s2.top_k - s.top_k).abs() < f64::EPSILON);
    }
}
