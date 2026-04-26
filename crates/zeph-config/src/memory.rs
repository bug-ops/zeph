// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::defaults::{default_sqlite_path_field, default_true};
use crate::providers::ProviderName;

fn default_sqlite_pool_size() -> u32 {
    5
}

fn default_max_history() -> usize {
    100
}

fn default_title_max_chars() -> usize {
    60
}

fn default_document_collection() -> String {
    "zeph_documents".into()
}

fn default_document_chunk_size() -> usize {
    1000
}

fn default_document_chunk_overlap() -> usize {
    100
}

fn default_document_top_k() -> usize {
    3
}

fn default_autosave_min_length() -> usize {
    20
}

fn default_tool_call_cutoff() -> usize {
    6
}

fn default_token_safety_margin() -> f32 {
    1.0
}

fn default_redact_credentials() -> bool {
    true
}

fn default_qdrant_url() -> String {
    "http://localhost:6334".into()
}

fn default_summarization_threshold() -> usize {
    50
}

fn default_context_budget_tokens() -> usize {
    0
}

fn default_soft_compaction_threshold() -> f32 {
    0.60
}

fn default_hard_compaction_threshold() -> f32 {
    0.90
}

fn default_compaction_preserve_tail() -> usize {
    6
}

fn default_compaction_cooldown_turns() -> u8 {
    2
}

fn default_auto_budget() -> bool {
    true
}

fn default_prune_protect_tokens() -> usize {
    40_000
}

fn default_cross_session_score_threshold() -> f32 {
    0.35
}

fn default_temporal_decay_half_life_days() -> u32 {
    30
}

fn default_mmr_lambda() -> f32 {
    0.7
}

fn default_semantic_enabled() -> bool {
    true
}

fn default_recall_limit() -> usize {
    5
}

fn default_vector_weight() -> f64 {
    0.7
}

fn default_keyword_weight() -> f64 {
    0.3
}

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

fn default_note_linking_similarity_threshold() -> f32 {
    0.85
}

fn default_note_linking_top_k() -> usize {
    10
}

fn default_note_linking_timeout_secs() -> u64 {
    5
}

fn default_shutdown_summary() -> bool {
    true
}

fn default_shutdown_summary_min_messages() -> usize {
    4
}

fn default_shutdown_summary_max_messages() -> usize {
    20
}

fn default_shutdown_summary_timeout_secs() -> u64 {
    30
}

fn validate_tier_similarity_threshold<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "similarity_threshold must be a finite number",
        ));
    }
    if !(0.5..=1.0).contains(&value) {
        return Err(serde::de::Error::custom(
            "similarity_threshold must be in [0.5, 1.0]",
        ));
    }
    Ok(value)
}

fn validate_tier_promotion_min_sessions<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <u32 as serde::Deserialize>::deserialize(deserializer)?;
    if value < 2 {
        return Err(serde::de::Error::custom(
            "promotion_min_sessions must be >= 2",
        ));
    }
    Ok(value)
}

fn validate_tier_sweep_batch_size<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <usize as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("sweep_batch_size must be >= 1"));
    }
    Ok(value)
}

fn default_tier_promotion_min_sessions() -> u32 {
    3
}

fn default_tier_similarity_threshold() -> f32 {
    0.92
}

fn default_tier_sweep_interval_secs() -> u64 {
    3600
}

fn default_tier_sweep_batch_size() -> usize {
    100
}

fn default_scene_similarity_threshold() -> f32 {
    0.80
}

fn default_scene_batch_size() -> usize {
    50
}

fn validate_scene_similarity_threshold<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "scene_similarity_threshold must be a finite number",
        ));
    }
    if !(0.5..=1.0).contains(&value) {
        return Err(serde::de::Error::custom(
            "scene_similarity_threshold must be in [0.5, 1.0]",
        ));
    }
    Ok(value)
}

fn validate_scene_batch_size<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <usize as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("scene_batch_size must be >= 1"));
    }
    Ok(value)
}

/// Configuration for the AOI three-layer memory tier promotion system (`[memory.tiers]`).
///
/// When `enabled = true`, a background sweep promotes frequently-accessed episodic messages
/// to semantic tier by clustering near-duplicates and distilling them via an LLM call.
///
/// # Validation
///
/// Constraints enforced at deserialization time:
/// - `similarity_threshold` in `[0.5, 1.0]`
/// - `promotion_min_sessions >= 2`
/// - `sweep_batch_size >= 1`
/// - `scene_similarity_threshold` in `[0.5, 1.0]`
/// - `scene_batch_size >= 1`
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct TierConfig {
    /// Enable the tier promotion system. When `false`, all messages remain episodic.
    /// Default: `false`.
    pub enabled: bool,
    /// Minimum number of distinct sessions a fact must appear in before promotion.
    /// Must be `>= 2`. Default: `3`.
    #[serde(deserialize_with = "validate_tier_promotion_min_sessions")]
    pub promotion_min_sessions: u32,
    /// Cosine similarity threshold for clustering near-duplicate facts during sweep.
    /// Must be in `[0.5, 1.0]`. Default: `0.92`.
    #[serde(deserialize_with = "validate_tier_similarity_threshold")]
    pub similarity_threshold: f32,
    /// How often the background promotion sweep runs, in seconds. Default: `3600`.
    pub sweep_interval_secs: u64,
    /// Maximum number of messages to evaluate per sweep cycle. Must be `>= 1`. Default: `100`.
    #[serde(deserialize_with = "validate_tier_sweep_batch_size")]
    pub sweep_batch_size: usize,
    /// Enable `MemScene` consolidation of semantic-tier messages. Default: `false`.
    pub scene_enabled: bool,
    /// Cosine similarity threshold for `MemScene` clustering. Must be in `[0.5, 1.0]`. Default: `0.80`.
    #[serde(deserialize_with = "validate_scene_similarity_threshold")]
    pub scene_similarity_threshold: f32,
    /// Maximum unassigned semantic messages processed per scene consolidation sweep. Default: `50`.
    #[serde(deserialize_with = "validate_scene_batch_size")]
    pub scene_batch_size: usize,
    /// Provider name from `[[llm.providers]]` for scene label/profile generation.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub scene_provider: ProviderName,
    /// How often the background scene consolidation sweep runs, in seconds. Default: `7200`.
    pub scene_sweep_interval_secs: u64,
}

fn default_scene_sweep_interval_secs() -> u64 {
    7200
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            promotion_min_sessions: default_tier_promotion_min_sessions(),
            similarity_threshold: default_tier_similarity_threshold(),
            sweep_interval_secs: default_tier_sweep_interval_secs(),
            sweep_batch_size: default_tier_sweep_batch_size(),
            scene_enabled: false,
            scene_similarity_threshold: default_scene_similarity_threshold(),
            scene_batch_size: default_scene_batch_size(),
            scene_provider: ProviderName::default(),
            scene_sweep_interval_secs: default_scene_sweep_interval_secs(),
        }
    }
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

