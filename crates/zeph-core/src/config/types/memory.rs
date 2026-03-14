// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use super::defaults::default_sqlite_path_field;

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
    0.70
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
pub struct MemoryConfig {
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
    pub routing: RoutingConfig,
    #[serde(default)]
    pub graph: GraphConfig,
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

/// Configuration for active context compression (#1161).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CompressionConfig {
    /// Compression strategy.
    #[serde(flatten)]
    pub strategy: CompressionStrategy,
    /// Model to use for compression summaries.
    ///
    /// Currently unused — the primary summary provider is used regardless of this value.
    /// Reserved for future per-compression model selection. Setting this field has no effect.
    pub model: String,
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
