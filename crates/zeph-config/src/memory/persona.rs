// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Persona, trajectory, and risk-accumulator configuration.
//!
//! Persona inference, trajectory categorization/tree building, sidequest cursors,
//! and the `TrajectoryRiskAccumulator` signal-weighting model.

use crate::defaults::default_true;
use crate::providers::ProviderName;
use serde::{Deserialize, Serialize};

fn default_sidequest_interval_turns() -> u32 {
    4
}

fn default_sidequest_max_eviction_ratio() -> f32 {
    0.5
}

fn default_sidequest_max_cursors() -> usize {
    30
}

fn default_sidequest_min_cursor_tokens() -> usize {
    100
}

/// Configuration for LLM-driven side-thread tool output eviction (#1885).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SidequestConfig {
    /// Enable `SideQuest` eviction. Default: `false`.
    pub enabled: bool,
    /// Run eviction every N user turns. Default: `4`.
    #[serde(default = "default_sidequest_interval_turns")]
    pub interval_turns: u32,
    /// Maximum fraction of tool outputs to evict per pass. Default: `0.5`.
    #[serde(default = "default_sidequest_max_eviction_ratio")]
    pub max_eviction_ratio: f32,
    /// Maximum cursor entries in eviction prompt (largest outputs first). Default: `30`.
    #[serde(default = "default_sidequest_max_cursors")]
    pub max_cursors: usize,
    /// Exclude tool outputs smaller than this token count from eviction candidates.
    /// Default: `100`.
    #[serde(default = "default_sidequest_min_cursor_tokens")]
    pub min_cursor_tokens: usize,
}

impl Default for SidequestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_turns: default_sidequest_interval_turns(),
            max_eviction_ratio: default_sidequest_max_eviction_ratio(),
            max_cursors: default_sidequest_max_cursors(),
            min_cursor_tokens: default_sidequest_min_cursor_tokens(),
        }
    }
}

/// Persona memory layer configuration (#2461).
///
/// When `enabled = true`, user preferences and domain knowledge are extracted from
/// conversation history via a cheap LLM provider and injected after the system prompt.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PersonaConfig {
    /// Enable persona memory extraction and injection. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for persona extraction.
    /// Should be a cheap/fast model. Falls back to the primary provider when empty.
    pub persona_provider: ProviderName,
    /// Minimum confidence threshold for facts included in context. Default: `0.6`.
    pub min_confidence: f64,
    /// Minimum user messages before extraction runs in a session. Default: `3`.
    pub min_messages: usize,
    /// Maximum messages sent to the LLM per extraction pass. Default: `10`.
    pub max_messages: usize,
    /// LLM timeout for the extraction call in seconds. Default: `10`.
    pub extraction_timeout_secs: u64,
    /// Token budget allocated to persona context in assembly. Default: `500`.
    pub context_budget_tokens: usize,
}

impl Default for PersonaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            persona_provider: ProviderName::default(),
            min_confidence: 0.6,
            min_messages: 3,
            max_messages: 10,
            extraction_timeout_secs: 10,
            context_budget_tokens: 500,
        }
    }
}

/// Trajectory-informed memory configuration (#2498).
///
/// When `enabled = true`, tool-call turns are analyzed by a fast LLM provider to extract
/// procedural (reusable how-to) and episodic (one-off event) entries stored per-conversation.
/// Procedural entries are injected into context as "past experience" during assembly.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TrajectoryConfig {
    /// Enable trajectory extraction and context injection. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for extraction.
    /// Should be a fast/cheap model. Falls back to the primary provider when empty.
    pub trajectory_provider: ProviderName,
    /// Token budget allocated to trajectory hints in context assembly. Default: `400`.
    pub context_budget_tokens: usize,
    /// Maximum messages fed to the extraction LLM per pass. Default: `10`.
    pub max_messages: usize,
    /// LLM timeout for the extraction call in seconds. Default: `10`.
    pub extraction_timeout_secs: u64,
    /// Number of procedural entries retrieved for context injection. Default: `5`.
    pub recall_top_k: usize,
    /// Minimum confidence score for entries included in context. Default: `0.6`.
    pub min_confidence: f64,
}

impl Default for TrajectoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trajectory_provider: ProviderName::default(),
            context_budget_tokens: 400,
            max_messages: 10,
            extraction_timeout_secs: 10,
            recall_top_k: 5,
            min_confidence: 0.6,
        }
    }
}