fn validate_similarity_threshold<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "similarity_threshold must be a finite number",
        ));
    }
    if !(0.0..=1.0).contains(&value) {
        return Err(serde::de::Error::custom(
            "similarity_threshold must be in [0.0, 1.0]",
        ));
    }
    Ok(value)
}

fn validate_importance_weight<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f64 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "importance_weight must be a finite number",
        ));
    }
    if value < 0.0 {
        return Err(serde::de::Error::custom(
            "importance_weight must be non-negative",
        ));
    }
    if value > 1.0 {
        return Err(serde::de::Error::custom("importance_weight must be <= 1.0"));
    }
    Ok(value)
}

fn default_importance_weight() -> f64 {
    0.15
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
    #[serde(deserialize_with = "validate_decay_lambda")]
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
}

fn validate_decay_lambda<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "decay_lambda must be a finite number",
        ));
    }
    if !(value > 0.0 && value <= 1.0) {
        return Err(serde::de::Error::custom(
            "decay_lambda must be in (0.0, 1.0]",
        ));
    }
    Ok(value)
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

impl SpreadingActivationConfig {
    /// Validate cross-field constraints that cannot be expressed in per-field validators.
    ///
    /// # Errors
    ///
    /// Returns an error string if `activation_threshold >= inhibition_threshold`.
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
    #[serde(deserialize_with = "validate_similarity_threshold")]
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

/// Vector backend selector for embedding storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VectorBackend {
    Qdrant,
    #[default]
    Sqlite,
}

impl VectorBackend {
    /// Return the lowercase identifier string for this backend.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::VectorBackend;
    ///
    /// assert_eq!(VectorBackend::Sqlite.as_str(), "sqlite");
    /// assert_eq!(VectorBackend::Qdrant.as_str(), "qdrant");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Qdrant => "qdrant",
            Self::Sqlite => "sqlite",
        }
    }
}

