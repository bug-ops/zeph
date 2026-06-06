// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Episodic and five-signal SYNAPSE consolidation configuration.
//!
//! Background daemons that promote/demote and consolidate episodic memories
//! using the five-signal salience model.

use crate::providers::ProviderName;
use serde::{Deserialize, Serialize};

// ── Episodic consolidation daemon config (issue #3799) ────────────────────────

fn default_episodic_consolidation_interval_secs() -> u64 {
    1800
}

fn default_episodic_consolidation_batch_size() -> usize {
    30
}

fn default_episodic_consolidation_min_age_secs() -> u64 {
    300
}

fn default_episodic_consolidation_dedup_jaccard_threshold() -> f32 {
    0.6
}

// ── Five-signal SYNAPSE retrieval config (issue #4374) ────────────────────────

fn default_five_signal_w_recency() -> f64 {
    0.35
}

fn default_five_signal_w_relevance() -> f64 {
    0.35
}

fn default_causal_bfs_max_depth() -> u32 {
    10
}

fn default_neutral_causal_distance() -> u32 {
    5
}

fn default_novelty_decay_rate() -> f64 {
    0.1
}

fn default_five_signal_interval_seconds() -> u64 {
    7200
}

fn default_five_signal_batch_size() -> usize {
    500
}

fn default_five_signal_daemon_max_runtime_ms() -> u64 {
    30_000
}

fn default_five_signal_promotion_score_threshold() -> f64 {
    0.70
}

fn default_five_signal_demotion_score_threshold() -> f64 {
    0.20
}

fn default_five_signal_top_k_per_run() -> usize {
    500
}

/// Five-signal SYNAPSE retrieval configuration (issue #4374).
///
/// Extends SYNAPSE recall with three additional signals — access frequency, causal
/// distance, and novelty — beyond the two-signal baseline (recency + relevance).
/// All new signal weights default to `0.0`, preserving exact backward compatibility.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.five_signal]
/// enabled = true
/// w_recency   = 0.35
/// w_relevance = 0.35
/// w_frequency = 0.15
/// w_causal    = 0.10
/// w_novelty   = 0.05
///
/// [memory.five_signal.consolidation_daemon]
/// enabled = true
/// interval_seconds = 7200
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FiveSignalConfig {
    /// Master switch. When `false`, the five-signal code path contributes zero overhead.
    #[serde(default)]
    pub enabled: bool,
    /// Weight for the recency signal. Default: `0.35`.
    #[serde(default = "default_five_signal_w_recency")]
    pub w_recency: f64,
    /// Weight for the semantic relevance signal. Default: `0.35`.
    #[serde(default = "default_five_signal_w_relevance")]
    pub w_relevance: f64,
    /// Weight for the access frequency signal. Default: `0.0` (baseline-compatible).
    #[serde(default)]
    pub w_frequency: f64,
    /// Weight for the causal distance signal. Default: `0.0` (baseline-compatible).
    #[serde(default)]
    pub w_causal: f64,
    /// Weight for the novelty signal. Default: `0.0` (baseline-compatible).
    #[serde(default)]
    pub w_novelty: f64,
    /// Maximum BFS depth for causal distance computation. Default: `10`.
    #[serde(default = "default_causal_bfs_max_depth")]
    pub causal_bfs_max_depth: u32,
    /// Causal distance assigned when no goal entity is set or a fact lies beyond
    /// `causal_bfs_max_depth`. Default: `5`.
    #[serde(default = "default_neutral_causal_distance")]
    pub neutral_causal_distance: u32,
    /// Decay rate λ in `exp(-λ × days)` for the novelty signal. Default: `0.1`.
    #[serde(default = "default_novelty_decay_rate")]
    pub novelty_decay_rate: f64,
    /// Async consolidation daemon that promotes hot episodic facts to Qdrant.
    #[serde(default)]
    pub consolidation_daemon: FiveSignalConsolidationConfig,
}

impl Default for FiveSignalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            w_recency: default_five_signal_w_recency(),
            w_relevance: default_five_signal_w_relevance(),
            w_frequency: 0.0,
            w_causal: 0.0,
            w_novelty: 0.0,
            causal_bfs_max_depth: default_causal_bfs_max_depth(),
            neutral_causal_distance: default_neutral_causal_distance(),
            novelty_decay_rate: default_novelty_decay_rate(),
            consolidation_daemon: FiveSignalConsolidationConfig::default(),
        }
    }
}