/// Category-aware memory configuration (#2428).
///
/// When `enabled = true`, messages are auto-tagged with a category derived from the active
/// skill or tool context. The category is stored in the `messages.category` column and used
/// as a Qdrant payload filter during recall.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CategoryConfig {
    /// Enable category tagging and category-filtered recall. Default: `false`.
    pub enabled: bool,
    /// Automatically assign category from skill metadata or tool type. Default: `true`.
    pub auto_tag: bool,
}

impl Default for CategoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_tag: true,
        }
    }
}

/// `TiMem` temporal-hierarchical memory tree configuration (#2262).
///
/// When `enabled = true`, memories are stored as leaf nodes and periodically consolidated
/// into hierarchical summaries by a background loop. Context assembly uses tree traversal
/// for complex queries.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TreeConfig {
    /// Enable the memory tree and background consolidation loop. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for node consolidation.
    /// Should be a fast/cheap model. Falls back to the primary provider when empty.
    pub consolidation_provider: ProviderName,
    /// Interval between consolidation sweeps in seconds. Default: `300`.
    pub sweep_interval_secs: u64,
    /// Maximum leaf nodes loaded per sweep batch. Default: `20`.
    pub batch_size: usize,
    /// Cosine similarity threshold for clustering leaves. Default: `0.8`.
    pub similarity_threshold: f32,
    /// Maximum tree depth (levels above leaves). Default: `3`.
    pub max_level: u32,
    /// Token budget allocated to tree memory in context assembly. Default: `400`.
    pub context_budget_tokens: usize,
    /// Number of tree nodes retrieved for context. Default: `5`.
    pub recall_top_k: usize,
    /// Minimum cluster size before triggering LLM consolidation. Default: `2`.
    pub min_cluster_size: usize,
}

impl Default for TreeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            consolidation_provider: ProviderName::default(),
            sweep_interval_secs: 300,
            batch_size: 20,
            similarity_threshold: 0.8,
            max_level: 3,
            context_budget_tokens: 400,
            recall_top_k: 5,
            min_cluster_size: 2,
        }
    }
}

// ── TrajectoryRiskAccumulator config (spec 004-16) ─────────────────────────────

fn validate_tra_nonneg_weight<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f64 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() || value < 0.0 {
        return Err(serde::de::Error::custom(
            "signal weight and severity multiplier values must be finite and non-negative",
        ));
    }
    Ok(value)
}

/// Per-signal-type base weights for the trajectory risk accumulator.
///
/// Each weight is in `(0.0, 1.0]` and is multiplied by the severity multiplier
/// before being added to `trajectory_risk`.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.shadow_memory.signal_weights]
/// prompt_injection = 0.6
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectorySignalWeights {
    /// Weight for `PolicyViolation` signals. Default: `0.30`.
    #[serde(
        default = "default_sw_policy_violation",
        deserialize_with = "validate_tra_nonneg_weight"
    )]
    pub policy_violation: f64,
    /// Weight for `PromptInjectionPattern` signals. Default: `0.50`.
    #[serde(
        default = "default_sw_prompt_injection",
        deserialize_with = "validate_tra_nonneg_weight"
    )]
    pub prompt_injection: f64,
    /// Weight for `ToolChainAnomaly` signals. Default: `0.25`.
    #[serde(
        default = "default_sw_tool_chain_anomaly",
        deserialize_with = "validate_tra_nonneg_weight"
    )]
    pub tool_chain_anomaly: f64,
    /// Weight for `ConfidenceDrop` signals. Default: `0.15`.
    #[serde(
        default = "default_sw_confidence_drop",
        deserialize_with = "validate_tra_nonneg_weight"
    )]
    pub confidence_drop: f64,
}

fn default_sw_policy_violation() -> f64 {
    0.30
}

fn default_sw_prompt_injection() -> f64 {
    0.50
}

fn default_sw_tool_chain_anomaly() -> f64 {
    0.25
}

fn default_sw_confidence_drop() -> f64 {
    0.15
}

impl Default for TrajectorySignalWeights {
    fn default() -> Self {
        Self {
            policy_violation: default_sw_policy_violation(),
            prompt_injection: default_sw_prompt_injection(),
            tool_chain_anomaly: default_sw_tool_chain_anomaly(),
            confidence_drop: default_sw_confidence_drop(),
        }
    }
}

/// Per-severity multipliers applied on top of signal base weights.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.shadow_memory.severity_multipliers]
/// high = 3.0
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectorySeverityMultipliers {
    /// Multiplier for low-severity signals. Default: `0.5`.
    #[serde(
        default = "default_sev_low",
        deserialize_with = "validate_tra_nonneg_weight"
    )]
    pub low: f64,
    /// Multiplier for medium-severity signals. Default: `1.0`.
    #[serde(
        default = "default_sev_medium",
        deserialize_with = "validate_tra_nonneg_weight"
    )]
    pub medium: f64,
    /// Multiplier for high-severity signals. Default: `2.0`.
    #[serde(
        default = "default_sev_high",
        deserialize_with = "validate_tra_nonneg_weight"
    )]
    pub high: f64,
}