/// Memory subsystem configuration, nested under `[memory]` in TOML.
///
/// Controls `SQLite` and Qdrant storage, semantic recall, context compaction,
/// multi-tier promotion, and all memory-related background tasks.
///
/// # Example (TOML)
///
/// ```toml
/// [memory]
/// sqlite_path = "~/.local/share/zeph/data/zeph.db"
/// qdrant_url = "http://localhost:6334"
/// history_limit = 50
/// summarization_threshold = 50
/// auto_budget = true
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct MemoryConfig {
    #[serde(default)]
    pub compression_guidelines: zeph_memory::CompressionGuidelinesConfig,
    #[serde(default = "default_sqlite_path_field")]
    pub sqlite_path: String,
    pub history_limit: u32,
    #[serde(default = "default_qdrant_url")]
    pub qdrant_url: String,
    #[serde(default)]
    pub semantic: SemanticConfig,
    #[serde(default = "default_summarization_threshold")]
    pub summarization_threshold: usize,
    #[serde(default = "default_context_budget_tokens")]
    pub context_budget_tokens: usize,
    #[serde(default = "default_soft_compaction_threshold")]
    pub soft_compaction_threshold: f32,
    #[serde(
        default = "default_hard_compaction_threshold",
        alias = "compaction_threshold"
    )]
    pub hard_compaction_threshold: f32,
    #[serde(default = "default_compaction_preserve_tail")]
    pub compaction_preserve_tail: usize,
    #[serde(default = "default_compaction_cooldown_turns")]
    pub compaction_cooldown_turns: u8,
    #[serde(default = "default_auto_budget")]
    pub auto_budget: bool,
    #[serde(default = "default_prune_protect_tokens")]
    pub prune_protect_tokens: usize,
    #[serde(default = "default_cross_session_score_threshold")]
    pub cross_session_score_threshold: f32,
    #[serde(default)]
    pub vector_backend: VectorBackend,
    #[serde(default = "default_token_safety_margin")]
    pub token_safety_margin: f32,
    #[serde(default = "default_redact_credentials")]
    pub redact_credentials: bool,
    #[serde(default = "default_true")]
    pub autosave_assistant: bool,
    #[serde(default = "default_autosave_min_length")]
    pub autosave_min_length: usize,
    #[serde(default = "default_tool_call_cutoff")]
    pub tool_call_cutoff: usize,
    #[serde(default = "default_sqlite_pool_size")]
    pub sqlite_pool_size: u32,
    #[serde(default)]
    pub sessions: SessionsConfig,
    #[serde(default)]
    pub documents: DocumentConfig,
    #[serde(default)]
    pub eviction: zeph_memory::EvictionConfig,
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub sidequest: SidequestConfig,
    #[serde(default)]
    pub graph: GraphConfig,
    /// Store a lightweight session summary to the vector store on shutdown when no session
    /// summary exists yet for this conversation. Enables cross-session recall for short or
    /// interrupted sessions that never triggered hard compaction. Default: `true`.
    #[serde(default = "default_shutdown_summary")]
    pub shutdown_summary: bool,
    /// Minimum number of user-turn messages required before a shutdown summary is generated.
    /// Sessions below this threshold are considered trivial and skipped. Default: `4`.
    #[serde(default = "default_shutdown_summary_min_messages")]
    pub shutdown_summary_min_messages: usize,
    /// Maximum number of recent messages (user + assistant) sent to the LLM for shutdown
    /// summarization. Caps token cost for long sessions that never triggered hard compaction.
    /// Default: `20`.
    #[serde(default = "default_shutdown_summary_max_messages")]
    pub shutdown_summary_max_messages: usize,
    /// Per-attempt timeout in seconds for each LLM call during shutdown summarization.
    /// Applies independently to the structured call and to the plain-text fallback.
    /// Default: `10`.
    #[serde(default = "default_shutdown_summary_timeout_secs")]
    pub shutdown_summary_timeout_secs: u64,
    /// Use structured anchored summaries for context compaction.
    ///
    /// When enabled, hard compaction requests a JSON schema from the LLM
    /// instead of free-form prose. Falls back to prose if the LLM fails
    /// to produce valid JSON. Default: `false`.
    #[serde(default)]
    pub structured_summaries: bool,
    /// AOI three-layer memory tier promotion system.
    ///
    /// When `tiers.enabled = true`, a background sweep promotes frequently-accessed episodic
    /// messages to a semantic tier by clustering near-duplicates and distilling via LLM.
    #[serde(default)]
    pub tiers: TierConfig,
    /// A-MAC adaptive memory admission control.
    ///
    /// When `admission.enabled = true`, each message is evaluated before saving and rejected
    /// if its composite admission score falls below the configured threshold.
    #[serde(default)]
    pub admission: AdmissionConfig,
    /// Session digest generation at session end. Default: disabled.
    #[serde(default)]
    pub digest: DigestConfig,
    /// Context assembly strategy. Default: `full_history` (current behavior).
    #[serde(default)]
    pub context_strategy: ContextStrategy,
    /// Number of turns at which `Adaptive` strategy switches to `MemoryFirst`. Default: `20`.
    #[serde(default = "default_crossover_turn_threshold")]
    pub crossover_turn_threshold: u32,
    /// All-Mem lifelong memory consolidation sweep.
    ///
    /// When `consolidation.enabled = true`, a background loop clusters semantically similar
    /// messages and merges them into consolidated entries via LLM.
    #[serde(default)]
    pub consolidation: ConsolidationConfig,
    /// `SleepGate` forgetting sweep (#2397).
    ///
    /// When `forgetting.enabled = true`, a background loop periodically decays importance
    /// scores and prunes memories below the forgetting floor.
    #[serde(default)]
    pub forgetting: ForgettingConfig,
    /// `PostgreSQL` connection URL.
    ///
    /// Used when the binary is compiled with `--features postgres`.
    /// Can be overridden by the vault key `ZEPH_DATABASE_URL`.
    /// Example: `postgres://user:pass@localhost:5432/zeph`
    /// Default: `None` (uses `sqlite_path` instead).
    #[serde(default)]
    pub database_url: Option<String>,
    /// Cost-sensitive store routing (#2444).
    ///
    /// When `store_routing.enabled = true`, query intent is classified and routed to
    /// the cheapest sufficient backend instead of querying all stores on every turn.
    #[serde(default)]
    pub store_routing: StoreRoutingConfig,
    /// Persona memory layer (#2461).
    ///
    /// When `persona.enabled = true`, user preferences and domain knowledge are extracted
    /// from conversation history and injected into context after the system prompt.
    #[serde(default)]
    pub persona: PersonaConfig,
    /// Trajectory-informed memory (#2498).
    #[serde(default)]
    pub trajectory: TrajectoryConfig,
    /// Category-aware memory (#2428).
    #[serde(default)]
    pub category: CategoryConfig,
    /// `TiMem` temporal-hierarchical memory tree (#2262).
    #[serde(default)]
    pub tree: TreeConfig,
    /// Time-based microcompact (#2699).
    ///
    /// When `microcompact.enabled = true`, stale low-value tool outputs are cleared
    /// from context when the session has been idle longer than `gap_threshold_minutes`.
    #[serde(default)]
    pub microcompact: MicrocompactConfig,
    /// autoDream background memory consolidation (#2697).
    ///
    /// When `autodream.enabled = true`, a constrained consolidation subagent runs
    /// after a session ends if both `min_sessions` and `min_hours` gates pass.
    #[serde(default)]
    pub autodream: AutoDreamConfig,
    /// Cosine similarity threshold for deduplicating key facts in `zeph_key_facts` (#2717).
    ///
    /// Before inserting a new key fact, its nearest neighbour is looked up in the
    /// `zeph_key_facts` collection.  If the best score is ≥ this threshold the fact is
    /// considered a near-duplicate and skipped.  Set to a value greater than `1.0` (e.g.
    /// `2.0`) to disable dedup entirely.  Default: `0.95`.
    #[serde(default = "default_key_facts_dedup_threshold")]
    pub key_facts_dedup_threshold: f32,
    /// Experience compression spectrum (#3305).
    ///
    /// Controls three-tier retrieval policy and background skill-promotion engine.
    #[serde(default)]
    pub compression_spectrum: crate::features::CompressionSpectrumConfig,
    /// MemMachine-inspired retrieval-stage tuning (#3340).
    ///
    /// Controls ANN candidate depth, search-prompt formatting, and the shape of memory snippets
    /// injected into agent context. Separate from `SemanticConfig` because these knobs apply
    /// uniformly across graph, hybrid, and vector-only recall paths.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [memory.retrieval]
    /// depth = 40
    /// search_prompt_template = ""
    /// context_format = "structured"
    /// ```
    #[serde(default)]
    pub retrieval: RetrievalConfig,
    /// `ReasoningBank`: distilled reasoning strategy memory (#3342).
    ///
    /// When `reasoning.enabled = true`, each completed agent turn is evaluated by a self-judge
    /// LLM call; successful and failed reasoning chains are compressed into short, generalizable
    /// strategy summaries stored in `reasoning_strategies` (`SQLite`) and a matching Qdrant
    /// collection. Top-k strategies are retrieved by embedding similarity at context-build time
    /// and injected before the LLM call.
    #[serde(default)]
    pub reasoning: ReasoningConfig,
    /// Hebbian edge-weight reinforcement configuration (HL-F1/F2, #3344).
    ///
    /// When `enabled = true`, the weight of each `graph_edges` row is incremented
    /// by `hebbian_lr` every time that edge is traversed during a recall. Default: disabled.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [memory.hebbian]
    /// enabled = true
    /// hebbian_lr = 0.1
    /// ```
    #[serde(default)]
    pub hebbian: HebbianConfig,
}

fn default_crossover_turn_threshold() -> u32 {
    20
}

fn default_key_facts_dedup_threshold() -> f32 {
    0.95
}

/// Session digest configuration (#2289).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DigestConfig {
    /// Enable session digest generation at session end. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for digest generation.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub provider: String,
    /// Maximum tokens for the digest text. Default: `500`.
    pub max_tokens: usize,
    /// Maximum messages to feed into the digest prompt. Default: `50`.
    pub max_input_messages: usize,
}

impl Default for DigestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: String::new(),
            max_tokens: 500,
            max_input_messages: 50,
        }
    }
}

/// Context assembly strategy (#2288).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextStrategy {
    /// Full conversation history trimmed to budget, with memory augmentation.
    /// This is the default and existing behavior.
    #[default]
    FullHistory,
    /// Drop conversation history; assemble context from summaries, semantic recall,
    /// cross-session memory, and session digest only.
    MemoryFirst,
    /// Start as `FullHistory`; switch to `MemoryFirst` when turn count exceeds
    /// `crossover_turn_threshold`.
    Adaptive,
}

/// Session list and auto-title configuration, nested under `[memory.sessions]` in TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionsConfig {
    /// Maximum number of sessions returned by list operations (0 = unlimited).
    #[serde(default = "default_max_history")]
    pub max_history: usize,
    /// Maximum characters for auto-generated session titles.
    #[serde(default = "default_title_max_chars")]
    pub title_max_chars: usize,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            max_history: default_max_history(),
            title_max_chars: default_title_max_chars(),
        }
    }
}

