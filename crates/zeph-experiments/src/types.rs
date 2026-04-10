// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};

/// A single-parameter variation: the parameter to change and its candidate value.
///
/// A [`Variation`] represents one experiment arm — it captures exactly which
/// [`ParameterKind`] is being tested and the candidate [`VariationValue`].
/// The experiment engine compares scores between the baseline and a snapshot
/// produced by applying this variation.
///
/// # Examples
///
/// ```rust
/// use zeph_experiments::{Variation, ParameterKind, VariationValue};
///
/// let v = Variation {
///     parameter: ParameterKind::Temperature,
///     value: VariationValue::from(0.8_f64),
/// };
/// assert_eq!(v.parameter.as_str(), "temperature");
/// assert!((v.value.as_f64() - 0.8).abs() < f64::EPSILON);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Variation {
    /// The parameter being varied.
    pub parameter: ParameterKind,
    /// The candidate value for this variation.
    pub value: VariationValue,
}

/// Identifies a tunable parameter in the experiment search space.
///
/// Each variant corresponds to a field in [`ConfigSnapshot`] and maps to a
/// named key in [`SearchSpace`] via [`ParameterKind::as_str`].
///
/// The enum is `#[non_exhaustive]` — new parameters may be added in future
/// versions without a breaking change.
///
/// # Examples
///
/// ```rust
/// use zeph_experiments::ParameterKind;
///
/// assert_eq!(ParameterKind::Temperature.as_str(), "temperature");
/// assert!(ParameterKind::TopK.is_integer());
/// assert!(!ParameterKind::TopP.is_integer());
/// ```
///
/// [`ConfigSnapshot`]: crate::ConfigSnapshot
/// [`SearchSpace`]: crate::SearchSpace
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterKind {
    /// LLM sampling temperature (float, typically `[0.0, 2.0]`).
    Temperature,
    /// Top-p (nucleus) sampling probability (float, `[0.0, 1.0]`).
    TopP,
    /// Top-k sampling cutoff (integer).
    TopK,
    /// Frequency penalty applied to already-seen tokens (float, `[-2.0, 2.0]`).
    FrequencyPenalty,
    /// Presence penalty applied to already-seen topics (float, `[-2.0, 2.0]`).
    PresencePenalty,
    /// Number of memory chunks to retrieve per query (integer).
    RetrievalTopK,
    /// Minimum cosine similarity score for cross-session memory recall (float).
    SimilarityThreshold,
    /// Half-life in days for temporal memory decay (float).
    TemporalDecay,
}

impl ParameterKind {
    /// Return the canonical snake_case name of this parameter.
    ///
    /// The returned string matches the key used in config files and experiment
    /// storage. It is identical to the `#[serde(rename_all = "snake_case")]`
    /// serialization form.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::ParameterKind;
    ///
    /// assert_eq!(ParameterKind::FrequencyPenalty.as_str(), "frequency_penalty");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        #[allow(unreachable_patterns)]
        match self {
            Self::Temperature => "temperature",
            Self::TopP => "top_p",
            Self::TopK => "top_k",
            Self::FrequencyPenalty => "frequency_penalty",
            Self::PresencePenalty => "presence_penalty",
            Self::RetrievalTopK => "retrieval_top_k",
            Self::SimilarityThreshold => "similarity_threshold",
            Self::TemporalDecay => "temporal_decay",
            _ => "unknown",
        }
    }

    /// Returns `true` if this parameter has integer semantics.
    ///
    /// Integer parameters produce a [`VariationValue::Int`] in `ConfigSnapshot::diff`
    /// and are rounded before being applied to generation overrides.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::ParameterKind;
    ///
    /// assert!(ParameterKind::TopK.is_integer());
    /// assert!(ParameterKind::RetrievalTopK.is_integer());
    /// assert!(!ParameterKind::Temperature.is_integer());
    /// ```
    #[must_use]
    pub fn is_integer(&self) -> bool {
        matches!(self, Self::TopK | Self::RetrievalTopK)
    }
}

impl std::fmt::Display for ParameterKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The value for a single parameter variation.
///
/// Floating-point values use [`ordered_float::OrderedFloat`] to support hashing
/// and equality, which are required for deduplication via [`std::collections::HashSet`].
///
/// # Examples
///
/// ```rust
/// use zeph_experiments::VariationValue;
///
/// let f = VariationValue::from(0.7_f64);
/// let i = VariationValue::from(40_i64);
///
/// assert!((f.as_f64() - 0.7).abs() < f64::EPSILON);
/// assert_eq!(i.as_f64(), 40.0);
/// assert_eq!(i.to_string(), "40");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum VariationValue {
    /// A floating-point parameter value.
    Float(OrderedFloat<f64>),
    /// An integer parameter value (used for `TopK`, `RetrievalTopK`).
    Int(i64),
}

