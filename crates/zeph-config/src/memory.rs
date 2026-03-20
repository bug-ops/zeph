// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::defaults::default_sqlite_path_field;

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
    10
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
    #[default]
    Qdrant,
    Sqlite,
}

impl VectorBackend {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Qdrant => "qdrant",
            Self::Sqlite => "sqlite",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
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
    #[serde(default)]
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
    pub routing: RoutingConfig,
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
}

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

#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SemanticConfig {
    #[serde(default = "default_semantic_enabled")]
    pub enabled: bool,
    #[serde(default = "default_recall_limit")]
    pub recall_limit: usize,
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    #[serde(default = "default_keyword_weight")]
    pub keyword_weight: f64,
    #[serde(default)]
    pub temporal_decay_enabled: bool,
    #[serde(default = "default_temporal_decay_half_life_days")]
    pub temporal_decay_half_life_days: u32,
    #[serde(default)]
    pub mmr_enabled: bool,
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f32,
    #[serde(default)]
    pub importance_enabled: bool,
    #[serde(
        default = "default_importance_weight",
        deserialize_with = "validate_importance_weight"
    )]
    pub importance_weight: f64,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: default_semantic_enabled(),
            recall_limit: default_recall_limit(),
            vector_weight: default_vector_weight(),
            keyword_weight: default_keyword_weight(),
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: default_temporal_decay_half_life_days(),
            mmr_enabled: false,
            mmr_lambda: default_mmr_lambda(),
            importance_enabled: false,
            importance_weight: default_importance_weight(),
        }
    }
}

/// Routing strategy for memory backend selection.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    /// Heuristic-based routing using query characteristics.
    #[default]
    Heuristic,
}

/// Configuration for query-aware memory routing (#1162).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// Routing strategy. Currently only `heuristic` is supported.
    pub strategy: RoutingStrategy,
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
}

/// Pruning strategy for tool-output eviction inside the compaction pipeline (#1851, #2022).
///
/// When `context-compression` feature is enabled, this replaces the default oldest-first
/// heuristic with scored eviction.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
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
    /// Combined `TaskAware` goal extraction + MIG scoring.
    /// Requires `context-compression` feature.
    TaskAwareMig,
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

impl std::str::FromStr for PruningStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "reactive" => Ok(Self::Reactive),
            "task_aware" | "task-aware" => Ok(Self::TaskAware),
            "mig" => Ok(Self::Mig),
            "task_aware_mig" | "task-aware-mig" => Ok(Self::TaskAwareMig),
            "subgoal" => Ok(Self::Subgoal),
            "subgoal_mig" | "subgoal-mig" => Ok(Self::SubgoalMig),
            other => Err(format!(
                "unknown pruning strategy `{other}`, expected \
                 reactive|task_aware|mig|task_aware_mig|subgoal|subgoal_mig"
            )),
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
    /// Compaction probe: validates summary quality before committing it (#1609).
    #[serde(default)]
    pub probe: zeph_memory::CompactionProbeConfig,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(cfg.importance_weight, 0.0);
    }

    #[test]
    fn importance_weight_valid_one() {
        let cfg = deserialize_importance_weight("1.0").unwrap();
        assert_eq!(cfg.importance_weight, 1.0);
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
}
