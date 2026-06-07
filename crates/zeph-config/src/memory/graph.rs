// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Knowledge-graph memory configuration.
//!
//! Covers entity/edge extraction, spreading activation, community summarization,
//! implicit-conflict detection, APEX-MEM quality gating, and graph retrieval strategies.

use crate::providers::ProviderName;
use serde::{Deserialize, Serialize};

use super::{BeliefRevisionConfig, ConflictRecencyConfig, RpeConfig, WriteGateConfig};

fn default_graph_max_entities_per_message() -> usize {
    10
}

fn default_graph_max_edges_per_message() -> usize {
    15
}

fn default_graph_community_refresh_interval() -> usize {
    100
}

fn default_graph_community_summary_max_prompt_bytes() -> usize {
    8192
}

fn default_graph_community_summary_concurrency() -> usize {
    4
}

fn default_lpa_edge_chunk_size() -> usize {
    10_000
}

fn default_graph_entity_similarity_threshold() -> f32 {
    0.85
}

fn default_graph_entity_ambiguous_threshold() -> f32 {
    0.70
}

fn default_graph_extraction_timeout_secs() -> u64 {
    15
}

fn default_graph_max_hops() -> u32 {
    2
}

fn default_graph_recall_limit() -> usize {
    10
}

fn default_graph_expired_edge_retention_days() -> u32 {
    90
}

fn default_graph_temporal_decay_rate() -> f64 {
    0.0
}

fn default_graph_edge_history_limit() -> usize {
    100
}

fn default_spreading_activation_decay_lambda() -> f32 {
    0.85
}

fn default_spreading_activation_max_hops() -> u32 {
    3
}

fn default_spreading_activation_activation_threshold() -> f32 {
    0.1
}

fn default_spreading_activation_inhibition_threshold() -> f32 {
    0.8
}

fn default_spreading_activation_max_activated_nodes() -> usize {
    50
}

fn default_spreading_activation_recall_timeout_ms() -> u64 {
    1000
}

fn default_benna_alpha() -> f32 {
    0.3
}

fn default_benna_fast_rate() -> f32 {
    0.5
}

fn default_benna_slow_rate() -> f32 {
    0.05
}

fn default_note_linking_similarity_threshold() -> f32 {
    0.85
}

fn default_note_linking_top_k() -> usize {
    10
}

fn default_note_linking_timeout_secs() -> u64 {
    5
}

fn validate_temporal_decay_rate<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f64 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "temporal_decay_rate must be a finite number",
        ));
    }
    if !(0.0..=10.0).contains(&value) {
        return Err(serde::de::Error::custom(
            "temporal_decay_rate must be in [0.0, 10.0]",
        ));
    }
    Ok(value)
}

