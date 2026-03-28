// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

fn default_min_failures() -> u32 {
    3
}

fn default_improve_threshold() -> f64 {
    0.7
}

fn default_rollback_threshold() -> f64 {
    0.5
}

fn default_min_evaluations() -> u32 {
    5
}

fn default_max_versions() -> u32 {
    10
}

fn default_cooldown_minutes() -> u64 {
    60
}

fn default_correction_detection() -> bool {
    true
}

fn default_correction_confidence_threshold() -> f32 {
    0.6
}

fn default_judge_adaptive_low() -> f32 {
    0.5
}

fn default_judge_adaptive_high() -> f32 {
    0.8
}

fn default_correction_recall_limit() -> u32 {
    3
}

fn default_correction_min_similarity() -> f32 {
    0.75
}

fn default_auto_promote_min_uses() -> u32 {
    50
}

fn default_auto_promote_threshold() -> f64 {
    0.95
}

fn default_auto_demote_min_uses() -> u32 {
    30
}

fn default_auto_demote_threshold() -> f64 {
    0.40
}

fn default_min_sessions_before_promote() -> u32 {
    2
}

fn default_min_sessions_before_demote() -> u32 {
    1
}

fn default_max_auto_sections() -> u32 {
    3
}

/// Strategy for detecting implicit user corrections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DetectorMode {
    /// Pattern-matching only — zero LLM calls. Default behavior.
    #[default]
    Regex,
    /// LLM-based judge for borderline / missed cases. Invoked only when
    /// regex confidence falls below `judge_adaptive_high` or regex returns None.
    ///
    /// Note: with current regex values (ExplicitRejection=0.85, SelfCorrection=0.80,
    /// Repetition=0.75, AlternativeRequest=0.70) and `adaptive_high=0.80`,
    /// `ExplicitRejection` and `SelfCorrection` bypass the judge (confidence >= `adaptive_high`),
    /// while `AlternativeRequest`, `Repetition`, and regex misses go through it.
    Judge,
    /// ML model-backed feedback classification via `LlmClassifier`.
    ///
    /// Uses the provider named in `feedback_provider` (or the primary provider if empty).
    /// Shares the same adaptive thresholds and rate limiter as `Judge` mode.
    /// Returns `JudgeVerdict` directly, preserving `kind` and `reasoning` metadata.
    ///
    /// Falls back to regex-only if the provider cannot be resolved — never fails startup.
    Model,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LearningConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub auto_activate: bool,
    #[serde(default = "default_min_failures")]
    pub min_failures: u32,
    #[serde(default = "default_improve_threshold")]
    pub improve_threshold: f64,
    #[serde(default = "default_rollback_threshold")]
    pub rollback_threshold: f64,
    #[serde(default = "default_min_evaluations")]
    pub min_evaluations: u32,
    #[serde(default = "default_max_versions")]
    pub max_versions: u32,
    #[serde(default = "default_cooldown_minutes")]
    pub cooldown_minutes: u64,
    #[serde(default = "default_correction_detection")]
    pub correction_detection: bool,
    #[serde(default = "default_correction_confidence_threshold")]
    pub correction_confidence_threshold: f32,
    /// Detector strategy: "regex" (default) or "judge".
    #[serde(default)]
    pub detector_mode: DetectorMode,
    /// Model for the judge detector (e.g. "claude-sonnet-4-6"). Empty = use primary provider.
    #[serde(default)]
    pub judge_model: String,
    /// Provider name from `[[llm.providers]]` for `detector_mode = "model"` (`LlmClassifier`).
    ///
    /// Empty = use the primary provider. Named but not found in registry = log warning,
    /// degrade to regex-only. Never fails startup.
    #[serde(default)]
    pub feedback_provider: String,
    /// Regex confidence below this value is treated as "not a correction" — judge not invoked.
    #[serde(default = "default_judge_adaptive_low")]
    pub judge_adaptive_low: f32,
    /// Regex confidence at or above this value is accepted without judge confirmation.
    #[serde(default = "default_judge_adaptive_high")]
    pub judge_adaptive_high: f32,
    #[serde(default = "default_correction_recall_limit")]
    pub correction_recall_limit: u32,
    #[serde(default = "default_correction_min_similarity")]
    pub correction_min_similarity: f32,
    #[serde(default = "default_auto_promote_min_uses")]
    pub auto_promote_min_uses: u32,
    #[serde(default = "default_auto_promote_threshold")]
    pub auto_promote_threshold: f64,
    #[serde(default = "default_auto_demote_min_uses")]
    pub auto_demote_min_uses: u32,
    #[serde(default = "default_auto_demote_threshold")]
    pub auto_demote_threshold: f64,
    /// When true, auto-promote and auto-demote decisions require the skill to have been used
    /// across at least `min_sessions_before_promote` (for promotion) or
    /// `min_sessions_before_demote` (for demotion) distinct conversation sessions.
    /// Prevents trust transitions from a single long session.
    #[serde(default)]
    pub cross_session_rollout: bool,
    /// Minimum number of distinct `conversation_id` values in `skill_outcomes` before
    /// auto-promotion is eligible. Only checked when `cross_session_rollout = true`.
    #[serde(default = "default_min_sessions_before_promote")]
    pub min_sessions_before_promote: u32,
    /// Minimum distinct sessions before auto-demotion when `cross_session_rollout = true`.
    ///
    /// Default 1 (demotion can happen after a single bad session by default). Separate from
    /// `min_sessions_before_promote` because demotion should be fast (low threshold) while
    /// promotion benefits from conservative validation (higher threshold).
    #[serde(default = "default_min_sessions_before_demote")]
    pub min_sessions_before_demote: u32,
    /// Maximum number of top-level content sections (markdown H2 headers) allowed in
    /// auto-generated skill bodies. Bodies exceeding this limit are rejected by
    /// `validate_body_sections()`.
    #[serde(default = "default_max_auto_sections")]
    pub max_auto_sections: u32,
    /// When true, auto-generated skill versions must pass a domain-conditioned evaluation
    /// before promotion. If the improved body drifts from the original skill's domain,
    /// activation is skipped (the version is still saved for manual review).
    #[serde(default)]
    pub domain_success_gate: bool,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_activate: false,
            min_failures: default_min_failures(),
            improve_threshold: default_improve_threshold(),
            rollback_threshold: default_rollback_threshold(),
            min_evaluations: default_min_evaluations(),
            max_versions: default_max_versions(),
            cooldown_minutes: default_cooldown_minutes(),
            correction_detection: default_correction_detection(),
            correction_confidence_threshold: default_correction_confidence_threshold(),
            detector_mode: DetectorMode::default(),
            judge_model: String::new(),
            feedback_provider: String::new(),
            judge_adaptive_low: default_judge_adaptive_low(),
            judge_adaptive_high: default_judge_adaptive_high(),
            correction_recall_limit: default_correction_recall_limit(),
            correction_min_similarity: default_correction_min_similarity(),
            auto_promote_min_uses: default_auto_promote_min_uses(),
            auto_promote_threshold: default_auto_promote_threshold(),
            auto_demote_min_uses: default_auto_demote_min_uses(),
            auto_demote_threshold: default_auto_demote_threshold(),
            cross_session_rollout: false,
            min_sessions_before_promote: default_min_sessions_before_promote(),
            min_sessions_before_demote: default_min_sessions_before_demote(),
            max_auto_sections: default_max_auto_sections(),
            domain_success_gate: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_mode_default_is_regex() {
        assert_eq!(DetectorMode::default(), DetectorMode::Regex);
    }

    #[test]
    fn detector_mode_serde_roundtrip() {
        for (mode, expected_str) in [
            (DetectorMode::Regex, "\"regex\""),
            (DetectorMode::Judge, "\"judge\""),
            (DetectorMode::Model, "\"model\""),
        ] {
            let serialized = serde_json::to_string(&mode).unwrap();
            assert_eq!(serialized, expected_str, "serialize {mode:?}");
            let deserialized: DetectorMode = serde_json::from_str(&serialized).unwrap();
            assert_eq!(deserialized, mode, "deserialize {mode:?}");
        }
    }

    #[test]
    fn learning_config_default_detector_mode_is_regex() {
        let cfg = LearningConfig::default();
        assert_eq!(cfg.detector_mode, DetectorMode::Regex);
    }

    #[test]
    fn learning_config_default_feedback_provider_is_empty() {
        let cfg = LearningConfig::default();
        assert!(cfg.feedback_provider.is_empty());
    }

    #[test]
    fn learning_config_deserialize_model_mode() {
        let toml = r#"detector_mode = "model"
feedback_provider = "fast""#;
        let cfg: LearningConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.detector_mode, DetectorMode::Model);
        assert_eq!(cfg.feedback_provider, "fast");
    }

    #[test]
    fn learning_config_deserialize_empty_feedback_provider() {
        let toml = r#"detector_mode = "model""#;
        let cfg: LearningConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.detector_mode, DetectorMode::Model);
        assert!(
            cfg.feedback_provider.is_empty(),
            "empty feedback_provider must default to empty string (fallback to primary)"
        );
    }

    #[test]
    fn learning_config_deserialize_empty_section_uses_defaults() {
        let cfg: LearningConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.min_failures, 3);
        assert_eq!(cfg.detector_mode, DetectorMode::Regex);
        assert!(cfg.feedback_provider.is_empty());
    }

    #[test]
    fn learning_config_defaults_for_new_fields() {
        let cfg = LearningConfig::default();
        assert!(!cfg.cross_session_rollout);
        assert_eq!(cfg.min_sessions_before_promote, 2);
        assert_eq!(cfg.max_auto_sections, 3);
        assert!(!cfg.domain_success_gate);
    }

    #[test]
    fn learning_config_min_sessions_before_demote_default() {
        let cfg = LearningConfig::default();
        assert_eq!(cfg.min_sessions_before_demote, 1);
    }

    #[test]
    fn learning_config_new_fields_serde_roundtrip() {
        let toml = r"
cross_session_rollout = true
min_sessions_before_promote = 5
min_sessions_before_demote = 2
max_auto_sections = 4
domain_success_gate = true
";
        let cfg: LearningConfig = toml::from_str(toml).unwrap();
        assert!(cfg.cross_session_rollout);
        assert_eq!(cfg.min_sessions_before_promote, 5);
        assert_eq!(cfg.min_sessions_before_demote, 2);
        assert_eq!(cfg.max_auto_sections, 4);
        assert!(cfg.domain_success_gate);
    }
}
