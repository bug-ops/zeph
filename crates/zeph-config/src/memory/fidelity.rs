// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compression, forgetting, and fidelity configuration.
//!
//! ACON budget compaction, ARC compaction, typed pages, optical forgetting,
//! eviction policy, pruning strategy, and compression guidelines.

use crate::providers::ProviderName;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::CompactionProbeConfig;

// ── ScrapMem optical forgetting config (issue #3713) ───────────────────────────

/// `ScrapMem` optical forgetting configuration.
///
/// Controls progressive content-fidelity decay: `Full` → `Compressed` → `SummaryOnly`.
/// The sweep is orthogonal to `SleepGate` (which decays importance scores); optical
/// forgetting compresses content in place based on age.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.optical_forgetting]
/// enabled = false
/// compress_provider = ""
/// compress_after_turns = 100
/// summarize_after_turns = 500
/// sweep_interval_secs = 3600
/// sweep_batch_size = 50
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OpticalForgettingConfig {
    /// Enable optical forgetting sweep. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for LLM-based content compression.
    /// Falls back to the primary provider when empty.
    pub compress_provider: ProviderName,
    /// Number of conversation turns after which `Full` messages are compressed. Default: `100`.
    pub compress_after_turns: u32,
    /// Number of conversation turns after which `Compressed` messages become `SummaryOnly`. Default: `500`.
    pub summarize_after_turns: u32,
    /// How often the sweep runs, in seconds. Default: `3600`.
    pub sweep_interval_secs: u64,
    /// Maximum messages to compress per sweep iteration. Default: `50`.
    pub sweep_batch_size: usize,
}

impl Default for OpticalForgettingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            compress_provider: ProviderName::default(),
            compress_after_turns: 100,
            summarize_after_turns: 500,
            sweep_interval_secs: 3600,
            sweep_batch_size: 50,
        }
    }
}

/// Session digest configuration (#2289).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DigestConfig {
    /// Enable session digest generation at session end. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for digest generation.
    /// Falls back to the primary provider when `None`.
    #[serde(default)]
    pub provider: Option<ProviderName>,
    /// Maximum tokens for the digest text. Default: `500`.
    pub max_tokens: usize,
    /// Maximum messages to feed into the digest prompt. Default: `50`.
    pub max_input_messages: usize,
}

impl Default for DigestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: None,
            max_tokens: 500,
            max_input_messages: 50,
        }
    }
}

/// Compression strategy for active context compression (#1161).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(tag = "strategy", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompressionStrategy {
    /// Compress only when reactive compaction fires (current behavior).
    #[default]
    Reactive,
    /// Compress proactively when context exceeds `threshold_tokens`.
    Proactive {
        /// Token count that triggers proactive compression.
        threshold_tokens: usize,
        /// Maximum tokens for the compressed summary (passed to LLM as `max_tokens`).
        max_summary_tokens: usize,
    },
    /// Agent calls `compress_context` tool explicitly. Reactive compaction still fires as a
    /// safety net. The `compress_context` tool is also available in all other strategies.
    Autonomous,
    /// Knowledge-block-aware compression strategy (#2510).
    ///
    /// Low-relevance context segments are automatically consolidated into `AutoConsolidated`
    /// knowledge blocks. LLM-curated blocks are never evicted before auto-consolidated ones.
    Focus,
}

/// Pruning strategy for tool-output eviction inside the compaction pipeline (#1851, #2022).
///
/// When `context-compression` feature is enabled, this replaces the default oldest-first
/// heuristic with scored eviction.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PruningStrategy {
    /// Oldest-first eviction — current default behavior.
    #[default]
    Reactive,
    /// Short LLM call extracts a task goal; blocks are scored by keyword overlap and pruned
    /// lowest-first. Requires `context-compression` feature.
    TaskAware,
    /// Coarse-to-fine MIG scoring: relevance − redundancy with temporal partitioning.
    /// Requires `context-compression` feature.
    Mig,
    /// Subgoal-aware pruning: tracks the agent's current subgoal via fire-and-forget LLM
    /// extraction and partitions tool outputs into Active/Completed/Outdated tiers (#2022).
    /// Requires `context-compression` feature.
    Subgoal,
    /// Subgoal-aware pruning combined with MIG redundancy scoring (#2022).
    /// Requires `context-compression` feature.
    SubgoalMig,
}