/// Configuration for SYNAPSE spreading activation retrieval over the entity graph.
///
/// When `enabled = true`, spreading activation replaces BFS-based graph recall.
/// Seeds are initialized from fuzzy entity matches, then activation propagates
/// hop-by-hop with exponential decay and lateral inhibition.
///
/// # Validation
///
/// Constraints enforced at deserialization time:
/// - `0.0 < decay_lambda <= 1.0`
/// - `max_hops >= 1`
/// - `activation_threshold < inhibition_threshold`
/// - `recall_timeout_ms >= 1` (clamped to 100 with a warning if set to 0)
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SpreadingActivationConfig {
    /// Enable spreading activation (replaces BFS in graph recall when `true`). Default: `false`.
    pub enabled: bool,
    /// Per-hop activation decay factor. Range: `(0.0, 1.0]`. Default: `0.85`.
    #[serde(deserialize_with = "crate::de_helpers::de_unit_open")]
    pub decay_lambda: f32,
    /// Maximum propagation depth. Must be `>= 1`. Default: `3`.
    #[serde(deserialize_with = "validate_max_hops")]
    pub max_hops: u32,
    /// Minimum activation score to include a node in results. Default: `0.1`.
    pub activation_threshold: f32,
    /// Activation level at which a node stops receiving more activation. Default: `0.8`.
    pub inhibition_threshold: f32,
    /// Cap on total activated nodes per spread pass. Default: `50`.
    pub max_activated_nodes: usize,
    /// Weight of structural score in hybrid seed ranking. Range: `[0.0, 1.0]`. Default: `0.4`.
    #[serde(default = "default_seed_structural_weight")]
    pub seed_structural_weight: f32,
    /// Maximum seeds per community. `0` = unlimited. Default: `3`.
    #[serde(default = "default_seed_community_cap")]
    pub seed_community_cap: usize,
    /// Timeout in milliseconds for a single spreading activation recall call. Default: `1000`.
    /// Values below 1 are clamped to 100ms at runtime. Benchmark data shows FTS5 + graph
    /// traversal completes within 200–400ms; 1000ms provides headroom for cold caches.
    #[serde(default = "default_spreading_activation_recall_timeout_ms")]
    pub recall_timeout_ms: u64,
    /// SYNAPSE blend coefficient for Benna-Fusi fast/slow variables (#3709).
    ///
    /// `blended = alpha * confidence_fast + (1 - alpha) * confidence_slow`.
    /// Range: `[0.0, 1.0]`. Default: `0.3`.
    #[serde(
        default = "default_benna_alpha",
        deserialize_with = "validate_benna_alpha"
    )]
    pub alpha: f32,
    /// Benna-Fusi fast-variable learning rate applied on each confidence merge (#3709).
    ///
    /// `fast' = fast + eta_f * (c - fast)`. Range: `(0.0, 1.0]`. Default: `0.5`.
    #[serde(
        default = "default_benna_fast_rate",
        deserialize_with = "validate_benna_rate"
    )]
    pub benna_fast_rate: f32,
    /// Benna-Fusi slow-variable learning rate applied on each confidence merge (#3709).
    ///
    /// `slow' = slow + eta_s * (fast' - slow)`. Range: `(0.0, 1.0]`. Default: `0.05`.
    #[serde(
        default = "default_benna_slow_rate",
        deserialize_with = "validate_benna_rate"
    )]
    pub benna_slow_rate: f32,
}

fn validate_max_hops<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <u32 as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("max_hops must be >= 1"));
    }
    Ok(value)
}

fn validate_benna_alpha<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if !value.is_finite() {
        return Err(serde::de::Error::custom("alpha must be a finite number"));
    }
    if !(0.0..=1.0).contains(&value) {
        return Err(serde::de::Error::custom("alpha must be in [0.0, 1.0]"));
    }
    Ok(value)
}

fn validate_benna_rate<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if !value.is_finite() {
        return Err(serde::de::Error::custom(
            "benna_fast_rate/benna_slow_rate must be a finite number",
        ));
    }
    if !(value > 0.0 && value <= 1.0) {
        return Err(serde::de::Error::custom(
            "benna_fast_rate/benna_slow_rate must be in (0.0, 1.0]",
        ));
    }
    Ok(value)
}

impl SpreadingActivationConfig {
    /// Validate cross-field constraints that cannot be expressed in per-field validators.
    ///
    /// # Errors
    ///
    /// Returns an error string if `activation_threshold >= inhibition_threshold`.
    #[must_use = "validation result must be checked"]
    pub fn validate(&self) -> Result<(), String> {
        if self.activation_threshold >= self.inhibition_threshold {
            return Err(format!(
                "activation_threshold ({}) must be < inhibition_threshold ({})",
                self.activation_threshold, self.inhibition_threshold
            ));
        }
        Ok(())
    }
}

fn default_seed_structural_weight() -> f32 {
    0.4
}

fn default_seed_community_cap() -> usize {
    3
}

impl Default for SpreadingActivationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            decay_lambda: default_spreading_activation_decay_lambda(),
            max_hops: default_spreading_activation_max_hops(),
            activation_threshold: default_spreading_activation_activation_threshold(),
            inhibition_threshold: default_spreading_activation_inhibition_threshold(),
            max_activated_nodes: default_spreading_activation_max_activated_nodes(),
            seed_structural_weight: default_seed_structural_weight(),
            seed_community_cap: default_seed_community_cap(),
            recall_timeout_ms: default_spreading_activation_recall_timeout_ms(),
            alpha: default_benna_alpha(),
            benna_fast_rate: default_benna_fast_rate(),
            benna_slow_rate: default_benna_slow_rate(),
        }
    }
}