impl VariationValue {
    /// Return the value as `f64`.
    ///
    /// `Int` variants are cast to `f64` via `as f64` (possible precision loss for
    /// very large integers, but parameter values are always small).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::VariationValue;
    ///
    /// assert!((VariationValue::from(0.5_f64).as_f64() - 0.5).abs() < f64::EPSILON);
    /// assert_eq!(VariationValue::from(10_i64).as_f64(), 10.0);
    /// ```
    #[must_use]
    pub fn as_f64(&self) -> f64 {
        match self {
            Self::Float(f) => f.into_inner(),
            #[allow(clippy::cast_precision_loss)]
            Self::Int(i) => *i as f64,
        }
    }
}

impl From<f64> for VariationValue {
    fn from(v: f64) -> Self {
        Self::Float(OrderedFloat(v))
    }
}

impl From<i64> for VariationValue {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl std::fmt::Display for VariationValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Float(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
        }
    }
}

/// Persisted record of a single variation trial.
///
/// Each time [`ExperimentEngine`] evaluates a candidate variation, it produces an
/// `ExperimentResult` that is stored in SQLite (when memory is configured) and
/// included in the [`ExperimentSessionReport`].
///
/// [`ExperimentEngine`]: crate::ExperimentEngine
/// [`ExperimentSessionReport`]: crate::engine::ExperimentSessionReport
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentResult {
    /// Row ID in the SQLite experiments table (`-1` when not yet persisted).
    pub id: i64,
    /// UUID of the experiment session that produced this result.
    pub session_id: String,
    /// The parameter variation that was tested.
    pub variation: Variation,
    /// Mean score of the current progressive baseline before this variation was tested.
    pub baseline_score: f64,
    /// Mean score achieved by the candidate configuration.
    pub candidate_score: f64,
    /// `candidate_score - baseline_score` (positive means improvement).
    pub delta: f64,
    /// Wall-clock latency for the candidate evaluation in milliseconds.
    pub latency_ms: u64,
    /// Total tokens consumed by judge calls during the candidate evaluation.
    pub tokens_used: u64,
    /// Whether this variation was accepted as the new baseline.
    pub accepted: bool,
    /// How this experiment was triggered.
    pub source: ExperimentSource,
    /// ISO-8601 timestamp when the result was recorded.
    pub created_at: String,
}

/// How an experiment session was initiated.
///
/// # Examples
///
/// ```rust
/// use zeph_experiments::ExperimentSource;
///
/// assert_eq!(ExperimentSource::Manual.as_str(), "manual");
/// assert_eq!(ExperimentSource::Scheduled.to_string(), "scheduled");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentSource {
    /// Started by the user (CLI, TUI, or API call).
    Manual,
    /// Started automatically by `zeph-scheduler` on a cron schedule.
    Scheduled,
}

impl ExperimentSource {
    /// Return the canonical snake_case name of this source.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_experiments::ExperimentSource;
    ///
    /// assert_eq!(ExperimentSource::Manual.as_str(), "manual");
    /// ```
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
    #![allow(clippy::approx_constant)]

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
    fn parameter_kind_is_integer() {
        assert!(ParameterKind::TopK.is_integer());
        assert!(ParameterKind::RetrievalTopK.is_integer());
        assert!(!ParameterKind::Temperature.is_integer());
        assert!(!ParameterKind::TopP.is_integer());
        assert!(!ParameterKind::FrequencyPenalty.is_integer());
        assert!(!ParameterKind::PresencePenalty.is_integer());
        assert!(!ParameterKind::SimilarityThreshold.is_integer());
        assert!(!ParameterKind::TemporalDecay.is_integer());
    }

    #[test]
    fn variation_value_as_f64_float() {
        let v = VariationValue::Float(OrderedFloat(3.14));
        assert!((v.as_f64() - 3.14).abs() < f64::EPSILON);
    }

    #[test]
    fn variation_value_as_f64_int() {
        let v = VariationValue::Int(42);
        assert!((v.as_f64() - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn variation_value_from_f64() {
        let v = VariationValue::from(0.7_f64);
        assert!(matches!(v, VariationValue::Float(_)));
        assert!((v.as_f64() - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn variation_value_from_i64() {
        let v = VariationValue::from(40_i64);
        assert!(matches!(v, VariationValue::Int(40)));
        assert!((v.as_f64() - 40.0).abs() < f64::EPSILON);
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
