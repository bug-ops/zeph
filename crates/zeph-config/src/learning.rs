// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::providers::ProviderName;
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

fn default_arise_min_tool_calls() -> u32 {
    2
}

fn default_stem_min_occurrences() -> u32 {
    3
}

fn default_stem_min_success_rate() -> f64 {
    0.8
}

fn default_stem_retention_days() -> u32 {
    90
}

fn default_stem_pattern_window_days() -> u32 {
    30
}

fn default_erl_max_heuristics_per_skill() -> u32 {
    3
}

fn default_erl_dedup_threshold() -> f32 {
    0.9
}

fn default_erl_min_confidence() -> f64 {
    0.5
}

fn default_d2skill_max_corrections() -> u32 {
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

/// Self-learning and skill evolution configuration, nested under `[skills.learning]` in TOML.
///
/// When `enabled = true`, Zeph tracks skill performance and can automatically improve or roll
/// back skill definitions based on usage outcomes (ARISE, STEM, `D2Skill` pipelines).
///
/// # Example (TOML)
///
/// ```toml
/// [skills.learning]
/// enabled = true
/// auto_activate = false
/// min_failures = 3
/// ```
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LearningConfig {
    /// Enable self-learning pipelines. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Automatically activate improved skill versions without user confirmation. Default: `false`.
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
    pub feedback_provider: ProviderName,
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

    // --- ARISE: trace-based skill improvement ---
    /// Enable ARISE trace-based skill improvement (disabled by default).
    #[serde(default)]
    pub arise_enabled: bool,
    /// Minimum tool calls in a turn to trigger ARISE trace improvement.
    #[serde(default = "default_arise_min_tool_calls")]
    pub arise_min_tool_calls: u32,
    /// Provider name from `[[llm.providers]]` for ARISE trace summarization.
    /// Empty = fall back to primary provider.
    #[serde(default)]
    pub arise_trace_provider: ProviderName,

    // --- STEM: pattern-to-skill conversion ---
    /// Enable STEM automatic tool pattern detection and skill generation (disabled by default).
    #[serde(default)]
    pub stem_enabled: bool,
    /// Minimum occurrences of a tool sequence before generating a skill candidate.
    #[serde(default = "default_stem_min_occurrences")]
    pub stem_min_occurrences: u32,
    /// Minimum success rate of the pattern before generating a skill candidate.
    #[serde(default = "default_stem_min_success_rate")]
    pub stem_min_success_rate: f64,
    /// Provider name from `[[llm.providers]]` for STEM skill generation.
    /// Empty = fall back to primary provider.
    #[serde(default)]
    pub stem_provider: ProviderName,
    /// Days to retain rows in `skill_usage_log` before pruning.
    #[serde(default = "default_stem_retention_days")]
    pub stem_retention_days: u32,
    /// Window in days for pattern detection queries (limits scan cost on large tables).
    #[serde(default = "default_stem_pattern_window_days")]
    pub stem_pattern_window_days: u32,

    // --- ERL: experiential reflective learning ---
    /// Enable ERL post-task heuristic extraction (disabled by default).
    #[serde(default)]
    pub erl_enabled: bool,
    /// Provider name from `[[llm.providers]]` for ERL heuristic extraction.
    /// Empty = fall back to primary provider.
    #[serde(default)]
    pub erl_extract_provider: ProviderName,
    /// Maximum heuristics prepended per skill at match time.
    #[serde(default = "default_erl_max_heuristics_per_skill")]
    pub erl_max_heuristics_per_skill: u32,
    /// Text similarity threshold (Jaccard) for heuristic deduplication.
    /// When exact text match exceeds this, increment `use_count` instead of inserting.
    #[serde(default = "default_erl_dedup_threshold")]
    pub erl_dedup_threshold: f32,
    /// Minimum confidence to include a heuristic at match time.
    #[serde(default = "default_erl_min_confidence")]
    pub erl_min_confidence: f64,

    // --- D2Skill: step-level error correction ---
    /// Enable `D2Skill` step-level error correction (disabled by default).
    ///
    /// Requires `arise_enabled = true` to populate corrections from ARISE traces.
    /// If `d2skill_enabled = true` and `arise_enabled = false`, existing corrections
    /// are still applied but no new ones are generated via ARISE.
    #[serde(default)]
    pub d2skill_enabled: bool,
    /// Maximum corrections to inject per failure event.
    #[serde(default = "default_d2skill_max_corrections")]
    pub d2skill_max_corrections: u32,
    /// Provider name from `[[llm.providers]]` for correction extraction from ARISE traces.
    /// Empty = fall back to primary provider.
    #[serde(default)]
    pub d2skill_provider: ProviderName,
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
            feedback_provider: ProviderName::default(),
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
            arise_enabled: false,
            arise_min_tool_calls: default_arise_min_tool_calls(),
            arise_trace_provider: ProviderName::default(),
            stem_enabled: false,
            stem_min_occurrences: default_stem_min_occurrences(),
            stem_min_success_rate: default_stem_min_success_rate(),
            stem_provider: ProviderName::default(),
            stem_retention_days: default_stem_retention_days(),
            stem_pattern_window_days: default_stem_pattern_window_days(),
            erl_enabled: false,
            erl_extract_provider: ProviderName::default(),
            erl_max_heuristics_per_skill: default_erl_max_heuristics_per_skill(),
            erl_dedup_threshold: default_erl_dedup_threshold(),
            erl_min_confidence: default_erl_min_confidence(),
            d2skill_enabled: false,
            d2skill_max_corrections: default_d2skill_max_corrections(),
            d2skill_provider: ProviderName::default(),
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
    fn arise_stem_erl_defaults() {
        let cfg = LearningConfig::default();
        assert!(!cfg.arise_enabled);
        assert_eq!(cfg.arise_min_tool_calls, 2);
        assert!(cfg.arise_trace_provider.is_empty());
        assert!(!cfg.stem_enabled);
        assert_eq!(cfg.stem_min_occurrences, 3);
        assert!((cfg.stem_min_success_rate - 0.8).abs() < f64::EPSILON);
        assert!(cfg.stem_provider.is_empty());
        assert_eq!(cfg.stem_retention_days, 90);
        assert_eq!(cfg.stem_pattern_window_days, 30);
        assert!(!cfg.erl_enabled);
        assert!(cfg.erl_extract_provider.is_empty());
        assert_eq!(cfg.erl_max_heuristics_per_skill, 3);
        assert!((cfg.erl_dedup_threshold - 0.9).abs() < f32::EPSILON);
        assert!((cfg.erl_min_confidence - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn arise_stem_erl_serde_roundtrip() {
        let toml = r#"
arise_enabled = true
arise_min_tool_calls = 3
arise_trace_provider = "fast"
stem_enabled = true
stem_min_occurrences = 5
stem_min_success_rate = 0.9
stem_provider = "mid"
stem_retention_days = 60
stem_pattern_window_days = 14
erl_enabled = true
erl_extract_provider = "fast"
erl_max_heuristics_per_skill = 5
erl_dedup_threshold = 0.85
erl_min_confidence = 0.6
"#;
        let cfg: LearningConfig = toml::from_str(toml).unwrap();
        assert!(cfg.arise_enabled);
        assert_eq!(cfg.arise_min_tool_calls, 3);
        assert_eq!(cfg.arise_trace_provider, "fast");
        assert!(cfg.stem_enabled);
        assert_eq!(cfg.stem_min_occurrences, 5);
        assert!((cfg.stem_min_success_rate - 0.9).abs() < f64::EPSILON);
        assert_eq!(cfg.stem_provider, "mid");
        assert_eq!(cfg.stem_retention_days, 60);
        assert_eq!(cfg.stem_pattern_window_days, 14);
        assert!(cfg.erl_enabled);
        assert_eq!(cfg.erl_extract_provider, "fast");
        assert_eq!(cfg.erl_max_heuristics_per_skill, 5);
        assert!((cfg.erl_dedup_threshold - 0.85_f32).abs() < f32::EPSILON);
        assert!((cfg.erl_min_confidence - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn arise_stem_erl_empty_section_uses_defaults() {
        let cfg: LearningConfig = toml::from_str("").unwrap();
        assert!(!cfg.arise_enabled);
        assert!(!cfg.stem_enabled);
        assert!(!cfg.erl_enabled);
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