/// Async consolidation daemon configuration for five-signal retrieval (issue #4374).
///
/// When `enabled = true`, a background task runs at `interval_seconds` intervals,
/// evaluates the top `top_k_per_run` episodic facts by five-signal score, promotes
/// facts above `promotion_score_threshold` to Qdrant, and demotes facts below
/// `demotion_score_threshold` to `episodic_only` tier.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.five_signal.consolidation_daemon]
/// enabled = true
/// interval_seconds = 7200
/// batch_size = 500
/// promotion_score_threshold = 0.70
/// demotion_score_threshold = 0.20
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FiveSignalConsolidationConfig {
    /// Enable the daemon. Requires the `scheduler` feature. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Interval between daemon runs in seconds. Default: `7200` (2 hours).
    #[serde(default = "default_five_signal_interval_seconds")]
    pub interval_seconds: u64,
    /// Maximum facts processed (embed + upsert) per run. Default: `500`.
    #[serde(default = "default_five_signal_batch_size")]
    pub batch_size: usize,
    /// Hard timeout per run in milliseconds. Default: `30000`.
    #[serde(default = "default_five_signal_daemon_max_runtime_ms")]
    pub daemon_max_runtime_ms: u64,
    /// Five-signal score above which a fact is promoted to Qdrant. Default: `0.70`.
    #[serde(default = "default_five_signal_promotion_score_threshold")]
    pub promotion_score_threshold: f64,
    /// Five-signal score below which a promoted fact is demoted. Default: `0.20`.
    #[serde(default = "default_five_signal_demotion_score_threshold")]
    pub demotion_score_threshold: f64,
    /// Number of episodic facts queried per run (SQL LIMIT). Must be >= `batch_size`.
    /// Default: `500`.
    #[serde(default = "default_five_signal_top_k_per_run")]
    pub top_k_per_run: usize,
}

impl Default for FiveSignalConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: default_five_signal_interval_seconds(),
            batch_size: default_five_signal_batch_size(),
            daemon_max_runtime_ms: default_five_signal_daemon_max_runtime_ms(),
            promotion_score_threshold: default_five_signal_promotion_score_threshold(),
            demotion_score_threshold: default_five_signal_demotion_score_threshold(),
            top_k_per_run: default_five_signal_top_k_per_run(),
        }
    }
}

/// Episodic-to-semantic consolidation daemon configuration (issue #3799).
///
/// When `enabled = true`, a background loop periodically sweeps mature `episodic_events`,
/// extracts durable factual statements via LLM, deduplicates them against existing
/// key facts using Jaccard similarity, and promotes accepted facts to the semantic tier
/// in both `consolidated_facts` (`SQLite` persistence) and `zeph_key_facts` (Qdrant, if available).
///
/// # Example (TOML)
///
/// ```toml
/// [memory.episodic_consolidation]
/// enabled = false
/// consolidation_provider = ""
/// interval_secs = 1800
/// batch_size = 30
/// min_age_secs = 300
/// dedup_jaccard_threshold = 0.6
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EpisodicConsolidationConfig {
    /// Enable the episodic consolidation daemon. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for fact extraction LLM calls.
    /// Falls back to the primary provider when empty.
    pub consolidation_provider: ProviderName,
    /// How often the consolidation sweep runs, in seconds. Default: `1800` (30 min).
    #[serde(default = "default_episodic_consolidation_interval_secs")]
    pub interval_secs: u64,
    /// Maximum number of episodic events to process per sweep. Default: `30`.
    #[serde(default = "default_episodic_consolidation_batch_size")]
    pub batch_size: usize,
    /// Minimum age in seconds before an episodic event is eligible. Default: `300` (5 min).
    /// Prevents consolidating events from the active conversation.
    #[serde(default = "default_episodic_consolidation_min_age_secs")]
    pub min_age_secs: u64,
    /// Jaccard similarity threshold for deduplication against existing key facts.
    /// Facts with token-set Jaccard >= this value are considered duplicates. Default: `0.6`.
    #[serde(default = "default_episodic_consolidation_dedup_jaccard_threshold")]
    pub dedup_jaccard_threshold: f32,
}

impl Default for EpisodicConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            consolidation_provider: ProviderName::default(),
            interval_secs: default_episodic_consolidation_interval_secs(),
            batch_size: default_episodic_consolidation_batch_size(),
            min_age_secs: default_episodic_consolidation_min_age_secs(),
            dedup_jaccard_threshold: default_episodic_consolidation_dedup_jaccard_threshold(),
        }
    }
}