/// Configuration for A-MEM dynamic note linking.
///
/// When enabled, after each graph extraction pass, entities extracted from the message are
/// compared against the entity embedding collection. Pairs with cosine similarity above
/// `similarity_threshold` receive a `similar_to` edge in the graph.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct NoteLinkingConfig {
    /// Enable A-MEM note linking after graph extraction. Default: `false`.
    pub enabled: bool,
    /// Minimum cosine similarity score to create a `similar_to` edge. Default: `0.85`.
    #[serde(deserialize_with = "crate::de_helpers::de_unit_closed")]
    pub similarity_threshold: f32,
    /// Maximum number of similar entities to link per extracted entity. Default: `10`.
    pub top_k: usize,
    /// Timeout for the entire linking pass in seconds. Default: `5`.
    pub timeout_secs: u64,
}

impl Default for NoteLinkingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            similarity_threshold: default_note_linking_similarity_threshold(),
            top_k: default_note_linking_top_k(),
            timeout_secs: default_note_linking_timeout_secs(),
        }
    }
}

// ── EM-Graph config (issue #3713) ──────────────────────────────────────────────

/// EM-Graph episodic event extraction and causal linking configuration.
///
/// When enabled, episodic events are extracted from conversation turns and linked
/// via causal relationships stored in `episodic_events` and `causal_links` tables.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.em_graph]
/// enabled = false
/// extract_provider = ""
/// max_chain_depth = 3
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EmGraphConfig {
    /// Enable EM-Graph event extraction and causal linking. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for event extraction.
    /// Falls back to the primary provider when empty.
    pub extract_provider: ProviderName,
    /// Maximum hops when traversing causal chains during recall. Default: `3`.
    pub max_chain_depth: u32,
}

impl Default for EmGraphConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            extract_provider: ProviderName::default(),
            max_chain_depth: 3,
        }
    }
}

/// Graph retrieval strategy for `[memory.graph]`.
///
/// Selects the algorithm used to traverse the knowledge graph during recall.
/// The default (`synapse`) preserves existing SYNAPSE spreading-activation behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GraphRetrievalStrategy {
    /// SYNAPSE spreading activation (default, existing behavior).
    #[default]
    Synapse,
    /// Hop-limited BFS traversal (pre-SYNAPSE behavior).
    Bfs,
    /// A* shortest-path traversal via petgraph.
    #[serde(rename = "astar")]
    AStar,
    /// Concentric BFS expanding outward from seed nodes.
    WaterCircles,
    /// Beam search: keep top-K candidates per hop.
    BeamSearch,
    /// Dynamic: LLM classifier selects strategy per query.
    Hybrid,
}

fn default_beam_width() -> usize {
    10
}

/// Beam search retrieval configuration for `[memory.graph.beam_search]`.
///
/// Controls the width of the beam during graph traversal: how many top candidates
/// are retained at each hop.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BeamSearchConfig {
    /// Number of top candidates kept per hop. Default: `10`.
    #[serde(default = "default_beam_width")]
    pub beam_width: usize,
}

impl Default for BeamSearchConfig {
    fn default() -> Self {
        Self {
            beam_width: default_beam_width(),
        }
    }
}

/// `WaterCircles` BFS configuration for `[memory.graph.watercircles]`.
///
/// Controls ring-by-ring concentric BFS traversal from seed nodes.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WaterCirclesConfig {
    /// Max facts per ring (hop). `0` = auto (`limit / max_hops`). Default: `0`.
    #[serde(default)]
    pub ring_limit: usize,
}

fn default_evolution_sweep_interval() -> usize {
    50
}

fn default_confidence_prune_threshold() -> f32 {
    0.1
}

/// Experience memory configuration for `[memory.graph.experience]`.
///
/// Controls recording of tool execution outcomes and graph evolution sweeps.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExperienceConfig {
    /// Enable experience memory recording. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Enable graph evolution sweep (prune self-loops + low-confidence edges). Default: `false`.
    #[serde(default)]
    pub evolution_sweep_enabled: bool,
    /// Confidence threshold below which zero-retrieval edges are pruned. Default: `0.1`.
    #[serde(default = "default_confidence_prune_threshold")]
    pub confidence_prune_threshold: f32,
    /// Number of turns between evolution sweeps. Default: `50`.
    #[serde(default = "default_evolution_sweep_interval")]
    pub evolution_sweep_interval: usize,
}