/// Configuration for the document ingestion and RAG retrieval pipeline.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DocumentConfig {
    #[serde(default = "default_document_collection")]
    pub collection: String,
    #[serde(default = "default_document_chunk_size")]
    pub chunk_size: usize,
    #[serde(default = "default_document_chunk_overlap")]
    pub chunk_overlap: usize,
    /// Number of document chunks to inject into agent context per turn.
    #[serde(default = "default_document_top_k")]
    pub top_k: usize,
    /// Enable document RAG injection into agent context.
    #[serde(default)]
    pub rag_enabled: bool,
}

impl Default for DocumentConfig {
    fn default() -> Self {
        Self {
            collection: default_document_collection(),
            chunk_size: default_document_chunk_size(),
            chunk_overlap: default_document_chunk_overlap(),
            top_k: default_document_top_k(),
            rag_enabled: false,
        }
    }
}

/// Semantic (vector) memory retrieval configuration, nested under `[memory.semantic]` in TOML.
///
/// Controls how memories are searched and ranked, including temporal decay, MMR diversity
/// re-ranking, and hybrid BM25+vector weighting.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.semantic]
/// enabled = true
/// recall_limit = 5
/// vector_weight = 0.7
/// keyword_weight = 0.3
/// mmr_lambda = 0.7
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct SemanticConfig {
    /// Enable vector-based semantic recall. Default: `true`.
    #[serde(default = "default_semantic_enabled")]
    pub enabled: bool,
    #[serde(default = "default_recall_limit")]
    pub recall_limit: usize,
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
    #[serde(default = "default_true")]
    pub temporal_decay_enabled: bool,
    #[serde(default = "default_temporal_decay_half_life_days")]
    pub temporal_decay_half_life_days: u32,
    #[serde(default = "default_true")]
    pub mmr_enabled: bool,
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f32,
    #[serde(default = "default_true")]
    pub importance_enabled: bool,
    #[serde(
        default = "default_importance_weight",
        deserialize_with = "validate_importance_weight"
    )]
    pub importance_weight: f64,
    /// Name of a `[[llm.providers]]` entry to use exclusively for embedding calls during
    /// memory write and backfill operations. A dedicated provider prevents `embed_backfill`
    /// from contending with the guardrail at the API server level (rate limits, Ollama
    /// single-model lock). When unset or empty, falls back to the main agent provider.
    #[serde(default)]
    pub embed_provider: Option<String>,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: default_semantic_enabled(),
            recall_limit: default_recall_limit(),
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            temporal_decay_enabled: true,
            temporal_decay_half_life_days: default_temporal_decay_half_life_days(),
            mmr_enabled: true,
            mmr_lambda: default_mmr_lambda(),
            importance_enabled: true,
            importance_weight: default_importance_weight(),
            embed_provider: None,
        }
    }
}

/// Memory snippet rendering format injected into agent context (MM-F5, #3340).
///
/// Controls how each recalled memory entry is presented in the assembled prompt.
/// Flipping this value does not affect stored content — `SQLite` rows and Qdrant points
/// always contain the raw message text. The format is applied exclusively during
/// context assembly and is never persisted.
///
/// # Token cost
///
/// `Structured` headers add roughly 2–3× more tokens per entry than `Plain`.
/// Consider raising `memory.recall_tokens` proportionally when switching to `Structured`.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ContextFormat {
    /// Emit a labeled header per snippet:
    /// `[Memory | <source> | <date> | relevance: <score>]` followed by the content.
    ///
    /// This is the default. Gives the LLM structured provenance metadata for each recalled
    /// memory without re-parsing the recall body.
    #[default]
    Structured,
    /// Legacy plain format: `- [role] content` per snippet, byte-identical to pre-#3340.
    ///
    /// Use `Plain` when downstream consumers rely on the old format or when token budget
    /// is tight and provenance headers are not needed.
    Plain,
}

/// Retrieval-stage tuning for semantic memory (MemMachine-inspired, #3340).
///
/// Controls ANN candidate depth, search-prompt template, and memory snippet rendering.
/// Nested under `[memory.retrieval]` in TOML.  All fields have defaults so existing
/// configs parse unchanged.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.retrieval]
/// # depth = 0          # 0 = legacy (recall_limit * 2); set ≥ 1 to override directly
/// # search_prompt_template = ""
/// # context_format = "structured"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct RetrievalConfig {
    /// Number of ANN candidates fetched from the vector store before keyword merge,
    /// temporal decay, and MMR re-ranking.
    ///
    /// - `0` (default): legacy behavior — `recall_limit * 2` candidates, byte-identical
    ///   to pre-#3340 deployments.
    /// - `≥ 1`: the configured value is passed directly to `qdrant.search` /
    ///   `keyword_search`. Set to at least `recall_limit * 2` to match the legacy pool
    ///   size, or higher for better MMR diversity.
    ///
    /// A value below `recall_limit` triggers a one-shot WARN because the ANN pool
    /// cannot saturate the requested top-k.
    pub depth: u32,
    /// Template applied to the raw user query before embedding.
    ///
    /// Supports a single `{query}` placeholder which is replaced with the raw query string.
    /// Empty string (default) = identity: the query is embedded as-is.
    ///
    /// Applied **only** at query-side embedding sites — stored content (summaries, documents)
    /// is never wrapped.  Use this for asymmetric embedding models (e.g. E5 `"query: {query}"`).
    pub search_prompt_template: String,
    /// Shape of memory snippets injected into agent context.
    ///
    /// See [`ContextFormat`] for the exact rendering and token-cost implications.
    /// Default: `Structured`.
    pub context_format: ContextFormat,
    /// Enable query-bias correction towards the user's profile centroid (MM-F3, #3341).
    ///
    /// When `true` and the query is classified as first-person, the query embedding is
    /// shifted towards the centroid of persona-fact embeddings. This nudges recall results
    /// towards persona-relevant content for self-referential queries.
    ///
    /// Default: `true` (low blast-radius: no-op when the persona table is empty).
    #[serde(default = "default_query_bias_correction")]
    pub query_bias_correction: bool,
    /// Blend weight for query-bias correction (MM-F3, #3341).
    ///
    /// Controls how much the query embedding shifts towards the profile centroid.
    /// `0.0` = no shift; `1.0` = full centroid. Clamped to `[0.0, 1.0]`. Default: `0.25`.
    #[serde(default = "default_query_bias_profile_weight")]
    pub query_bias_profile_weight: f32,
    /// Centroid TTL in seconds (MM-F3, #3341).
    ///
    /// The profile centroid computed from persona facts is cached for this many seconds.
    /// After expiry it is recomputed on the next first-person query. Default: 300 (5 min).
    #[serde(default = "default_query_bias_centroid_ttl_secs")]
    pub query_bias_centroid_ttl_secs: u64,
}

fn default_query_bias_correction() -> bool {
    true
}

fn default_query_bias_profile_weight() -> f32 {
    0.25
}

