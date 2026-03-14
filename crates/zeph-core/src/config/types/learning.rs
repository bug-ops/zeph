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
}

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
            judge_adaptive_low: default_judge_adaptive_low(),
            judge_adaptive_high: default_judge_adaptive_high(),
            correction_recall_limit: default_correction_recall_limit(),
            correction_min_similarity: default_correction_min_similarity(),
            auto_promote_min_uses: default_auto_promote_min_uses(),
            auto_promote_threshold: default_auto_promote_threshold(),
            auto_demote_min_uses: default_auto_demote_min_uses(),
            auto_demote_threshold: default_auto_demote_threshold(),
        }
    }
}