impl Default for ExperienceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            evolution_sweep_enabled: false,
            confidence_prune_threshold: default_confidence_prune_threshold(),
            evolution_sweep_interval: default_evolution_sweep_interval(),
        }
    }
}

/// Configuration for the knowledge graph memory subsystem (`[memory.graph]` TOML section).
///
/// # Security
///
/// Entity names, relation labels, and fact strings extracted by the LLM are stored verbatim
/// without PII redaction. This is a known pre-1.0 MVP limitation. Do not enable graph memory
/// when processing conversations that may contain personal, medical, or sensitive data until
/// a redaction pass is implemented on the write path.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct GraphConfig {
    pub enabled: bool,
    pub extract_model: String,
    #[serde(default = "default_graph_max_entities_per_message")]
    pub max_entities_per_message: usize,
    #[serde(default = "default_graph_max_edges_per_message")]
    pub max_edges_per_message: usize,
    #[serde(default = "default_graph_community_refresh_interval")]
    pub community_refresh_interval: usize,
    #[serde(default = "default_graph_entity_similarity_threshold")]
    pub entity_similarity_threshold: f32,
    #[serde(default = "default_graph_extraction_timeout_secs")]
    pub extraction_timeout_secs: u64,
    #[serde(default)]
    pub use_embedding_resolution: bool,
    #[serde(default = "default_graph_entity_ambiguous_threshold")]
    pub entity_ambiguous_threshold: f32,
    #[serde(default = "default_graph_max_hops")]
    pub max_hops: u32,
    #[serde(default = "default_graph_recall_limit")]
    pub recall_limit: usize,
    /// Days to retain expired (superseded) edges before deletion. Default: 90.
    #[serde(default = "default_graph_expired_edge_retention_days")]
    pub expired_edge_retention_days: u32,
    /// Maximum entities to retain in the graph. 0 = unlimited.
    #[serde(default)]
    pub max_entities: usize,
    /// Maximum prompt size in bytes for community summary generation. Default: 8192.
    #[serde(default = "default_graph_community_summary_max_prompt_bytes")]
    pub community_summary_max_prompt_bytes: usize,
    /// Maximum concurrent LLM calls during community summarization. Default: 4.
    #[serde(default = "default_graph_community_summary_concurrency")]
    pub community_summary_concurrency: usize,
    /// Number of edges fetched per chunk during community detection. Default: 10000.
    /// Set to 0 to disable chunking and load all edges at once (legacy behavior).
    #[serde(default = "default_lpa_edge_chunk_size")]
    pub lpa_edge_chunk_size: usize,
    /// Temporal recency decay rate for graph recall scoring (units: 1/day).
    ///
    /// When > 0, recent edges receive a small additive score boost over older edges.
    /// The boost formula is `1 / (1 + age_days * rate)`, blended additively with the base
    /// composite score. Default 0.0 preserves existing scoring behavior exactly.
    #[serde(
        default = "default_graph_temporal_decay_rate",
        deserialize_with = "validate_temporal_decay_rate"
    )]
    pub temporal_decay_rate: f64,
    /// Maximum number of historical edge versions returned by `edge_history()`. Default: 100.
    ///
    /// Caps the result set returned for a given source entity + predicate pair. Prevents
    /// unbounded memory usage for high-churn predicates when this method is exposed via TUI
    /// or API endpoints.
    #[serde(default = "default_graph_edge_history_limit")]
    pub edge_history_limit: usize,
    /// A-MEM dynamic note linking configuration.
    ///
    /// When `note_linking.enabled = true`, entities extracted from each message are linked to
    /// semantically similar entities via `similar_to` edges. Requires an embedding store
    /// (`qdrant` or `sqlite` vector backend) to be configured.
    #[serde(default)]
    pub note_linking: NoteLinkingConfig,
    /// SYNAPSE spreading activation retrieval configuration.
    ///
    /// When `spreading_activation.enabled = true`, graph recall uses spreading activation
    /// with lateral inhibition and temporal decay instead of BFS.
    #[serde(default)]
    pub spreading_activation: SpreadingActivationConfig,
    /// Graph retrieval strategy. Default: `synapse` (preserves existing behavior).
    ///
    /// When `spreading_activation.enabled = true` and `retrieval_strategy` is `synapse`,
    /// SYNAPSE spreading activation is used. Set to `bfs` to revert to hop-limited BFS.
    #[serde(default)]
    pub retrieval_strategy: GraphRetrievalStrategy,
    /// Named LLM provider from `[[llm.providers]]` for graph entity/relation extraction.
    ///
    /// When non-empty, graph extraction (and downstream note linking and community
    /// summarization) use this provider instead of the primary `SemanticMemory.provider`.
    /// This is the recommended fix for `quality_gate` false positives (#3601): JSON
    /// extraction tasks produce structurally low prompt/response similarity (~0.55–0.70),
    /// which causes systematic quality gate rejections. A named provider built via
    /// `resolve_background_provider` bypasses `apply_routing_signals()` and therefore
    /// has no quality gate attached.
    ///
    /// Falls back to the primary provider when empty. Default: `""` (use primary).
    #[serde(default)]
    pub extract_provider: ProviderName,
    /// Named LLM provider for hybrid strategy classification.
    /// Falls back to the default provider when `None`.
    #[serde(default)]
    pub strategy_classifier_provider: Option<ProviderName>,
    /// Beam search configuration.
    #[serde(default)]
    pub beam_search: BeamSearchConfig,
    /// `WaterCircles` BFS configuration.
    #[serde(default)]
    pub watercircles: WaterCirclesConfig,
    /// Experience memory configuration.
    #[serde(default)]
    pub experience: ExperienceConfig,
    /// A-MEM link weight decay: multiplicative factor applied to `retrieval_count`
    /// for un-retrieved edges each decay pass. Range: `(0.0, 1.0]`. Default: `0.95`.
    #[serde(
        default = "default_link_weight_decay_lambda",
        deserialize_with = "crate::de_helpers::de_unit_open"
    )]
    pub link_weight_decay_lambda: f64,
    /// Seconds between link weight decay passes. Default: `86400` (24 hours).
    #[serde(default = "default_link_weight_decay_interval_secs")]
    pub link_weight_decay_interval_secs: u64,
    /// Kumiho AGM-inspired belief revision configuration.
    ///
    /// When `belief_revision.enabled = true`, new edges that semantically contradict existing
    /// edges for the same entity pair trigger revision: the old edge is invalidated with a
    /// `superseded_by` pointer and the new edge becomes the current belief.
    #[serde(default)]
    pub belief_revision: BeliefRevisionConfig,
    /// D-MEM RPE-based tiered graph extraction routing.
    ///
    /// When `rpe.enabled = true`, low-surprise turns skip the expensive MAGMA LLM extraction
    /// pipeline. A consecutive-skip safety valve ensures no turn is silently skipped indefinitely.
    #[serde(default)]
    pub rpe: RpeConfig,
    /// `SQLite` connection pool size dedicated to graph operations.
    ///
    /// Graph tables share the same database file as messages/embeddings but use a
    /// separate pool to prevent pool starvation when community detection or spreading
    /// activation runs concurrently with regular memory operations. Default: `3`.
    #[serde(default = "default_graph_pool_size")]
    pub pool_size: u32,
    /// APEX-MEM append-only write path (#3631).
    ///
    /// When `apex_mem.enabled = true`, edge insertion uses `insert_or_supersede` with
    /// supersession chains instead of the legacy destructive-update path.
    #[serde(default)]
    pub apex_mem: ApexMemConfig,
    /// LLM call timeout per extraction request, in seconds. Default: `30`.
    #[serde(default = "default_graph_llm_timeout_secs")]
    pub llm_timeout_secs: u64,
    /// PRISM query-sensitive edge costing in A* graph recall.
    ///
    /// When `true`, edge cost in the A\* graph recall function is modulated by the cosine similarity
    /// between the query embedding and the target entity embedding:
    /// `cost = (1.0 - confidence) * (1.0 - target_cosine).max(0.01)`.
    /// Edges toward semantically relevant entities receive lower cost and are therefore
    /// preferred by A*, producing query-aligned recall paths.
    ///
    /// Requires an embedding store (`qdrant` or `sqlite` vector backend). When the embedding
    /// store is unavailable or a target entity has no stored embedding, falls back to the
    /// baseline cost `1.0 - confidence`.
    ///
    /// Default: `false` (preserves existing A* behaviour).
    #[serde(default)]
    pub query_sensitive_cost: bool,

    /// Implicit conflict detection for SYNAPSE recall (spec 004-17, STALE/CUPMem).
    ///
    /// When enabled, write-time fuzzy predicate matching detects implicit conflicts
    /// between graph edges and annotates SYNAPSE recall results accordingly.
    #[serde(default)]
    pub implicit_conflict: ImplicitConflictConfig,
    /// `MemORAI` write-gate prefilter (#3709).
    ///
    /// When `write_gate.enabled = true`, low-signal edges are dropped before graph write,
    /// reducing noise. Opt-in; default is `false`.
    #[serde(default)]
    pub write_gate: WriteGateConfig,
    /// Conflict resolver recency-fallback threshold (#3709).
    ///
    /// Controls when the recency strategy is allowed to override `valid_from` comparison.
    #[serde(default)]
    pub conflict_recency: ConflictRecencyConfig,
    /// Include knowledge-ingest edges in graph recall (spec-067 FR-003, INV-3).
    ///
    /// When `true` (default), all active edges are returned regardless of origin.
    /// When `false`, edges with `origin = 'ingest'` are excluded from recall so only
    /// conversation-derived knowledge appears in agent responses.
    ///
    /// Note: this flag lives here rather than `[knowledge]` for Phase 0 — it will be
    /// re-homed when the `[knowledge]` section is introduced in a later ingest issue.
    #[serde(default = "default_true")]
    pub recall_include_imported: bool,
}