fn default_query_bias_centroid_ttl_secs() -> u64 {
    300
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            depth: 0,
            search_prompt_template: String::new(),
            context_format: ContextFormat::default(),
            query_bias_correction: default_query_bias_correction(),
            query_bias_profile_weight: default_query_bias_profile_weight(),
            query_bias_centroid_ttl_secs: default_query_bias_centroid_ttl_secs(),
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
    /// Falls back to the main provider when empty or unresolvable. Default: `"fast"`.
    pub consolidate_provider: String,
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
    pub spread_edge_types: Vec<String>,
    /// Per-step circuit-breaker timeout for HL-F5 in milliseconds.
    ///
    /// Any internal step (anchor ANN, edges batch, vectors batch) that exceeds this
    /// duration triggers an `Ok(Vec::new())` fallback with a `WARN`. Default: `8`.
    pub step_budget_ms: u64,
}

impl Default for HebbianConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hebbian_lr: 0.1,
            consolidation_interval_secs: 3600,
            consolidation_threshold: 5.0,
            consolidate_provider: String::new(),
            max_candidates_per_sweep: 10,
            consolidation_cooldown_secs: 86_400,
            consolidation_prompt_timeout_secs: 30,
            consolidation_max_neighbors: 20,
            spreading_activation: false,
            spread_depth: 2,
            spread_edge_types: Vec::new(),
            step_budget_ms: 8,
        }
    }
}

/// Compression strategy for active context compression (#1161).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(tag = "strategy", rename_all = "snake_case")]
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
    pub probe: zeph_memory::CompactionProbeConfig,
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
}

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

/// Graph retrieval strategy for `[memory.graph]`.
///
/// Selects the algorithm used to traverse the knowledge graph during recall.
/// The default (`synapse`) preserves existing SYNAPSE spreading-activation behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Named LLM provider for hybrid strategy classification. Empty = use default provider.
    #[serde(default)]
    pub strategy_classifier_provider: String,
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
        deserialize_with = "validate_link_weight_decay_lambda"
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
}

fn default_graph_pool_size() -> u32 {
    3
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
            strategy_classifier_provider: String::new(),
            beam_search: BeamSearchConfig::default(),
            watercircles: WaterCirclesConfig::default(),
            experience: ExperienceConfig::default(),
            link_weight_decay_lambda: default_link_weight_decay_lambda(),
            link_weight_decay_interval_secs: default_link_weight_decay_interval_secs(),
            belief_revision: BeliefRevisionConfig::default(),
            rpe: RpeConfig::default(),
            pool_size: default_graph_pool_size(),
        }
    }
}

fn default_consolidation_confidence_threshold() -> f32 {
    0.7
}

fn default_consolidation_sweep_interval_secs() -> u64 {
    3600
}

fn default_consolidation_sweep_batch_size() -> usize {
    50
}

fn default_consolidation_similarity_threshold() -> f32 {
    0.85
}

/// Configuration for the All-Mem lifelong memory consolidation sweep (`[memory.consolidation]`).
///
/// When `enabled = true`, a background loop periodically clusters semantically similar messages
/// and merges them into consolidated entries via an LLM call. Originals are never deleted —
/// they are marked as consolidated and deprioritized in recall via temporal decay.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ConsolidationConfig {
    /// Enable the consolidation background loop. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for consolidation LLM calls.
    /// Falls back to the primary provider when empty. Default: `""`.
    #[serde(default)]
    pub consolidation_provider: ProviderName,
    /// Minimum LLM-assigned confidence for a topology op to be applied. Default: `0.7`.
    #[serde(default = "default_consolidation_confidence_threshold")]
    pub confidence_threshold: f32,
    /// How often the background consolidation sweep runs, in seconds. Default: `3600`.
    #[serde(default = "default_consolidation_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
    /// Maximum number of messages to evaluate per sweep cycle. Default: `50`.
    #[serde(default = "default_consolidation_sweep_batch_size")]
    pub sweep_batch_size: usize,
    /// Minimum cosine similarity for two messages to be considered consolidation candidates.
    /// Default: `0.85`.
    #[serde(default = "default_consolidation_similarity_threshold")]
    pub similarity_threshold: f32,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            consolidation_provider: ProviderName::default(),
            confidence_threshold: default_consolidation_confidence_threshold(),
            sweep_interval_secs: default_consolidation_sweep_interval_secs(),
            sweep_batch_size: default_consolidation_sweep_batch_size(),
            similarity_threshold: default_consolidation_similarity_threshold(),
        }
    }
}

fn default_link_weight_decay_lambda() -> f64 {
    0.95
}

fn default_link_weight_decay_interval_secs() -> u64 {
    86400
}

fn validate_link_weight_decay_lambda<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f64 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "link_weight_decay_lambda must be a finite number",
        ));
    }
    if !(value > 0.0 && value <= 1.0) {
        return Err(serde::de::Error::custom(
            "link_weight_decay_lambda must be in (0.0, 1.0]",
        ));
    }
    Ok(value)
}

fn validate_admission_threshold<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "threshold must be a finite number",
        ));
    }
    if !(0.0..=1.0).contains(&value) {
        return Err(serde::de::Error::custom("threshold must be in [0.0, 1.0]"));
    }
    Ok(value)
}

fn validate_admission_fast_path_margin<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "fast_path_margin must be a finite number",
        ));
    }
    if !(0.0..=1.0).contains(&value) {
        return Err(serde::de::Error::custom(
            "fast_path_margin must be in [0.0, 1.0]",
        ));
    }
    Ok(value)
}

fn default_admission_threshold() -> f32 {
    0.40
}

fn default_admission_fast_path_margin() -> f32 {
    0.15
}

fn default_rl_min_samples() -> u32 {
    500
}

fn default_rl_retrain_interval_secs() -> u64 {
    3600
}

/// Admission decision strategy.
///
/// `Heuristic` uses the existing multi-factor weighted score with an optional LLM call.
/// `Rl` replaces the LLM-based `future_utility` factor with a trained logistic regression model.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStrategy {
    /// Current A-MAC behavior: weighted heuristics + optional LLM call. Default.
    #[default]
    Heuristic,
    /// Learned model: logistic regression trained on recall feedback.
    /// Falls back to `Heuristic` when training data is below `rl_min_samples`.
    Rl,
}

fn validate_admission_weight<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value < 0.0 {
        return Err(serde::de::Error::custom(
            "admission weight must be non-negative (>= 0.0)",
        ));
    }
    Ok(value)
}

