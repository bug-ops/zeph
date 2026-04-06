// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
use std::sync::atomic::AtomicU64;

use zeph_llm::any::AnyProvider;

use crate::admission::AdmissionControl;
use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::token_counter::TokenCounter;

pub(crate) const SESSION_SUMMARIES_COLLECTION: &str = "zeph_session_summaries";
pub(crate) const KEY_FACTS_COLLECTION: &str = "zeph_key_facts";
pub(crate) const CORRECTIONS_COLLECTION: &str = "zeph_corrections";

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
    pub(crate) community_detection_failures: Arc<AtomicU64>,
    pub(crate) graph_extraction_count: Arc<AtomicU64>,
    pub(crate) graph_extraction_failures: Arc<AtomicU64>,
    /// A-MAC admission control gate. When `Some`, each `remember()` call is evaluated.
    pub(crate) admission_control: Option<Arc<AdmissionControl>>,
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
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            admission_control: None,
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
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            admission_control: None,
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
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            admission_control: None,
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
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            admission_control: None,
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
}