/// Similarity method for implicit conflict detection.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SimilarityMethod {
    /// Normalized Levenshtein edit distance.
    #[default]
    Levenshtein,
    /// Cosine similarity over pre-computed predicate embeddings.
    Embedding,
    /// Either method triggers detection.
    Both,
}

/// Resolution strategy when an implicit conflict is detected.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConflictResolutionStrategy {
    /// Mark the pair as a candidate but do not supersede either edge.
    #[default]
    FlagOnly,
    /// Supersede the older edge via APEX-MEM `insert_or_supersede`.
    Recency,
    /// Supersede the lower-confidence edge.
    Confidence,
    /// Delegate resolution to an LLM provider; fall back to `flag_only` on timeout.
    Llm,
}

/// Configuration for the optional background consolidation daemon (spec 004-17).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ConsolidationDaemonConfig {
    /// Enable the background consolidation daemon.
    pub enabled: bool,
    /// How often the daemon runs, in seconds. Default: 7200 (2 hours).
    #[serde(default = "default_ic_daemon_interval_secs")]
    pub interval_seconds: u64,
    /// Maximum number of candidates processed per daemon run. Default: 100.
    #[serde(default = "default_ic_daemon_batch_size")]
    pub batch_size: usize,
}

impl Default for ConsolidationDaemonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: default_ic_daemon_interval_secs(),
            batch_size: default_ic_daemon_batch_size(),
        }
    }
}