/// Per-factor weights for the A-MAC admission score (`[memory.admission.weights]`).
///
/// Weights are normalized at runtime (divided by their sum), so they do not need to sum to 1.0.
/// All values must be non-negative.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdmissionWeights {
    /// LLM-estimated future reuse probability. Default: `0.30`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub future_utility: f32,
    /// Factual confidence heuristic (inverse of hedging markers). Default: `0.15`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub factual_confidence: f32,
    /// Semantic novelty: 1 - max similarity to existing memories. Default: `0.30`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub semantic_novelty: f32,
    /// Temporal recency: always 1.0 at write time. Default: `0.10`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub temporal_recency: f32,
    /// Content type prior based on role. Default: `0.15`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub content_type_prior: f32,
    /// Goal-conditioned utility (#2408). `0.0` when `goal_conditioned_write = false`.
    /// When enabled, set this alongside reducing `future_utility` so total sums remain stable.
    /// Normalized automatically at runtime. Default: `0.0`.
    #[serde(deserialize_with = "validate_admission_weight")]
    pub goal_utility: f32,
}

impl Default for AdmissionWeights {
    fn default() -> Self {
        Self {
            future_utility: 0.30,
            factual_confidence: 0.15,
            semantic_novelty: 0.30,
            temporal_recency: 0.10,
            content_type_prior: 0.15,
            goal_utility: 0.0,
        }
    }
}

impl AdmissionWeights {
    /// Return weights normalized so they sum to 1.0.
    ///
    /// All weights are non-negative; the sum is always > 0 when defaults are used.
    #[must_use]
    pub fn normalized(&self) -> Self {
        let sum = self.future_utility
            + self.factual_confidence
            + self.semantic_novelty
            + self.temporal_recency
            + self.content_type_prior
            + self.goal_utility;
        if sum <= f32::EPSILON {
            return Self::default();
        }
        Self {
            future_utility: self.future_utility / sum,
            factual_confidence: self.factual_confidence / sum,
            semantic_novelty: self.semantic_novelty / sum,
            temporal_recency: self.temporal_recency / sum,
            content_type_prior: self.content_type_prior / sum,
            goal_utility: self.goal_utility / sum,
        }
    }
}

/// Configuration for A-MAC adaptive memory admission control (`[memory.admission]` TOML section).
///
/// When `enabled = true`, a write-time gate evaluates each message before saving to memory.
/// Messages below the composite admission threshold are rejected and not persisted.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdmissionConfig {
    /// Enable A-MAC admission control. Default: `false`.
    pub enabled: bool,
    /// Composite score threshold below which messages are rejected. Range: `[0.0, 1.0]`.
    /// Default: `0.40`.
    #[serde(deserialize_with = "validate_admission_threshold")]
    pub threshold: f32,
    /// Margin above threshold at which the fast path admits without an LLM call. Range: `[0.0, 1.0]`.
    /// When heuristic score >= threshold + margin, LLM call is skipped. Default: `0.15`.
    #[serde(deserialize_with = "validate_admission_fast_path_margin")]
    pub fast_path_margin: f32,
    /// Provider name from `[[llm.providers]]` for `future_utility` LLM evaluation.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub admission_provider: ProviderName,
    /// Per-factor weights. Normalized at runtime. Default: `{0.30, 0.15, 0.30, 0.10, 0.15}`.
    pub weights: AdmissionWeights,
    /// Admission decision strategy. Default: `heuristic`.
    #[serde(default)]
    pub admission_strategy: AdmissionStrategy,
    /// Minimum training samples before the RL model is activated.
    /// Below this count the system falls back to `Heuristic`. Default: `500`.
    #[serde(default = "default_rl_min_samples")]
    pub rl_min_samples: u32,
    /// Background RL model retraining interval in seconds. Default: `3600`.
    #[serde(default = "default_rl_retrain_interval_secs")]
    pub rl_retrain_interval_secs: u64,
    /// Enable goal-conditioned write gate (#2408). When `true`, memories are scored
    /// against the current task goal and rejected if relevance is below `goal_utility_threshold`.
    /// Zero regression when `false`. Default: `false`.
    #[serde(default)]
    pub goal_conditioned_write: bool,
    /// Provider name from `[[llm.providers]]` for goal-utility LLM refinement.
    /// Used only for borderline cases (similarity within 0.1 of threshold).
    /// Falls back to the primary provider when empty. Default: `""`.
    #[serde(default)]
    pub goal_utility_provider: ProviderName,
    /// Minimum cosine similarity between goal embedding and candidate memory
    /// to consider it goal-relevant. Below this, `goal_utility = 0.0`. Default: `0.4`.
    #[serde(default = "default_goal_utility_threshold")]
    pub goal_utility_threshold: f32,
    /// Weight of the `goal_utility` factor in the composite admission score.
    /// Set to `0.0` to disable (equivalent to `goal_conditioned_write = false`). Default: `0.25`.
    #[serde(default = "default_goal_utility_weight")]
    pub goal_utility_weight: f32,
}

fn default_goal_utility_threshold() -> f32 {
    0.4
}

fn default_goal_utility_weight() -> f32 {
    0.25
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_admission_threshold(),
            fast_path_margin: default_admission_fast_path_margin(),
            admission_provider: ProviderName::default(),
            weights: AdmissionWeights::default(),
            admission_strategy: AdmissionStrategy::default(),
            rl_min_samples: default_rl_min_samples(),
            rl_retrain_interval_secs: default_rl_retrain_interval_secs(),
            goal_conditioned_write: false,
            goal_utility_provider: ProviderName::default(),
            goal_utility_threshold: default_goal_utility_threshold(),
            goal_utility_weight: default_goal_utility_weight(),
        }
    }
}

/// Routing strategy for `[memory.store_routing]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreRoutingStrategy {
    /// Pure heuristic pattern matching. Zero LLM calls. Default.
    #[default]
    Heuristic,
    /// LLM-based classification via `routing_classifier_provider`.
    Llm,
    /// Heuristic first; escalates to LLM only when confidence is low.
    Hybrid,
}

/// Configuration for cost-sensitive store routing (`[memory.store_routing]`).
///
/// Controls how each query is classified and routed to the appropriate memory
/// backend(s), avoiding unnecessary store queries for simple lookups.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct StoreRoutingConfig {
    /// Enable configurable store routing. When `false`, `HeuristicRouter` is used
    /// directly (existing behavior). Default: `false`.
    pub enabled: bool,
    /// Routing strategy. Default: `heuristic`.
    pub strategy: StoreRoutingStrategy,
    /// Provider name from `[[llm.providers]]` for LLM-based classification.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub routing_classifier_provider: ProviderName,
    /// Route to use when the classifier is uncertain (confidence < threshold).
    /// Default: `"hybrid"`.
    pub fallback_route: String,
    /// Confidence threshold below which `HybridRouter` escalates to LLM.
    /// Range: `[0.0, 1.0]`. Default: `0.7`.
    pub confidence_threshold: f32,
}