impl PruningStrategy {
    /// Returns `true` when the strategy is subgoal-aware (`Subgoal` or `SubgoalMig`).
    #[must_use]
    pub fn is_subgoal(self) -> bool {
        matches!(self, Self::Subgoal | Self::SubgoalMig)
    }
}

// Route serde deserialization through FromStr so that removed variants (e.g. task_aware_mig)
// emit a warning and fall back to Reactive instead of hard-erroring when found in TOML configs.
impl<'de> serde::Deserialize<'de> for PruningStrategy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for PruningStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "reactive" => Ok(Self::Reactive),
            "task_aware" | "task-aware" => Ok(Self::TaskAware),
            "mig" => Ok(Self::Mig),
            // task_aware_mig was removed (dead code — was routed to scored path only).
            // Fall back to Reactive so existing TOML configs do not hard-error on startup.
            "task_aware_mig" | "task-aware-mig" => {
                tracing::warn!(
                    "pruning strategy `task_aware_mig` has been removed; \
                     falling back to `reactive`. Use `task_aware` or `mig` instead."
                );
                Ok(Self::Reactive)
            }
            "subgoal" => Ok(Self::Subgoal),
            "subgoal_mig" | "subgoal-mig" => Ok(Self::SubgoalMig),
            other => Err(format!(
                "unknown pruning strategy `{other}`, expected \
                 reactive|task_aware|mig|subgoal|subgoal_mig"
            )),
        }
    }
}

fn default_high_density_budget() -> f32 {
    0.7
}

fn default_low_density_budget() -> f32 {
    0.3
}

/// Configuration for the `SleepGate` forgetting sweep (#2397).
///
/// When `enabled = true`, a background loop periodically decays importance scores
/// (synaptic downscaling), restores recently-accessed memories (selective replay),
/// and prunes memories below `forgetting_floor` (targeted forgetting).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ForgettingConfig {
    /// Enable the `SleepGate` forgetting sweep. Default: `false`.
    pub enabled: bool,
    /// Per-sweep decay rate applied to importance scores. Range: (0.0, 1.0). Default: `0.1`.
    pub decay_rate: f32,
    /// Importance floor below which memories are pruned. Range: [0.0, 1.0]. Default: `0.05`.
    pub forgetting_floor: f32,
    /// How often the forgetting sweep runs, in seconds. Default: `7200`.
    pub sweep_interval_secs: u64,
    /// Maximum messages to process per sweep. Default: `500`.
    pub sweep_batch_size: usize,
    /// Hours: messages accessed within this window get replay protection. Default: `24`.
    pub replay_window_hours: u32,
    /// Messages with `access_count` >= this get replay protection. Default: `3`.
    pub replay_min_access_count: u32,
    /// Hours: never prune messages accessed within this window. Default: `24`.
    pub protect_recent_hours: u32,
    /// Never prune messages with `access_count` >= this. Default: `3`.
    pub protect_min_access_count: u32,
}

impl Default for ForgettingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            decay_rate: 0.1,
            forgetting_floor: 0.05,
            sweep_interval_secs: 7200,
            sweep_batch_size: 500,
            replay_window_hours: 24,
            replay_min_access_count: 3,
            protect_recent_hours: 24,
            protect_min_access_count: 3,
        }
    }
}