fn default_sev_low() -> f64 {
    0.5
}

fn default_sev_medium() -> f64 {
    1.0
}

fn default_sev_high() -> f64 {
    2.0
}

impl Default for TrajectorySeverityMultipliers {
    fn default() -> Self {
        Self {
            low: default_sev_low(),
            medium: default_sev_medium(),
            high: default_sev_high(),
        }
    }
}

/// Configuration for the MAGE trajectory risk accumulator (spec 004-16).
///
/// Controls how per-turn safety signals accumulate into a session-level risk score
/// and when tool execution is blocked or escalated.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.shadow_memory]
/// enabled = true
/// risk_threshold = 0.75
/// escalation_threshold = 0.50
/// risk_halflife_turns = 10
/// signal_history_cap = 200
/// tui_show_risk_gauge = true
/// reset_on_compaction = false
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryRiskAccumulatorConfig {
    /// Enable shadow memory. When `false`, `TrajectoryRiskAccumulator` is a zero-cost noop.
    #[serde(default)]
    pub enabled: bool,
    /// Block tool execution when `trajectory_risk >= risk_threshold`. Default: `0.75`.
    #[serde(default = "default_tra_risk_threshold")]
    pub risk_threshold: f64,
    /// Escalate to human confirmation when risk is in `[escalation_threshold, risk_threshold)`.
    /// Default: `0.50`.
    #[serde(default = "default_tra_escalation_threshold")]
    pub escalation_threshold: f64,
    /// Number of turns after which accumulated risk halves (exponential decay). Default: `10`.
    #[serde(default = "default_tra_risk_halflife_turns")]
    pub risk_halflife_turns: u32,
    /// Maximum number of signal events kept in the ring buffer. Default: `200`.
    #[serde(default = "default_tra_signal_history_cap")]
    pub signal_history_cap: usize,
    /// Show a risk gauge in the TUI security panel when the TUI is enabled. Default: `true`.
    #[serde(default = "default_true")]
    pub tui_show_risk_gauge: bool,
    /// Reset `trajectory_risk` to zero when a context compaction occurs. Default: `false`.
    #[serde(default)]
    pub reset_on_compaction: bool,
    /// Per-signal-type base weights.
    #[serde(default)]
    pub signal_weights: TrajectorySignalWeights,
    /// Per-severity multipliers applied on top of signal weights.
    #[serde(default)]
    pub severity_multipliers: TrajectorySeverityMultipliers,
}

fn default_tra_risk_threshold() -> f64 {
    0.75
}

fn default_tra_escalation_threshold() -> f64 {
    0.50
}

fn default_tra_risk_halflife_turns() -> u32 {
    10
}

fn default_tra_signal_history_cap() -> usize {
    200
}

impl TrajectoryRiskAccumulatorConfig {
    /// Validate threshold ordering after deserialization.
    ///
    /// Returns an error string if `escalation_threshold >= risk_threshold`. An
    /// inverted/equal pair silently disables the soft-escalation tier (`should_escalate`'s
    /// `[escalation_threshold, risk_threshold)` band becomes empty) — the hard block
    /// (`is_blocked`) still works, so this is a degraded-but-safe misconfiguration, not a
    /// security gap; validation exists to surface it instead of leaving it silent (critic
    /// finding F4, spec 004-16).
    ///
    /// # Errors
    ///
    /// Returns a descriptive error string when the threshold ordering invariant is violated.
    #[must_use = "validation result must be checked"]
    pub fn validate(&self) -> Result<(), String> {
        if self.escalation_threshold >= self.risk_threshold {
            return Err(format!(
                "memory.shadow_memory: escalation_threshold ({}) must be < risk_threshold ({}) \
                 — otherwise the escalation band is empty and soft-escalation never fires",
                self.escalation_threshold, self.risk_threshold
            ));
        }
        Ok(())
    }
}

impl Default for TrajectoryRiskAccumulatorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            risk_threshold: default_tra_risk_threshold(),
            escalation_threshold: default_tra_escalation_threshold(),
            risk_halflife_turns: default_tra_risk_halflife_turns(),
            signal_history_cap: default_tra_signal_history_cap(),
            tui_show_risk_gauge: true,
            reset_on_compaction: false,
            signal_weights: TrajectorySignalWeights::default(),
            severity_multipliers: TrajectorySeverityMultipliers::default(),
        }
    }
}
