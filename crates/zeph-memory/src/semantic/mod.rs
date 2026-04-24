// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! High-level semantic memory orchestrator.
//!
//! [`SemanticMemory`] is the primary entry point used by `zeph-core`.  It wires
//! together [`crate::store::SqliteStore`] (relational persistence) and
//! [`crate::embedding_store::EmbeddingStore`] (Qdrant vector index) into a single
//! object with `remember` / `recall` / `summarize` operations.
//!
//! # Construction
//!
//! Use [`SemanticMemory::new`] for the default 0.7/0.3 vector/keyword weights, or
//! [`SemanticMemory::with_qdrant_ops`] inside `AppBuilder` to share a single gRPC
//! channel across all subsystems.
//!
//! # Hybrid recall
//!
//! Recall uses reciprocal-rank fusion of BM25 (`SQLite` FTS5) and cosine-similarity
//! (Qdrant) results, with optional temporal decay, MMR diversity reranking, and
//! per-tier score boosts.

mod algorithms;
mod corrections;
mod cross_session;
mod graph;
pub(crate) mod importance;
pub mod persona;
mod recall;
mod summarization;
pub mod trajectory;
pub mod tree_consolidation;
pub(crate) mod write_buffer;

#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::admission::AdmissionControl;
use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::token_counter::TokenCounter;

pub(crate) const SESSION_SUMMARIES_COLLECTION: &str = "zeph_session_summaries";
pub(crate) const KEY_FACTS_COLLECTION: &str = "zeph_key_facts";
pub(crate) const CORRECTIONS_COLLECTION: &str = "zeph_corrections";

/// Progress state for embed backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillProgress {
    /// Number of messages processed so far (including failures).
    pub done: usize,
    /// Total number of unembedded messages at backfill start.
    pub total: usize,
}

pub use algorithms::{apply_mmr, apply_temporal_decay};
pub use cross_session::SessionSummaryResult;
pub use graph::{
    ExtractionResult, ExtractionStats, GraphExtractionConfig, LinkingStats, NoteLinkingConfig,
    PostExtractValidator, extract_and_store, link_memory_notes,
};
pub use persona::{
    PersonaExtractionConfig, contains_self_referential_language, extract_persona_facts,
};
pub use recall::{EmbedContext, RecalledMessage};
pub use summarization::{StructuredSummary, Summary, build_summarization_prompt};
pub use trajectory::{TrajectoryEntry, TrajectoryExtractionConfig, extract_trajectory_entries};
pub use tree_consolidation::{
    TreeConsolidationConfig, TreeConsolidationResult, run_tree_consolidation_sweep,
    start_tree_consolidation_loop,
};
pub use write_buffer::{BufferedWrite, WriteBuffer};

