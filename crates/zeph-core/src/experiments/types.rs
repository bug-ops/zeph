// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Variation {
    pub parameter: ParameterKind,
    pub value: VariationValue,
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterKind {
    Temperature,
    TopP,
    TopK,
    FrequencyPenalty,
    PresencePenalty,
    RetrievalTopK,
    SimilarityThreshold,
    TemporalDecay,
}

impl ParameterKind {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Temperature => "temperature",
            Self::TopP => "top_p",
            Self::TopK => "top_k",
            Self::FrequencyPenalty => "frequency_penalty",
            Self::PresencePenalty => "presence_penalty",
            Self::RetrievalTopK => "retrieval_top_k",
            Self::SimilarityThreshold => "similarity_threshold",
            Self::TemporalDecay => "temporal_decay",
        }
    }
}

impl std::fmt::Display for ParameterKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum VariationValue {
    Float(OrderedFloat<f64>),
    Int(i64),
}

impl std::fmt::Display for VariationValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Float(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentResult {
    pub id: i64,
    pub session_id: String,
    pub variation: Variation,
    pub baseline_score: f64,
    pub candidate_score: f64,
    pub delta: f64,
    pub latency_ms: u64,
    pub tokens_used: u64,
    pub accepted: bool,
    pub source: ExperimentSource,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentSource {
    Manual,
    Scheduled,
}

impl ExperimentSource {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Scheduled => "scheduled",
        }
    }
}

impl std::fmt::Display for ExperimentSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_kind_as_str_all_variants() {
        let cases = [
            (ParameterKind::Temperature, "temperature"),
            (ParameterKind::TopP, "top_p"),
            (ParameterKind::TopK, "top_k"),
            (ParameterKind::FrequencyPenalty, "frequency_penalty"),
            (ParameterKind::PresencePenalty, "presence_penalty"),
            (ParameterKind::RetrievalTopK, "retrieval_top_k"),
            (ParameterKind::SimilarityThreshold, "similarity_threshold"),
            (ParameterKind::TemporalDecay, "temporal_decay"),
        ];
        for (kind, expected) in cases {
            assert_eq!(kind.as_str(), expected);
            assert_eq!(kind.to_string(), expected);
        }
    }

    #[test]
    fn variation_value_float_hash_eq() {
        use std::collections::HashSet;
        let a = VariationValue::Float(OrderedFloat(0.7));
        let b = VariationValue::Float(OrderedFloat(0.7));
        let c = VariationValue::Float(OrderedFloat(0.8));
        let mut set = HashSet::new();
        set.insert(a.clone());
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    #[test]
    fn variation_serde_roundtrip() {
        let v = Variation {
            parameter: ParameterKind::Temperature,
            value: VariationValue::Float(OrderedFloat(0.7)),
        };
        let json = serde_json::to_string(&v).expect("serialize");
        let v2: Variation = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(v, v2);
    }

    #[test]
    fn experiment_source_as_str() {
        assert_eq!(ExperimentSource::Manual.as_str(), "manual");
        assert_eq!(ExperimentSource::Scheduled.as_str(), "scheduled");
        assert_eq!(ExperimentSource::Manual.to_string(), "manual");
        assert_eq!(ExperimentSource::Scheduled.to_string(), "scheduled");
    }

    #[test]
    fn variation_value_int_display() {
        let v = VariationValue::Int(42);
        assert_eq!(v.to_string(), "42");
    }

    #[test]
    fn experiment_result_serde_roundtrip() {
        let result = ExperimentResult {
            id: 1,
            session_id: "sess-abc".to_string(),
            variation: Variation {
                parameter: ParameterKind::Temperature,
                value: VariationValue::Float(OrderedFloat(0.7)),
            },
            baseline_score: 7.0,
            candidate_score: 8.0,
            delta: 1.0,
            latency_ms: 500,
            tokens_used: 1_000,
            accepted: true,
            source: ExperimentSource::Manual,
            created_at: "2026-03-07 22:00:00".to_string(),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["session_id"], "sess-abc");
        assert_eq!(parsed["accepted"], true);
        assert_eq!(parsed["source"], "manual");
        assert_eq!(parsed["variation"]["parameter"], "temperature");

        let result2: ExperimentResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result2.id, result.id);
        assert_eq!(result2.session_id, result.session_id);
        assert_eq!(result2.variation, result.variation);
        assert!(result2.accepted);
        assert_eq!(result2.source, ExperimentSource::Manual);
    }
}