impl Default for StoreRoutingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: StoreRoutingStrategy::Heuristic,
            routing_classifier_provider: ProviderName::default(),
            fallback_route: "hybrid".into(),
            confidence_threshold: 0.7,
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

/// autoDream background memory consolidation configuration (#2697).
///
/// When `enabled = true`, a constrained consolidation subagent runs after
/// a session ends if both `min_sessions` and `min_hours` gates pass.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct AutoDreamConfig {
    /// Enable autoDream consolidation. Default: `false`.
    pub enabled: bool,
    /// Minimum number of sessions between consolidations. Default: `3`.
    pub min_sessions: u32,
    /// Minimum hours between consolidations. Default: `24`.
    pub min_hours: u32,
    /// Provider name from `[[llm.providers]]` for consolidation LLM calls.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub consolidation_provider: ProviderName,
    /// Maximum agent loop iterations for the consolidation subagent. Default: `8`.
    pub max_iterations: u8,
}

impl Default for AutoDreamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_sessions: 3,
            min_hours: 24,
            consolidation_provider: ProviderName::default(),
            max_iterations: 8,
        }
    }
}

/// `MagicDocs` auto-maintained markdown configuration (#2702).
///
/// When `enabled = true`, files read via file tools that contain a `# MAGIC DOC:` header
/// are registered and periodically updated by a constrained subagent.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct MagicDocsConfig {
    /// Enable `MagicDocs` auto-maintenance. Default: `false`.
    pub enabled: bool,
    /// Minimum turns between updates for a given doc path. Default: `5`.
    pub min_turns_between_updates: u32,
    /// Provider name from `[[llm.providers]]` for doc update LLM calls.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub update_provider: ProviderName,
    /// Maximum agent loop iterations per doc update. Default: `4`.
    pub max_iterations: u8,
}