fn default_ic_daemon_interval_secs() -> u64 {
    7200
}

fn default_ic_daemon_batch_size() -> usize {
    100
}

/// Configuration for implicit conflict detection (spec 004-17, STALE/CUPMem).
///
/// Controls write-time fuzzy predicate matching and SYNAPSE recall annotation.
/// All detection is gated behind `enabled = false` by default — no overhead when disabled.
///
/// TOML path: `[memory.graph.implicit_conflict]`
///
/// # Examples
///
/// ```toml
/// [memory.graph.implicit_conflict]
/// enabled = true
/// similarity_method = "levenshtein"
/// conflict_similarity_threshold = 0.80
/// resolution_strategy = "flag_only"
/// candidate_ttl_days = 30
/// propagation_depth = 2
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ImplicitConflictConfig {
    /// Enable implicit conflict detection. Default: `false`.
    pub enabled: bool,
    /// Similarity method used to detect candidate pairs.
    #[serde(default)]
    pub similarity_method: SimilarityMethod,
    /// Minimum similarity score to flag a pair as a conflict candidate. Default: 0.80.
    #[serde(default = "default_ic_similarity_threshold")]
    pub conflict_similarity_threshold: f64,
    /// How to resolve detected conflicts. Default: `flag_only`.
    #[serde(default)]
    pub resolution_strategy: ConflictResolutionStrategy,
    /// Provider name (from `[[llm.providers]]`) for LLM-mediated resolution.
    #[serde(default)]
    pub implicit_conflict_provider: crate::providers::ProviderName,
    /// LLM resolution timeout in milliseconds. Default: 800.
    #[serde(default = "default_ic_llm_timeout_ms")]
    pub conflict_llm_timeout_ms: u64,
    /// Days before an unresolved candidate entry expires. Default: 30.
    #[serde(default = "default_ic_candidate_ttl_days")]
    pub candidate_ttl_days: u32,
    /// SYNAPSE propagation depth for surfacing superseding facts. Default: 2.
    #[serde(default = "default_ic_propagation_depth")]
    pub propagation_depth: u32,
    /// Background consolidation daemon configuration.
    #[serde(default)]
    pub consolidation_daemon: ConsolidationDaemonConfig,
}

