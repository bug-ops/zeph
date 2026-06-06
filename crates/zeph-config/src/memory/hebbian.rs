// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hebbian / APEX synaptic-learning configuration.
//!
//! Edge-weight plasticity ([`HebbianConfig`]), reward-prediction-error gating
//! ([`RpeConfig`]), belief revision, write gating, and conflict-recency tuning.

use crate::providers::ProviderName;
use serde::{Deserialize, Serialize};
use zeph_common::memory::EdgeType;

use super::validate_similarity_threshold;

fn default_write_gate_min_edge_relevance() -> f32 {
    0.3
}

fn default_conflict_recency_slow_threshold() -> f32 {
    0.2
}

/// `MemORAI` write-gate prefilter configuration (#3709).
///
/// When `enabled = true`, low-signal edges (confidence below threshold + generic relation type)
/// are silently dropped before write, reducing noise in the knowledge graph.
///
/// TOML path: `[memory.graph.write_gate]`
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WriteGateConfig {
    /// Enable write-gate prefilter. Default: `false` (opt-in).
    pub enabled: bool,
    /// Minimum edge confidence to pass the gate when the relation is low-signal. Default: `0.3`.
    ///
    /// Range: `[0.0, 1.0]`.
    #[serde(
        default = "default_write_gate_min_edge_relevance",
        deserialize_with = "validate_similarity_threshold"
    )]
    pub min_edge_relevance: f32,
}

impl Default for WriteGateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_edge_relevance: default_write_gate_min_edge_relevance(),
        }
    }
}

/// Recency fallback threshold for the conflict resolver (#3709).
///
/// TOML path: `[memory.graph.conflict]`
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ConflictRecencyConfig {
    /// Minimum `confidence_slow` for the recency strategy to prefer an edge. Default: `0.2`.
    ///
    /// When two cardinality-1 heads conflict and recency is the resolution strategy,
    /// only edges with `confidence_slow >= recency_slow_threshold` are preferred by recency;
    /// edges below the threshold fall back to `valid_from` comparison. Range: `[0.0, 1.0]`.
    #[serde(
        default = "default_conflict_recency_slow_threshold",
        deserialize_with = "validate_similarity_threshold"
    )]
    pub recency_slow_threshold: f32,
}

impl Default for ConflictRecencyConfig {
    fn default() -> Self {
        Self {
            recency_slow_threshold: default_conflict_recency_slow_threshold(),
        }
    }
}

/// Kumiho belief revision configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BeliefRevisionConfig {
    /// Enable semantic contradiction detection for graph edges. Default: `false`.
    pub enabled: bool,
    /// Cosine similarity threshold for considering two facts as contradictory.
    /// Only edges with similarity >= this value are candidates for revision. Default: `0.85`.
    #[serde(deserialize_with = "validate_similarity_threshold")]
    pub similarity_threshold: f32,
}

fn default_belief_revision_similarity_threshold() -> f32 {
    0.85
}

impl Default for BeliefRevisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            similarity_threshold: default_belief_revision_similarity_threshold(),
        }
    }
}

/// D-MEM RPE-based tiered graph extraction routing configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RpeConfig {
    /// Enable RPE-based routing to skip extraction on low-surprise turns. Default: `false`.
    pub enabled: bool,
    /// RPE threshold. Turns with RPE < this value skip graph extraction. Range: `[0.0, 1.0]`.
    /// Default: `0.3`.
    #[serde(deserialize_with = "validate_similarity_threshold")]
    pub threshold: f32,
    /// Maximum consecutive turns to skip before forcing extraction (safety valve). Default: `5`.
    pub max_skip_turns: u32,
}

fn default_rpe_threshold() -> f32 {
    0.3
}

fn default_rpe_max_skip_turns() -> u32 {
    5
}

impl Default for RpeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_rpe_threshold(),
            max_skip_turns: default_rpe_max_skip_turns(),
        }
    }
}