/// High-level semantic memory orchestrator combining `SQLite` and Qdrant.
///
/// Instantiate via [`SemanticMemory::new`] or the `AppBuilder` integration.
/// All fields are `pub(crate)` — callers interact through the inherent method API.
pub struct SemanticMemory {
    pub(crate) sqlite: SqliteStore,
    pub(crate) qdrant: Option<Arc<EmbeddingStore>>,
    pub(crate) provider: AnyProvider,
    /// Dedicated provider for batch embedding calls (backfill, write-path embedding).
    ///
    /// When `Some`, all embedding I/O is routed through this provider instead of `provider`.
    /// This prevents `embed_backfill` from saturating the main provider and causing guardrail
    /// timeouts. When `None`, falls back to `provider`.
    pub(crate) embed_provider: Option<AnyProvider>,
    pub(crate) embedding_model: String,
    pub(crate) vector_weight: f64,
    pub(crate) keyword_weight: f64,
    pub(crate) temporal_decay_enabled: bool,
    pub(crate) temporal_decay_half_life_days: u32,
    pub(crate) mmr_enabled: bool,
    pub(crate) mmr_lambda: f32,
    pub(crate) importance_enabled: bool,
    pub(crate) importance_weight: f64,
    /// Multiplicative score boost for semantic-tier messages in recall ranking.
    /// Default: `1.3`. Disabled when set to `1.0`.
    pub(crate) tier_boost_semantic: f64,
    pub token_counter: Arc<TokenCounter>,
    pub graph_store: Option<Arc<crate::graph::GraphStore>>,
    /// Experience store for tool-outcome telemetry and per-turn evolution sweeps.
    ///
    /// `Some` when `memory.graph.experience.enabled = true` at bootstrap.
    pub experience: Option<Arc<crate::graph::experience::ExperienceStore>>,
    /// `ReasoningBank` store for distilled reasoning strategies (#3342).
    ///
    /// `Some` when `memory.reasoning.enabled = true` at bootstrap.
    pub reasoning: Option<Arc<crate::reasoning::ReasoningMemory>>,
    pub(crate) community_detection_failures: Arc<AtomicU64>,
    pub(crate) graph_extraction_count: Arc<AtomicU64>,
    pub(crate) graph_extraction_failures: Arc<AtomicU64>,
    pub(crate) last_qdrant_warn: Arc<AtomicU64>,
    /// A-MAC admission control gate. When `Some`, each `remember()` call is evaluated.
    pub(crate) admission_control: Option<Arc<AdmissionControl>>,
    /// Write quality gate. When `Some`, evaluated in `remember()`/`remember_with_parts()`
    /// after A-MAC admission and before persistence.
    pub(crate) quality_gate: Option<Arc<crate::quality_gate::QualityGate>>,
    /// Cosine similarity threshold for skipping near-duplicate key facts (0.0–1.0).
    /// When a new fact's nearest neighbour in `zeph_key_facts` has score >= this value,
    /// the fact is considered a duplicate and not inserted.  Default: `0.95`.
    pub(crate) key_facts_dedup_threshold: f32,
    /// Bounded set of in-flight background embed tasks.
    ///
    /// Guarded by a `Mutex` because `SemanticMemory` is shared via `Arc` and
    /// `JoinSet` requires `&mut self` for `spawn`. Capacity is capped at
    /// `MAX_EMBED_BG_TASKS`; tasks that exceed the limit are dropped with a debug log.
    pub(crate) embed_tasks: Mutex<tokio::task::JoinSet<()>>,
    /// ANN candidate count fetched from the vector store before reranking (MM-F1, #3340).
    ///
    /// `0` = legacy behavior (`recall_limit * 2`). `≥ 1` = direct count.
    pub(crate) retrieval_depth: u32,
    /// Template applied to raw user queries before embedding (MM-F2, #3340).
    ///
    /// Empty string = identity (pass raw query through). Applied at query-side embed sites only;
    /// never applied to stored content (summaries, documents).
    pub(crate) search_prompt_template: String,
    /// Fires `tracing::warn!` once per instance when `retrieval_depth < recall_limit`.
    pub(crate) depth_below_limit_warned: Arc<std::sync::atomic::AtomicBool>,
    /// Fires `tracing::warn!` once per instance when `search_prompt_template` has no `{query}`.
    pub(crate) missing_placeholder_warned: Arc<std::sync::atomic::AtomicBool>,
}