/// Configuration for active context compression (#1161).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// Compression strategy.
    #[serde(flatten)]
    pub strategy: CompressionStrategy,
    /// Tool-output pruning strategy (requires `context-compression` feature).
    pub pruning_strategy: PruningStrategy,
    /// Model to use for compression summaries.
    ///
    /// Currently unused — the primary summary provider is used regardless of this value.
    /// Reserved for future per-compression model selection. Setting this field has no effect.
    pub model: String,
    /// Provider name from `[[llm.providers]]` for `compress_context` summaries.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub compress_provider: ProviderName,
    /// Compaction probe: validates summary quality before committing it (#1609).
    #[serde(default)]
    pub probe: CompactionProbeConfig,
    /// Archive tool output bodies to `SQLite` before compaction (Memex #2432).
    ///
    /// When enabled, tool output bodies in the compaction range are saved to
    /// `tool_overflow` with `archive_type = 'archive'` before summarization.
    /// The LLM summarizes placeholder messages; archived content is appended as
    /// a postfix after summarization so references survive compaction.
    /// Default: `false`.
    #[serde(default)]
    pub archive_tool_outputs: bool,
    /// Provider for Focus strategy segment scoring and the auto-consolidation extraction
    /// LLM call (#2510, #3313). Both are cheap/mid-tier tasks, so one provider suffices.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub focus_scorer_provider: ProviderName,
    /// Token-budget fraction for high-density content in density-aware compression (#2481).
    /// Must sum to 1.0 with `low_density_budget`. Default: `0.7`.
    #[serde(default = "default_high_density_budget")]
    pub high_density_budget: f32,
    /// Token-budget fraction for low-density content in density-aware compression (#2481).
    /// Must sum to 1.0 with `high_density_budget`. Default: `0.3`.
    #[serde(default = "default_low_density_budget")]
    pub low_density_budget: f32,
    /// Typed-page classification and batch-level assertion checking (#3630).
    #[serde(default)]
    pub typed_pages: TypedPagesConfig,
    /// Acon tool-result compression settings (#4021).
    ///
    /// Controls per-result and batch-level token budgets for tool outputs before they enter
    /// message history. Distinct from `[tools.compression]` (TACO), which applies regex-based
    /// rule compression at the executor level.
    #[serde(default)]
    pub acon: AconConfig,
    /// ARC agent-initiated compaction settings (#4020).
    ///
    /// When `allow_agent_compaction = true`, the agent can call the `request_compaction`
    /// internal tool to trigger context summarization on demand.
    #[serde(default)]
    pub arc: ArcCompactionConfig,
}

fn default_acon_passthrough_threshold() -> usize {
    2000
}

fn default_acon_summarize_threshold() -> usize {
    4000
}

fn default_acon_total_budget() -> usize {
    8000
}

fn validate_acon_passthrough_threshold<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <usize as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "acon.passthrough_threshold must be >= 1",
        ));
    }
    Ok(value)
}

fn validate_acon_summarize_threshold<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <usize as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "acon.summarize_threshold must be >= 1",
        ));
    }
    Ok(value)
}

fn validate_acon_total_budget<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <usize as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("acon.total_budget must be >= 1"));
    }
    Ok(value)
}

/// Token budget configuration for Acon tool-result compression (#4021).
///
/// Controls per-result and batch-level token budgets for tool outputs injected into context.
/// Distinct from `[tools.compression]` (TACO), which applies regex-based rule compression
/// at the executor level.
///
/// # Invariants
///
/// The following ordering must hold: `passthrough_threshold < summarize_threshold <= total_budget`.
/// A config where `passthrough_threshold >= summarize_threshold` would make the summarization path
/// unreachable, silently producing incorrect compression behavior.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.compression.acon]
/// enabled = true
/// passthrough_threshold = 2000
/// summarize_threshold = 4000
/// total_budget = 8000
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AconConfig {
    /// Enable Acon tool-result compression. Default: `true`.
    pub enabled: bool,
    /// Token count below which results pass through unchanged.
    /// Also the truncation target: results above this get char-truncated to this size.
    /// Must be < `summarize_threshold`. Default: `2000`.
    #[serde(default = "default_acon_passthrough_threshold")]
    #[serde(deserialize_with = "validate_acon_passthrough_threshold")]
    pub passthrough_threshold: usize,
    /// Token count above which LLM summarization should be attempted before truncation.
    /// Must be > `passthrough_threshold` and <= `total_budget`. Default: `4000`.
    #[serde(default = "default_acon_summarize_threshold")]
    #[serde(deserialize_with = "validate_acon_summarize_threshold")]
    pub summarize_threshold: usize,
    /// Maximum total tokens for all tool results in a single turn.
    /// Must be >= `summarize_threshold`. Default: `8000`.
    #[serde(default = "default_acon_total_budget")]
    #[serde(deserialize_with = "validate_acon_total_budget")]
    pub total_budget: usize,
    /// Provider name from `[[llm.providers]]` for LLM summarization of large results.
    /// Falls back to the primary provider when empty. Default: `""`.
    #[serde(default)]
    pub summarize_provider: ProviderName,
}