/// Hebbian edge-weight reinforcement and consolidation configuration (HL-F1/F2/F3/F4, #3344/#3345).
///
/// Controls opt-in Hebbian learning on knowledge-graph edges. When enabled, every
/// recall traversal increments the `weight` column of the traversed edges, building
/// a usage-frequency signal into the graph. The consolidation sub-feature (HL-F3/F4)
/// runs a background sweep that identifies high-traffic entity clusters and distills
/// them into `graph_rules` entries via an LLM.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct HebbianConfig {
    /// Master switch. When `false`, no `weight` updates are written to the database
    /// and the consolidation loop does not start. Default: `false`.
    pub enabled: bool,
    /// Weight increment per co-activation (HL-F2, #3344).
    ///
    /// Typical range: `0.01`–`0.5`. A value of `0.0` is accepted but logs a `WARN` at
    /// startup when `enabled = true`. Default: `0.1`.
    pub hebbian_lr: f32,
    /// How often the consolidation sweep runs, in seconds (HL-F3, #3345).
    ///
    /// Set to `0` to disable the consolidation loop while keeping Hebbian updates active.
    /// Default: `3600` (one hour).
    pub consolidation_interval_secs: u64,
    /// Minimum `degree × avg_weight` score for an entity to qualify as a consolidation
    /// candidate (HL-F3, #3345). Default: `5.0`.
    pub consolidation_threshold: f64,
    /// Provider name (from `[[llm.providers]]`) used for cluster distillation (HL-F4, #3345).
    ///
    /// Falls back to the main provider when `None` or unresolvable.
    #[serde(default)]
    pub consolidate_provider: Option<ProviderName>,
    /// Maximum number of candidates processed per sweep (HL-F3, #3345). Default: `10`.
    pub max_candidates_per_sweep: usize,
    /// Minimum seconds between consecutive consolidations of the same entity (HL-F3, #3345).
    ///
    /// An entity is skipped if its `consolidated_at` timestamp is within this window.
    /// Default: `86400` (24 hours).
    pub consolidation_cooldown_secs: u64,
    /// LLM prompt timeout for a single distillation call, in seconds (HL-F4, #3345).
    /// Default: `30`.
    pub consolidation_prompt_timeout_secs: u64,
    /// Maximum number of neighbouring entity summaries passed to the LLM per candidate
    /// (HL-F4, #3345). Default: `20`.
    pub consolidation_max_neighbors: usize,
    /// Enable HL-F5 spreading activation from the top-1 ANN anchor (HL-F5, #3346).
    ///
    /// When `true` and `enabled = true`, `recall_graph_hela` performs BFS from the
    /// nearest entity anchor, scoring nodes by `path_weight × cosine`. Default: `false`.
    pub spreading_activation: bool,
    /// BFS depth for HL-F5 spreading activation. Clamped to `[1, 6]`. Default: `2`.
    pub spread_depth: u32,
    /// MAGMA edge-type filter for HL-F5 spreading activation.
    ///
    /// Accepted values: `"semantic"`, `"temporal"`, `"causal"`, `"entity"`.
    /// Empty = traverse all edge types. Default: `[]`.
    pub spread_edge_types: Vec<EdgeType>,
    /// Per-step circuit-breaker timeout for HL-F5 in milliseconds.
    ///
    /// Any internal step (anchor ANN, edges batch, vectors batch) that exceeds this
    /// duration triggers an `Ok(Vec::new())` fallback with a `WARN`. Default: `8`.
    pub step_budget_ms: u64,
    /// Timeout for the initial query embedding call in HL-F5, in seconds.
    ///
    /// `0` disables the timeout. Default: `5`.
    pub embed_timeout_secs: u64,
}

impl Default for HebbianConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hebbian_lr: 0.1,
            consolidation_interval_secs: 3600,
            consolidation_threshold: 5.0,
            consolidate_provider: None,
            max_candidates_per_sweep: 10,
            consolidation_cooldown_secs: 86_400,
            consolidation_prompt_timeout_secs: 30,
            consolidation_max_neighbors: 20,
            spreading_activation: false,
            spread_depth: 2,
            spread_edge_types: Vec::new(),
            step_budget_ms: 8,
            embed_timeout_secs: 5,
        }
    }
}