impl SemanticMemory {
    /// Create a new `SemanticMemory` instance with default hybrid search weights (0.7/0.3).
    ///
    /// Qdrant connection is best-effort: if unavailable, semantic search is disabled.
    ///
    /// For `AppBuilder` bootstrap, prefer [`SemanticMemory::with_qdrant_ops`] to share
    /// a single gRPC channel across all subsystems.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn new(
        sqlite_path: &str,
        qdrant_url: &str,
        provider: AnyProvider,
        embedding_model: &str,
    ) -> Result<Self, MemoryError> {
        Self::with_weights(sqlite_path, qdrant_url, provider, embedding_model, 0.7, 0.3).await
    }

    /// Create a new `SemanticMemory` with custom vector/keyword weights for hybrid search.
    ///
    /// For `AppBuilder` bootstrap, prefer [`SemanticMemory::with_qdrant_ops`] to share
    /// a single gRPC channel across all subsystems.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_weights(
        sqlite_path: &str,
        qdrant_url: &str,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
    ) -> Result<Self, MemoryError> {
        Self::with_weights_and_pool_size(
            sqlite_path,
            qdrant_url,
            provider,
            embedding_model,
            vector_weight,
            keyword_weight,
            5,
        )
        .await
    }

    /// Create a new `SemanticMemory` with custom weights and configurable pool size.
    ///
    /// For `AppBuilder` bootstrap, prefer [`SemanticMemory::with_qdrant_ops`] to share
    /// a single gRPC channel across all subsystems.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_weights_and_pool_size(
        sqlite_path: &str,
        qdrant_url: &str,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
        pool_size: u32,
    ) -> Result<Self, MemoryError> {
        let sqlite = SqliteStore::with_pool_size(sqlite_path, pool_size).await?;
        let pool = sqlite.pool().clone();

        let qdrant = match EmbeddingStore::new(qdrant_url, pool) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!("Qdrant unavailable, semantic search disabled: {e:#}");
                None
            }
        };

        Ok(Self {
            sqlite,
            qdrant,
            provider,
            embed_provider: None,
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            importance_enabled: false,
            importance_weight: 0.15,
            tier_boost_semantic: 1.3,
            token_counter: Arc::new(TokenCounter::new()),
            graph_store: None,
            experience: None,
            reasoning: None,
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            last_qdrant_warn: Arc::new(AtomicU64::new(0)),
            admission_control: None,
            quality_gate: None,
            key_facts_dedup_threshold: 0.95,
            embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            retrieval_depth: 0,
            search_prompt_template: String::new(),
            depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Create a `SemanticMemory` from a pre-built `QdrantOps` instance.
    ///
    /// Use this at bootstrap to share one `QdrantOps` (and thus one gRPC channel)
    /// across all subsystems. The `ops` is consumed and wrapped inside `EmbeddingStore`.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_qdrant_ops(
        sqlite_path: &str,
        ops: crate::QdrantOps,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
        pool_size: u32,
    ) -> Result<Self, MemoryError> {
        let sqlite = SqliteStore::with_pool_size(sqlite_path, pool_size).await?;
        let pool = sqlite.pool().clone();
        let store = EmbeddingStore::with_store(Box::new(ops), pool);

        Ok(Self {
            sqlite,
            qdrant: Some(Arc::new(store)),
            provider,
            embed_provider: None,
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            importance_enabled: false,
            importance_weight: 0.15,
            tier_boost_semantic: 1.3,
            token_counter: Arc::new(TokenCounter::new()),
            graph_store: None,
            experience: None,
            reasoning: None,
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            last_qdrant_warn: Arc::new(AtomicU64::new(0)),
            admission_control: None,
            quality_gate: None,
            key_facts_dedup_threshold: 0.95,
            embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            retrieval_depth: 0,
            search_prompt_template: String::new(),
            depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Attach a `GraphStore` for graph-aware retrieval.
    ///
    /// When set, `recall_graph` traverses the graph starting from entities
    /// matched by the query.
    #[must_use]
    pub fn with_graph_store(mut self, store: Arc<crate::graph::GraphStore>) -> Self {
        self.graph_store = Some(store);
        self
    }

    /// Attach an [`ExperienceStore`](crate::graph::experience::ExperienceStore) for tool-outcome
    /// telemetry and per-turn evolution sweeps.
    ///
    /// When set, the agent records one row per tool invocation in `experience_nodes` and
    /// periodically runs `evolution_sweep` to prune low-confidence and self-loop edges.
    #[must_use]
    pub fn with_experience_store(
        mut self,
        store: Arc<crate::graph::experience::ExperienceStore>,
    ) -> Self {
        self.experience = Some(store);
        self
    }

    /// Attach a [`ReasoningMemory`](crate::reasoning::ReasoningMemory) store for
    /// distilled reasoning strategy storage and retrieval (#3342).
    ///
    /// When set, [`SemanticMemory::retrieve_reasoning_strategies`] uses this store for
    /// embedding-similarity lookups. When `None`, retrieval returns an empty vec.
    #[must_use]
    pub fn with_reasoning(mut self, store: Arc<crate::reasoning::ReasoningMemory>) -> Self {
        self.reasoning = Some(store);
        self
    }

    /// Returns the cumulative count of community detection failures since startup.
    #[must_use]
    pub fn community_detection_failures(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.community_detection_failures.load(Ordering::Relaxed)
    }

    /// Returns the cumulative count of successful graph extractions since startup.
    #[must_use]
    pub fn graph_extraction_count(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.graph_extraction_count.load(Ordering::Relaxed)
    }

    /// Returns the cumulative count of failed graph extractions since startup.
    #[must_use]
    pub fn graph_extraction_failures(&self) -> u64 {
        use std::sync::atomic::Ordering;
        self.graph_extraction_failures.load(Ordering::Relaxed)
    }

    /// Configure temporal decay and MMR re-ranking options.
    #[must_use]
    pub fn with_ranking_options(
        mut self,
        temporal_decay_enabled: bool,
        temporal_decay_half_life_days: u32,
        mmr_enabled: bool,
        mmr_lambda: f32,
    ) -> Self {
        self.temporal_decay_enabled = temporal_decay_enabled;
        self.temporal_decay_half_life_days = temporal_decay_half_life_days;
        self.mmr_enabled = mmr_enabled;
        self.mmr_lambda = mmr_lambda;
        self
    }

    /// Configure write-time importance scoring for memory retrieval.
    #[must_use]
    pub fn with_importance_options(mut self, enabled: bool, weight: f64) -> Self {
        self.importance_enabled = enabled;
        self.importance_weight = weight;
        self
    }

    /// Configure the multiplicative score boost applied to semantic-tier messages during recall.
    ///
    /// Set to `1.0` to disable the boost. Default: `1.3`.
    #[must_use]
    pub fn with_tier_boost(mut self, boost: f64) -> Self {
        self.tier_boost_semantic = boost;
        self
    }

    /// Attach an A-MAC admission controller.
    ///
    /// When set, `remember()` and `remember_with_parts()` evaluate each message before persisting.
    /// Messages below the admission threshold return `Ok(None)` without incrementing counts.
    #[must_use]
    pub fn with_admission_control(mut self, control: AdmissionControl) -> Self {
        self.admission_control = Some(Arc::new(control));
        self
    }

    /// Attach a write quality gate that scores each `remember()` call before persisting.
    ///
    /// When set, the gate is evaluated after A-MAC admission. A `Some(reason)` result from
    /// [`crate::quality_gate::QualityGate::evaluate`] causes the write to be skipped
    /// and `Ok(None)` / `Ok((None, false))` to be returned.
    #[must_use]
    pub fn with_quality_gate(mut self, gate: Arc<crate::quality_gate::QualityGate>) -> Self {
        self.quality_gate = Some(gate);
        self
    }

    /// Set the cosine similarity threshold used to skip near-duplicate key facts on insert.
    ///
    /// When a candidate fact's nearest neighbour in `zeph_key_facts` has a score ≥ this value,
    /// the fact is not stored.  Default: `0.95`.
    #[must_use]
    pub fn with_key_facts_dedup_threshold(mut self, threshold: f32) -> Self {
        self.key_facts_dedup_threshold = threshold;
        self
    }

    /// Configure retrieval depth and search prompt template (MM-F1/F2, #3340).
    ///
    /// `depth` is the number of ANN candidates fetched from the vector store before keyword merge
    /// and MMR re-ranking.  `0` = legacy behavior (`recall_limit * 2`).  `≥ 1` = exact count.
    ///
    /// `search_prompt_template` is applied to the raw user query before embedding.  Supports a
    /// single `{query}` placeholder.  Empty string = identity.
    #[must_use]
    pub fn with_retrieval_options(
        mut self,
        depth: u32,
        search_prompt_template: impl Into<String>,
    ) -> Self {
        self.retrieval_depth = depth;
        self.search_prompt_template = search_prompt_template.into();
        self
    }

    /// Effective ANN candidate count for a given requested final limit (MM-F1, #3340).
    ///
    /// - `retrieval_depth == 0`: legacy behavior, returns `limit * 2`.
    /// - `retrieval_depth >= 1`: returns the configured depth directly.
    ///
    /// When `retrieval_depth < limit`, a one-shot WARN fires because the ANN pool cannot
    /// saturate the requested top-k.  When `limit <= retrieval_depth < limit * 2`, an INFO
    /// fires per call noting the smaller-than-legacy pool.
    pub(crate) fn effective_depth(&self, limit: usize) -> usize {
        use std::sync::atomic::Ordering;

        let depth = self.retrieval_depth as usize;
        if depth == 0 {
            return limit.saturating_mul(2);
        }
        if depth < limit {
            if !self.depth_below_limit_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    retrieval_depth = depth,
                    recall_limit = limit,
                    "memory.retrieval.depth < recall_limit; ANN pool cannot saturate top-k — consider raising depth"
                );
            }
        } else if depth < limit.saturating_mul(2) {
            tracing::info!(
                retrieval_depth = depth,
                recall_limit = limit,
                legacy_default = limit.saturating_mul(2),
                "memory.retrieval.depth is below legacy limit*2; ANN pool will be smaller than pre-#3340"
            );
        } else {
            tracing::debug!(
                retrieval_depth = depth,
                recall_limit = limit,
                "recall: using configured ANN depth"
            );
        }
        depth
    }

    /// Apply the configured search prompt template to a raw query (MM-F2, #3340).
    ///
    /// Returns `query` as-is when the template is empty or has no `{query}` placeholder.
    /// A one-shot WARN fires when the template is non-empty but missing the placeholder.
    pub(crate) fn apply_search_prompt(&self, query: &str) -> String {
        use std::sync::atomic::Ordering;

        let template = &self.search_prompt_template;
        if template.is_empty() {
            return query.to_owned();
        }
        if !template.contains("{query}") {
            if !self
                .missing_placeholder_warned
                .swap(true, Ordering::Relaxed)
            {
                tracing::warn!(
                    template = template.as_str(),
                    "memory.retrieval.search_prompt_template has no {{query}} placeholder — \
                     using raw query as-is"
                );
            }
            return query.to_owned();
        }
        template.replace("{query}", query)
    }

    /// Attach a dedicated embedding provider for write-path and backfill operations.
    ///
    /// When set, all batch embedding calls (backfill, `remember`) route through this provider
    /// instead of the main `provider`. This prevents `embed_backfill` from saturating the main
    /// provider and causing guardrail timeouts due to rate-limit contention or Ollama model-lock.
    #[must_use]
    pub fn with_embed_provider(mut self, embed_provider: AnyProvider) -> Self {
        self.embed_provider = Some(embed_provider);
        self
    }

    /// Returns the provider to use for embedding calls.
    ///
    /// Returns the dedicated embed provider when configured, falling back to the main provider.
    pub(crate) fn effective_embed_provider(&self) -> &AnyProvider {
        self.embed_provider.as_ref().unwrap_or(&self.provider)
    }

    /// Construct a `SemanticMemory` from pre-built parts.
    ///
    /// Intended for tests that need full control over the backing stores.
    #[must_use]
    pub fn from_parts(
        sqlite: SqliteStore,
        qdrant: Option<Arc<EmbeddingStore>>,
        provider: AnyProvider,
        embedding_model: impl Into<String>,
        vector_weight: f64,
        keyword_weight: f64,
        token_counter: Arc<TokenCounter>,
    ) -> Self {
        Self {
            sqlite,
            qdrant,
            provider,
            embed_provider: None,
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            importance_enabled: false,
            importance_weight: 0.15,
            tier_boost_semantic: 1.3,
            token_counter,
            graph_store: None,
            experience: None,
            reasoning: None,
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            last_qdrant_warn: Arc::new(AtomicU64::new(0)),
            admission_control: None,
            quality_gate: None,
            key_facts_dedup_threshold: 0.95,
            embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            retrieval_depth: 0,
            search_prompt_template: String::new(),
            depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create a `SemanticMemory` using the `SQLite`-embedded vector backend.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_sqlite_backend(
        sqlite_path: &str,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
    ) -> Result<Self, MemoryError> {
        Self::with_sqlite_backend_and_pool_size(
            sqlite_path,
            provider,
            embedding_model,
            vector_weight,
            keyword_weight,
            5,
        )
        .await
    }

    /// Create a `SemanticMemory` using the `SQLite`-embedded vector backend with configurable pool size.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn with_sqlite_backend_and_pool_size(
        sqlite_path: &str,
        provider: AnyProvider,
        embedding_model: &str,
        vector_weight: f64,
        keyword_weight: f64,
        pool_size: u32,
    ) -> Result<Self, MemoryError> {
        let sqlite = SqliteStore::with_pool_size(sqlite_path, pool_size).await?;
        let pool = sqlite.pool().clone();
        let store = EmbeddingStore::new_sqlite(pool);

        Ok(Self {
            sqlite,
            qdrant: Some(Arc::new(store)),
            provider,
            embed_provider: None,
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            importance_enabled: false,
            importance_weight: 0.15,
            tier_boost_semantic: 1.3,
            token_counter: Arc::new(TokenCounter::new()),
            graph_store: None,
            experience: None,
            reasoning: None,
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            last_qdrant_warn: Arc::new(AtomicU64::new(0)),
            admission_control: None,
            quality_gate: None,
            key_facts_dedup_threshold: 0.95,
            embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            retrieval_depth: 0,
            search_prompt_template: String::new(),
            depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Access the underlying `SqliteStore` for operations that don't involve semantics.
    #[must_use]
    pub fn sqlite(&self) -> &SqliteStore {
        &self.sqlite
    }

    /// Check if the vector store backend is reachable.
    ///
    /// Performs a real health check (Qdrant gRPC ping or `SQLite` query)
    /// instead of just checking whether the client was created.
    pub async fn is_vector_store_connected(&self) -> bool {
        match self.qdrant.as_ref() {
            Some(store) => store.health_check().await,
            None => false,
        }
    }

    /// Check if a vector store client is configured (may not be connected).
    #[must_use]
    pub fn has_vector_store(&self) -> bool {
        self.qdrant.is_some()
    }

    /// Return a reference to the embedding store, if configured.
    #[must_use]
    pub fn embedding_store(&self) -> Option<&Arc<EmbeddingStore>> {
        self.qdrant.as_ref()
    }

    /// Return a reference to the underlying LLM provider (used for RPE embedding).
    pub fn provider(&self) -> &AnyProvider {
        &self.provider
    }

    /// Count messages in a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_count(
        &self,
        conversation_id: crate::types::ConversationId,
    ) -> Result<i64, MemoryError> {
        self.sqlite.count_messages(conversation_id).await
    }

    /// Count messages not yet covered by any summary.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn unsummarized_message_count(
        &self,
        conversation_id: crate::types::ConversationId,
    ) -> Result<i64, MemoryError> {
        let after_id = self
            .sqlite
            .latest_summary_last_message_id(conversation_id)
            .await?
            .unwrap_or(crate::types::MessageId(0));
        self.sqlite
            .count_messages_after(conversation_id, after_id)
            .await
    }

    /// Load recent episodic messages for the promotion-scan window.
    ///
    /// Returns up to `max_items` of the most recent non-deleted messages across all
    /// conversations, with their `conversation_id` for session-count heuristics.
    ///
    /// # Embedding note
    ///
    /// `embedding` is returned as `None` in this MVP implementation. A future pass
    /// will join with the Qdrant payload to populate embeddings inline.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the underlying `SQLite` query fails.
    // TODO(review): populate embeddings by fetching from Qdrant when available.
    pub async fn load_promotion_window(
        &self,
        max_items: usize,
    ) -> Result<Vec<crate::compression::promotion::PromotionInput>, MemoryError> {
        use zeph_db::sql;

        let limit = i64::try_from(max_items).unwrap_or(i64::MAX);
        let rows: Vec<(
            crate::types::MessageId,
            crate::types::ConversationId,
            String,
        )> = zeph_db::query_as(sql!(
            "SELECT id, conversation_id, content \
                 FROM messages \
                 WHERE deleted_at IS NULL \
                 ORDER BY id DESC \
                 LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(self.sqlite.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|(message_id, conversation_id, content)| {
                crate::compression::promotion::PromotionInput {
                    message_id,
                    conversation_id,
                    content,
                    // Embeddings not wired yet — scan will skip rows with None.
                    embedding: None,
                }
            })
            .collect())
    }

    /// Retrieve top-k reasoning strategies by embedding similarity to `query`.
    ///
    /// Returns an empty vec when reasoning memory is not attached, Qdrant is unavailable,
    /// or the provider does not support embeddings.
    ///
    /// This method is **pure** — it does not increment `use_count` or `last_used_at`.
    /// Call [`crate::reasoning::ReasoningMemory::mark_used`] with the ids of strategies
    /// actually injected into the prompt (after budget truncation).
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or the vector search fails.
    pub async fn retrieve_reasoning_strategies(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::reasoning::ReasoningStrategy>, MemoryError> {
        let Some(reasoning) = &self.reasoning else {
            return Ok(Vec::new());
        };
        if !self.effective_embed_provider().supports_embeddings() {
            return Ok(Vec::new());
        }
        let embedding = self.effective_embed_provider().embed(query).await?;
        reasoning
            .retrieve_by_embedding(&embedding, limit as u64)
            .await
    }
}