impl AconConfig {
    /// Validate threshold ordering invariants after deserialization.
    ///
    /// Returns an error string if `passthrough_threshold >= summarize_threshold` or
    /// `summarize_threshold > total_budget`.
    ///
    /// # Errors
    ///
    /// Returns a descriptive error string when any threshold invariant is violated.
    pub fn validate(&self) -> Result<(), String> {
        if self.passthrough_threshold >= self.summarize_threshold {
            return Err(format!(
                "acon: passthrough_threshold ({}) must be < summarize_threshold ({})",
                self.passthrough_threshold, self.summarize_threshold
            ));
        }
        if self.summarize_threshold > self.total_budget {
            return Err(format!(
                "acon: summarize_threshold ({}) must be <= total_budget ({})",
                self.summarize_threshold, self.total_budget
            ));
        }
        Ok(())
    }
}

impl Default for AconConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            passthrough_threshold: default_acon_passthrough_threshold(),
            summarize_threshold: default_acon_summarize_threshold(),
            total_budget: default_acon_total_budget(),
            summarize_provider: ProviderName::default(),
        }
    }
}

/// Configuration for ARC agent-initiated compaction (#4020).
///
/// When `allow_agent_compaction = true`, the `request_compaction` internal tool is
/// registered and the agent can call it to trigger context summarization on demand.
/// Rate limiting is handled by `CompactionState` — only one compaction fires per turn.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.compression.arc]
/// allow_agent_compaction = true
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ArcCompactionConfig {
    /// Allow the agent to request compaction via the `request_compaction` tool call.
    /// Default: `true`.
    pub allow_agent_compaction: bool,
}

impl Default for ArcCompactionConfig {
    fn default() -> Self {
        Self {
            allow_agent_compaction: true,
        }
    }
}

/// Configuration for typed-page compaction invariants (#3630).
///
/// Controls classification, batch-level assertion checking, and audit logging.
/// All behavior is disabled by default; set `enabled = true` to activate.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.compression.typed_pages]
/// enabled = true
/// enforcement = "active"
/// audit_path = ""
/// audit_channel_capacity = 256
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct TypedPagesConfig {
    /// Enable typed-page classification and batch-level assertion checking.
    /// Default: `false`.
    pub enabled: bool,
    /// Enforcement mode:
    ///
    /// - `observe`: classify and emit audit records only; no behavioral change.
    /// - `active`: classify + `SystemContext` pointer-replace + batch assertions + audit.
    ///
    /// Default: `"observe"`.
    pub enforcement: TypedPagesEnforcement,
    /// Path for JSONL audit log. Empty string resolves to `{data_dir}/audit/compaction.jsonl`.
    /// Default: `""`.
    ///
    /// # Security
    ///
    /// This field is **operator-only trusted input** read from the agent's configuration file.
    /// Write access to the config file implies file-system write access, so no additional
    /// canonicalization is enforced here. Do not expose this field to end-users or untrusted
    /// configuration sources.
    pub audit_path: String,
    /// Bounded channel capacity for the async audit writer. Default: `256`.
    pub audit_channel_capacity: usize,
}