impl Default for ImplicitConflictConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            similarity_method: SimilarityMethod::default(),
            conflict_similarity_threshold: default_ic_similarity_threshold(),
            resolution_strategy: ConflictResolutionStrategy::default(),
            implicit_conflict_provider: crate::providers::ProviderName::default(),
            conflict_llm_timeout_ms: default_ic_llm_timeout_ms(),
            candidate_ttl_days: default_ic_candidate_ttl_days(),
            propagation_depth: default_ic_propagation_depth(),
            consolidation_daemon: ConsolidationDaemonConfig::default(),
        }
    }
}

fn default_ic_similarity_threshold() -> f64 {
    0.80
}

fn default_ic_llm_timeout_ms() -> u64 {
    800
}

fn default_ic_candidate_ttl_days() -> u32 {
    30
}

fn default_ic_propagation_depth() -> u32 {
    2
}

fn default_graph_pool_size() -> u32 {
    3
}

fn default_graph_llm_timeout_secs() -> u64 {
    30
}

/// APEX-MEM append-only write path configuration (`[memory.graph.apex_mem]`).
///
/// When `enabled = true`, graph edge insertion uses `insert_or_supersede`
/// instead of the legacy destructive-update `resolve_edge_typed`. This preserves
/// the full supersession chain and enables conflict resolution.
///
/// Spec: `/specs/004-memory/004-7-memory-apex-magma.md`
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ApexMemConfig {
    /// Enable the APEX-MEM append-only write path. Default: `false`.
    pub enabled: bool,
}

fn default_quality_gate_threshold() -> f32 {
    0.55
}

fn default_quality_gate_recent_window() -> usize {
    32
}

fn default_quality_gate_contradiction_grace_seconds() -> u64 {
    300
}

fn default_quality_gate_information_value_weight() -> f32 {
    0.4
}

fn default_quality_gate_reference_completeness_weight() -> f32 {
    0.3
}

fn default_quality_gate_contradiction_weight() -> f32 {
    0.3
}

fn default_quality_gate_rejection_rate_alarm_ratio() -> f32 {
    0.35
}

fn default_quality_gate_llm_timeout_ms() -> u64 {
    500
}

fn default_quality_gate_llm_weight() -> f32 {
    0.5
}

fn default_quality_gate_reference_check_lang_en() -> bool {
    true
}

/// Write quality gate configuration (`[memory.quality_gate]`).
///
/// When `enabled = true`, each `remember()` call is scored before persistence. Writes
/// below `threshold` are rejected. Rule-based scoring is the default; LLM-assisted
/// scoring is opt-in via `quality_gate_provider`.
///
/// Spec: `/specs/004-memory/004-9-memory-write-gate.md`
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WriteQualityGateConfig {
    /// Enable the write quality gate. Default: `false`.
    pub enabled: bool,
    /// Combined score threshold below which writes are rejected. Default: `0.55`.
    #[serde(default = "default_quality_gate_threshold")]
    pub threshold: f32,
    /// Number of recent writes compared for information-value scoring. Default: `32`.
    #[serde(default = "default_quality_gate_recent_window")]
    pub recent_window: usize,
    /// Edges older than this (seconds) are stable for contradiction detection. Default: `300`.
    #[serde(default = "default_quality_gate_contradiction_grace_seconds")]
    pub contradiction_grace_seconds: u64,
    /// Weight of `information_value` sub-score. Default: `0.4`.
    #[serde(default = "default_quality_gate_information_value_weight")]
    pub information_value_weight: f32,
    /// Weight of `reference_completeness` sub-score. Default: `0.3`.
    #[serde(default = "default_quality_gate_reference_completeness_weight")]
    pub reference_completeness_weight: f32,
    /// Weight of `contradiction` sub-score. Default: `0.3`.
    #[serde(default = "default_quality_gate_contradiction_weight")]
    pub contradiction_weight: f32,
    /// Rolling rejection-rate alarm ratio. Default: `0.35`.
    #[serde(default = "default_quality_gate_rejection_rate_alarm_ratio")]
    pub rejection_rate_alarm_ratio: f32,
    /// Named LLM provider for optional scoring path. Default: `""` (rule-based only).
    #[serde(default)]
    pub quality_gate_provider: ProviderName,
    /// LLM timeout in milliseconds. Default: `500`.
    #[serde(default = "default_quality_gate_llm_timeout_ms")]
    pub llm_timeout_ms: u64,
    /// LLM blend weight into final score. Default: `0.5`.
    #[serde(default = "default_quality_gate_llm_weight")]
    pub llm_weight: f32,
    /// Enable pronoun/deictic reference checks (English only). Default: `true`.
    #[serde(default = "default_quality_gate_reference_check_lang_en")]
    pub reference_check_lang_en: bool,
}