impl Default for MagicDocsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_turns_between_updates: 5,
            update_provider: ProviderName::default(),
            max_iterations: 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verify that serde deserialization routes through FromStr so that removed variants
    // (task_aware_mig) fall back to Reactive instead of hard-erroring when found in TOML.
    #[test]
    fn pruning_strategy_toml_task_aware_mig_falls_back_to_reactive() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            pruning_strategy: PruningStrategy,
        }
        let toml = r#"pruning_strategy = "task_aware_mig""#;
        let w: Wrapper = toml::from_str(toml).expect("should deserialize without error");
        assert_eq!(
            w.pruning_strategy,
            PruningStrategy::Reactive,
            "task_aware_mig must fall back to Reactive"
        );
    }

    #[test]
    fn pruning_strategy_toml_round_trip() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            pruning_strategy: PruningStrategy,
        }
        for (input, expected) in [
            ("reactive", PruningStrategy::Reactive),
            ("task_aware", PruningStrategy::TaskAware),
            ("mig", PruningStrategy::Mig),
        ] {
            let toml = format!(r#"pruning_strategy = "{input}""#);
            let w: Wrapper = toml::from_str(&toml)
                .unwrap_or_else(|e| panic!("failed to deserialize `{input}`: {e}"));
            assert_eq!(w.pruning_strategy, expected, "mismatch for `{input}`");
        }
    }

    #[test]
    fn pruning_strategy_toml_unknown_value_errors() {
        #[derive(serde::Deserialize)]
        #[allow(dead_code)]
        struct Wrapper {
            pruning_strategy: PruningStrategy,
        }
        let toml = r#"pruning_strategy = "nonexistent_strategy""#;
        assert!(
            toml::from_str::<Wrapper>(toml).is_err(),
            "unknown strategy must produce an error"
        );
    }

    #[test]
    fn tier_config_defaults_are_correct() {
        let cfg = TierConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.promotion_min_sessions, 3);
        assert!((cfg.similarity_threshold - 0.92).abs() < f32::EPSILON);
        assert_eq!(cfg.sweep_interval_secs, 3600);
        assert_eq!(cfg.sweep_batch_size, 100);
    }

    #[test]
    fn tier_config_rejects_min_sessions_below_2() {
        let toml = "promotion_min_sessions = 1";
        assert!(toml::from_str::<TierConfig>(toml).is_err());
    }

    #[test]
    fn tier_config_rejects_similarity_threshold_below_0_5() {
        let toml = "similarity_threshold = 0.4";
        assert!(toml::from_str::<TierConfig>(toml).is_err());
    }

    #[test]
    fn tier_config_rejects_zero_sweep_batch_size() {
        let toml = "sweep_batch_size = 0";
        assert!(toml::from_str::<TierConfig>(toml).is_err());
    }

    fn deserialize_importance_weight(toml_val: &str) -> Result<SemanticConfig, toml::de::Error> {
        let input = format!("importance_weight = {toml_val}");
        toml::from_str::<SemanticConfig>(&input)
    }

    #[test]
    fn importance_weight_default_is_0_15() {
        let cfg = SemanticConfig::default();
        assert!((cfg.importance_weight - 0.15).abs() < f64::EPSILON);
    }

    #[test]
    fn importance_weight_valid_zero() {
        let cfg = deserialize_importance_weight("0.0").unwrap();
        assert!((cfg.importance_weight - 0.0_f64).abs() < f64::EPSILON);
    }

    #[test]
    fn importance_weight_valid_one() {
        let cfg = deserialize_importance_weight("1.0").unwrap();
        assert!((cfg.importance_weight - 1.0_f64).abs() < f64::EPSILON);
    }

    #[test]
    fn importance_weight_rejects_near_zero_negative() {
        // TOML does not have a NaN literal, but we can test via a f64 that
        // the validator rejects out-of-range values. Test with negative here
        // and rely on validate_importance_weight rejecting non-finite via
        // a constructed deserializer call.
        let result = deserialize_importance_weight("-0.01");
        assert!(
            result.is_err(),
            "negative importance_weight must be rejected"
        );
    }

    #[test]
    fn importance_weight_rejects_negative() {
        let result = deserialize_importance_weight("-1.0");
        assert!(result.is_err(), "negative value must be rejected");
    }

    #[test]
    fn importance_weight_rejects_greater_than_one() {
        let result = deserialize_importance_weight("1.01");
        assert!(result.is_err(), "value > 1.0 must be rejected");
    }

    // ── AdmissionWeights::normalized() tests (#2317) ────────────────────────

    // Test: weights that don't sum to 1.0 are normalized to sum to 1.0.
    #[test]
    fn admission_weights_normalized_sums_to_one() {
        let w = AdmissionWeights {
            future_utility: 2.0,
            factual_confidence: 1.0,
            semantic_novelty: 3.0,
            temporal_recency: 1.0,
            content_type_prior: 3.0,
            goal_utility: 0.0,
        };
        let n = w.normalized();
        let sum = n.future_utility
            + n.factual_confidence
            + n.semantic_novelty
            + n.temporal_recency
            + n.content_type_prior;
        assert!(
            (sum - 1.0).abs() < 0.001,
            "normalized weights must sum to 1.0, got {sum}"
        );
    }

    // Test: already-normalized weights are preserved.
    #[test]
    fn admission_weights_normalized_preserves_already_unit_sum() {
        let w = AdmissionWeights::default();
        let n = w.normalized();
        let sum = n.future_utility
            + n.factual_confidence
            + n.semantic_novelty
            + n.temporal_recency
            + n.content_type_prior;
        assert!(
            (sum - 1.0).abs() < 0.001,
            "default weights sum to ~1.0 after normalization"
        );
    }

    // Test: zero weights fall back to default (no divide-by-zero panic).
    #[test]
    fn admission_weights_normalized_zero_sum_falls_back_to_default() {
        let w = AdmissionWeights {
            future_utility: 0.0,
            factual_confidence: 0.0,
            semantic_novelty: 0.0,
            temporal_recency: 0.0,
            content_type_prior: 0.0,
            goal_utility: 0.0,
        };
        let n = w.normalized();
        let default = AdmissionWeights::default();
        assert!(
            (n.future_utility - default.future_utility).abs() < 0.001,
            "zero-sum weights must fall back to defaults"
        );
    }

    // Test: AdmissionConfig default values match documented defaults.
    #[test]
    fn admission_config_defaults() {
        let cfg = AdmissionConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.threshold - 0.40).abs() < 0.001);
        assert!((cfg.fast_path_margin - 0.15).abs() < 0.001);
        assert!(cfg.admission_provider.is_empty());
    }

    // ── SpreadingActivationConfig tests (#2514) ──────────────────────────────

    #[test]
    fn spreading_activation_default_recall_timeout_ms_is_1000() {
        let cfg = SpreadingActivationConfig::default();
        assert_eq!(
            cfg.recall_timeout_ms, 1000,
            "default recall_timeout_ms must be 1000ms"
        );
    }

    #[test]
    fn spreading_activation_toml_recall_timeout_ms_round_trip() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            recall_timeout_ms: u64,
        }
        let toml = "recall_timeout_ms = 500";
        let w: Wrapper = toml::from_str(toml).unwrap();
        assert_eq!(w.recall_timeout_ms, 500);
    }

    #[test]
    fn spreading_activation_validate_cross_field_constraints() {
        let mut cfg = SpreadingActivationConfig::default();
        // Default activation_threshold (0.1) < inhibition_threshold (0.8) → must be Ok.
        assert!(cfg.validate().is_ok());

        // Equal thresholds must be rejected.
        cfg.activation_threshold = 0.5;
        cfg.inhibition_threshold = 0.5;
        assert!(cfg.validate().is_err());
    }

    // ─── CompressionConfig: new Focus fields deserialization (#2510, #2481) ──

    #[test]
    fn compression_config_focus_strategy_deserializes() {
        let toml = r#"strategy = "focus""#;
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.strategy, CompressionStrategy::Focus);
    }

    #[test]
    fn compression_config_density_budget_defaults_on_deserialize() {
        // `#[serde(default = "...")]` applies during deserialization, not via Default::default().
        // Verify that omitting both fields yields the serde defaults (0.7 / 0.3).
        let toml = r#"strategy = "reactive""#;
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert!((cfg.high_density_budget - 0.7).abs() < 1e-6);
        assert!((cfg.low_density_budget - 0.3).abs() < 1e-6);
    }

    #[test]
    fn compression_config_density_budget_round_trip() {
        let toml = "strategy = \"reactive\"\nhigh_density_budget = 0.6\nlow_density_budget = 0.4";
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert!((cfg.high_density_budget - 0.6).abs() < f32::EPSILON);
        assert!((cfg.low_density_budget - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn compression_config_focus_scorer_provider_default_empty() {
        let cfg = CompressionConfig::default();
        assert!(cfg.focus_scorer_provider.is_empty());
    }

    #[test]
    fn compression_config_focus_scorer_provider_round_trip() {
        let toml = "strategy = \"focus\"\nfocus_scorer_provider = \"fast\"";
        let cfg: CompressionConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.focus_scorer_provider.as_str(), "fast");
    }
}

/// `ReasoningBank`: distilled reasoning strategy memory configuration (#3342).
///
/// When `enabled = true`, each completed agent turn is evaluated by a self-judge LLM call.
/// Successful and failed reasoning chains are compressed into short, generalizable strategy
/// summaries. At context-build time, top-k strategies are retrieved by embedding similarity
/// and injected into the prompt preamble.
///
/// All LLM work (self-judge, distillation) runs asynchronously — never on the turn thread.
///
/// # Example
///
/// ```toml
/// [memory.reasoning]
/// enabled = true
/// extract_provider = "fast"
/// distill_provider = "fast"
/// top_k = 3
/// store_limit = 1000
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ReasoningConfig {
    /// Enable the reasoning-bank pipeline. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for the self-judge step.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub extract_provider: ProviderName,
    /// Provider name from `[[llm.providers]]` for the distillation step.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub distill_provider: ProviderName,
    /// Number of strategies retrieved per turn for context injection. Default: `3`.
    pub top_k: usize,
    /// Maximum stored strategies; oldest unused are evicted when limit is reached. Default: `1000`.
    pub store_limit: usize,
    /// Maximum number of recent messages passed to the self-judge LLM. Default: `6`.
    pub max_messages: usize,
    /// Per-message content truncation limit (chars) before building the judge transcript. Default: `2000`.
    pub max_message_chars: usize,
    /// Maximum token budget for injected reasoning strategies in context. Default: `500`.
    pub context_budget_tokens: usize,
    /// Minimum number of messages required before self-judge fires. Default: `2`.
    pub min_messages: usize,
    /// Timeout in seconds for the self-judge LLM call. Default: `30`.
    pub extraction_timeout_secs: u64,
    /// Timeout in seconds for the distillation LLM call. Default: `30`.
    pub distill_timeout_secs: u64,
    /// Maximum number of recent messages passed to the self-judge evaluator.
    /// Narrowing to the last user+assistant pair improves classification accuracy.
    /// Default: `2`.
    pub self_judge_window: usize,
    /// Minimum characters in the assistant response to trigger self-judge.
    /// Short or trivial responses are skipped. Default: `50`.
    pub min_assistant_chars: usize,
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            extract_provider: ProviderName::default(),
            distill_provider: ProviderName::default(),
            top_k: 3,
            store_limit: 1000,
            max_messages: 6,
            max_message_chars: 2000,
            context_budget_tokens: 500,
            min_messages: 2,
            extraction_timeout_secs: 30,
            distill_timeout_secs: 30,
            self_judge_window: 2,
            min_assistant_chars: 50,
        }
    }
}