impl Default for TypedPagesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            enforcement: TypedPagesEnforcement::Observe,
            audit_path: String::new(),
            audit_channel_capacity: 256,
        }
    }
}

/// Enforcement mode for typed-page compaction (#3630).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TypedPagesEnforcement {
    /// Classify and audit only. Zero behavioral change relative to the untyped path.
    #[default]
    Observe,
    /// Classify + pointer-replace `SystemContext` pages + batch assertions + audit.
    Active,
}

/// Time-based microcompact configuration (#2699).
///
/// When `enabled = true`, low-value tool outputs are cleared from context
/// (replaced with a sentinel string) when the session gap exceeds `gap_threshold_minutes`.
/// The most recent `keep_recent` tool messages are preserved unconditionally.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct MicrocompactConfig {
    /// Enable time-based microcompaction. Default: `false`.
    pub enabled: bool,
    /// Minimum idle gap in minutes before stale tool outputs are cleared. Default: `60`.
    pub gap_threshold_minutes: u32,
    /// Number of most recent compactable tool messages to preserve. Default: `3`.
    pub keep_recent: usize,
}

impl Default for MicrocompactConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            gap_threshold_minutes: 60,
            keep_recent: 3,
        }
    }
}

// ── Eviction config (moved from zeph-memory) ─────────────────────────────────

/// Eviction policy variant.
///
/// Serialises as `"ebbinghaus"` in TOML/JSON so existing configs remain valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum EvictionPolicy {
    /// Ebbinghaus forgetting-curve eviction.
    #[default]
    Ebbinghaus,
}

/// Configuration for the memory eviction policy.
///
/// Controls which policy runs during the periodic sweep and how many entries
/// are retained. `zeph-memory` re-exports this type from here.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvictionConfig {
    /// Eviction policy. Currently only [`EvictionPolicy::Ebbinghaus`] is supported.
    pub policy: EvictionPolicy,
    /// Maximum number of entries to retain. `0` means unlimited (eviction disabled).
    pub max_entries: usize,
    /// How often to run the eviction sweep, in seconds.
    pub sweep_interval_secs: u64,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            policy: EvictionPolicy::Ebbinghaus,
            max_entries: 0,
            sweep_interval_secs: 3600,
        }
    }
}

// ── Compression guidelines config (moved from zeph-memory) ───────────────────

/// Configuration for ACON failure-driven compression guidelines.
///
/// `zeph-memory` re-exports this type from here.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CompressionGuidelinesConfig {
    /// Enable the feature. Default: `false`.
    pub enabled: bool,
    /// Minimum unused failure pairs before triggering a guidelines update. Default: `5`.
    pub update_threshold: u16,
    /// Maximum token budget for the guidelines document. Default: `500`.
    pub max_guidelines_tokens: usize,
    /// Maximum failure pairs consumed per update cycle. Default: `10`.
    pub max_pairs_per_update: usize,
    /// Number of turns after hard compaction to watch for context loss. Default: `10`.
    pub detection_window_turns: u64,
    /// Interval in seconds between background updater checks. Default: `300`.
    pub update_interval_secs: u64,
    /// Maximum unused failure pairs to retain (cleanup policy). Default: `100`.
    pub max_stored_pairs: usize,
    /// Provider name from `[[llm.providers]]` for guidelines update LLM calls.
    /// `None` (or `Some("")`) falls back to the primary provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidelines_provider: Option<ProviderName>,
    /// Maintain separate guideline documents per content category.
    #[serde(default)]
    pub categorized_guidelines: bool,
}

impl Default for CompressionGuidelinesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            update_threshold: 5,
            max_guidelines_tokens: 500,
            max_pairs_per_update: 10,
            detection_window_turns: 10,
            update_interval_secs: 300,
            max_stored_pairs: 100,
            guidelines_provider: None,
            categorized_guidelines: false,
        }
    }
}