impl Default for WriteQualityGateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_quality_gate_threshold(),
            recent_window: default_quality_gate_recent_window(),
            contradiction_grace_seconds: default_quality_gate_contradiction_grace_seconds(),
            information_value_weight: default_quality_gate_information_value_weight(),
            reference_completeness_weight: default_quality_gate_reference_completeness_weight(),
            contradiction_weight: default_quality_gate_contradiction_weight(),
            rejection_rate_alarm_ratio: default_quality_gate_rejection_rate_alarm_ratio(),
            quality_gate_provider: ProviderName::default(),
            llm_timeout_ms: default_quality_gate_llm_timeout_ms(),
            llm_weight: default_quality_gate_llm_weight(),
            reference_check_lang_en: default_quality_gate_reference_check_lang_en(),
        }
    }
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            extract_model: String::new(),
            max_entities_per_message: default_graph_max_entities_per_message(),
            max_edges_per_message: default_graph_max_edges_per_message(),
            community_refresh_interval: default_graph_community_refresh_interval(),
            entity_similarity_threshold: default_graph_entity_similarity_threshold(),
            extraction_timeout_secs: default_graph_extraction_timeout_secs(),
            use_embedding_resolution: false,
            entity_ambiguous_threshold: default_graph_entity_ambiguous_threshold(),
            max_hops: default_graph_max_hops(),
            recall_limit: default_graph_recall_limit(),
            expired_edge_retention_days: default_graph_expired_edge_retention_days(),
            max_entities: 0,
            community_summary_max_prompt_bytes: default_graph_community_summary_max_prompt_bytes(),
            community_summary_concurrency: default_graph_community_summary_concurrency(),
            lpa_edge_chunk_size: default_lpa_edge_chunk_size(),
            temporal_decay_rate: default_graph_temporal_decay_rate(),
            edge_history_limit: default_graph_edge_history_limit(),
            note_linking: NoteLinkingConfig::default(),
            spreading_activation: SpreadingActivationConfig::default(),
            retrieval_strategy: GraphRetrievalStrategy::default(),
            extract_provider: ProviderName::default(),
            strategy_classifier_provider: None,
            beam_search: BeamSearchConfig::default(),
            watercircles: WaterCirclesConfig::default(),
            experience: ExperienceConfig::default(),
            link_weight_decay_lambda: default_link_weight_decay_lambda(),
            link_weight_decay_interval_secs: default_link_weight_decay_interval_secs(),
            belief_revision: BeliefRevisionConfig::default(),
            rpe: RpeConfig::default(),
            pool_size: default_graph_pool_size(),
            apex_mem: ApexMemConfig::default(),
            llm_timeout_secs: default_graph_llm_timeout_secs(),
            query_sensitive_cost: false,
            implicit_conflict: ImplicitConflictConfig::default(),
            write_gate: WriteGateConfig::default(),
            conflict_recency: ConflictRecencyConfig::default(),
            recall_include_imported: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_link_weight_decay_lambda() -> f64 {
    0.95
}

fn default_link_weight_decay_interval_secs() -> u64 {
    86400
}
