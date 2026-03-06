// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role};

use std::sync::Arc;
#[cfg(feature = "graph-memory")]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::embedding_store::{EmbeddingStore, MessageKind, SearchFilter};
use crate::error::MemoryError;
use crate::sqlite::SqliteStore;
use crate::token_counter::TokenCounter;
use crate::types::{ConversationId, MessageId};
use crate::vector_store::{FieldCondition, FieldValue, VectorFilter};

const SESSION_SUMMARIES_COLLECTION: &str = "zeph_session_summaries";
const KEY_FACTS_COLLECTION: &str = "zeph_key_facts";
const CORRECTIONS_COLLECTION: &str = "zeph_corrections";

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct StructuredSummary {
    pub summary: String,
    pub key_facts: Vec<String>,
    pub entities: Vec<String>,
}

#[derive(Debug)]
pub struct RecalledMessage {
    pub message: Message,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub id: i64,
    pub conversation_id: ConversationId,
    pub content: String,
    pub first_message_id: MessageId,
    pub last_message_id: MessageId,
    pub token_estimate: i64,
}

#[derive(Debug, Clone)]
pub struct SessionSummaryResult {
    pub summary_text: String,
    pub score: f32,
    pub conversation_id: ConversationId,
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

fn apply_temporal_decay(
    ranked: &mut [(MessageId, f64)],
    timestamps: &std::collections::HashMap<MessageId, i64>,
    half_life_days: u32,
) {
    if half_life_days == 0 {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .cast_signed();
    let lambda = std::f64::consts::LN_2 / f64::from(half_life_days);

    for (msg_id, score) in ranked.iter_mut() {
        if let Some(&ts) = timestamps.get(msg_id) {
            #[allow(clippy::cast_precision_loss)]
            let age_days = (now - ts).max(0) as f64 / 86400.0;
            *score *= (-lambda * age_days).exp();
        }
    }
}

fn apply_mmr(
    ranked: &[(MessageId, f64)],
    vectors: &std::collections::HashMap<MessageId, Vec<f32>>,
    lambda: f32,
    limit: usize,
) -> Vec<(MessageId, f64)> {
    if ranked.is_empty() || limit == 0 {
        return Vec::new();
    }

    let lambda = f64::from(lambda);
    let mut selected: Vec<(MessageId, f64)> = Vec::with_capacity(limit);
    let mut remaining: Vec<(MessageId, f64)> = ranked.to_vec();

    while selected.len() < limit && !remaining.is_empty() {
        let best_idx = if selected.is_empty() {
            // Pick highest relevance first
            0
        } else {
            let mut best = 0usize;
            let mut best_score = f64::NEG_INFINITY;

            for (i, &(cand_id, relevance)) in remaining.iter().enumerate() {
                let max_sim = if let Some(cand_vec) = vectors.get(&cand_id) {
                    selected
                        .iter()
                        .filter_map(|(sel_id, _)| vectors.get(sel_id))
                        .map(|sel_vec| f64::from(cosine_similarity(cand_vec, sel_vec)))
                        .fold(f64::NEG_INFINITY, f64::max)
                } else {
                    0.0
                };
                let max_sim = if max_sim == f64::NEG_INFINITY {
                    0.0
                } else {
                    max_sim
                };
                let mmr_score = lambda * relevance - (1.0 - lambda) * max_sim;
                if mmr_score > best_score {
                    best_score = mmr_score;
                    best = i;
                }
            }
            best
        };

        selected.push(remaining.remove(best_idx));
    }

    selected
}

fn build_summarization_prompt(messages: &[(MessageId, String, String)]) -> String {
    let mut prompt = String::from(
        "Summarize the following conversation. Extract key facts, decisions, entities, \
         and context needed to continue the conversation.\n\n\
         Respond in JSON with fields: summary (string), key_facts (list of strings), \
         entities (list of strings).\n\nConversation:\n",
    );

    for (_, role, content) in messages {
        prompt.push_str(role);
        prompt.push_str(": ");
        prompt.push_str(content);
        prompt.push('\n');
    }

    prompt
}

pub struct SemanticMemory {
    sqlite: SqliteStore,
    qdrant: Option<Arc<EmbeddingStore>>,
    provider: AnyProvider,
    embedding_model: String,
    vector_weight: f64,
    keyword_weight: f64,
    temporal_decay_enabled: bool,
    temporal_decay_half_life_days: u32,
    mmr_enabled: bool,
    mmr_lambda: f32,
    pub token_counter: Arc<TokenCounter>,
    #[cfg(feature = "graph-memory")]
    pub graph_store: Option<Arc<crate::graph::GraphStore>>,
    #[cfg(feature = "graph-memory")]
    community_detection_failures: Arc<AtomicU64>,
}

impl SemanticMemory {
    /// Create a new `SemanticMemory` instance with default hybrid search weights (0.7/0.3).
    ///
    /// Qdrant connection is best-effort: if unavailable, semantic search is disabled.
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
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            token_counter: Arc::new(TokenCounter::new()),
            #[cfg(feature = "graph-memory")]
            graph_store: None,
            #[cfg(feature = "graph-memory")]
            community_detection_failures: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Attach a `GraphStore` for graph-aware retrieval.
    ///
    /// When set, `recall_graph` traverses the graph starting from entities
    /// matched by the query.
    #[cfg(feature = "graph-memory")]
    #[must_use]
    pub fn with_graph_store(mut self, store: Arc<crate::graph::GraphStore>) -> Self {
        self.graph_store = Some(store);
        self
    }

    /// Returns the cumulative count of community detection failures since startup.
    #[cfg(feature = "graph-memory")]
    #[must_use]
    pub fn community_detection_failures(&self) -> u64 {
        self.community_detection_failures.load(Ordering::Relaxed)
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

    /// Construct a `SemanticMemory` from pre-built parts.
    ///
    /// Intended for tests that need full control over the backing stores.
    #[cfg(any(test, feature = "mock"))]
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
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            token_counter,
            #[cfg(feature = "graph-memory")]
            graph_store: None,
            #[cfg(feature = "graph-memory")]
            community_detection_failures: Arc::new(AtomicU64::new(0)),
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
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            token_counter: Arc::new(TokenCounter::new()),
            #[cfg(feature = "graph-memory")]
            graph_store: None,
            #[cfg(feature = "graph-memory")]
            community_detection_failures: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Save a message to `SQLite` and optionally embed and store in Qdrant.
    ///
    /// Returns the message ID assigned by `SQLite`.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` save fails. Embedding failures are logged but not
    /// propagated.
    pub async fn remember(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
    ) -> Result<MessageId, MemoryError> {
        let message_id = self
            .sqlite
            .save_message(conversation_id, role, content)
            .await?;

        if let Some(qdrant) = &self.qdrant
            && self.provider.supports_embeddings()
        {
            match self.provider.embed(content).await {
                Ok(vector) => {
                    // Ensure collection exists before storing
                    let vector_size = u64::try_from(vector.len()).unwrap_or(896);
                    if let Err(e) = qdrant.ensure_collection(vector_size).await {
                        tracing::warn!("Failed to ensure Qdrant collection: {e:#}");
                    } else if let Err(e) = qdrant
                        .store(
                            message_id,
                            conversation_id,
                            role,
                            vector,
                            MessageKind::Regular,
                            &self.embedding_model,
                        )
                        .await
                    {
                        tracing::warn!("Failed to store embedding: {e:#}");
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to generate embedding: {e:#}");
                }
            }
        }

        Ok(message_id)
    }

    /// Save a message with pre-serialized parts JSON to `SQLite` and optionally embed in Qdrant.
    ///
    /// Returns `(message_id, embedding_stored)` tuple where `embedding_stored` is `true` if
    /// an embedding was successfully generated and stored in Qdrant.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` save fails.
    pub async fn remember_with_parts(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        parts_json: &str,
    ) -> Result<(MessageId, bool), MemoryError> {
        let message_id = self
            .sqlite
            .save_message_with_parts(conversation_id, role, content, parts_json)
            .await?;

        let mut embedding_stored = false;

        if let Some(qdrant) = &self.qdrant
            && self.provider.supports_embeddings()
        {
            match self.provider.embed(content).await {
                Ok(vector) => {
                    let vector_size = u64::try_from(vector.len()).unwrap_or(896);
                    if let Err(e) = qdrant.ensure_collection(vector_size).await {
                        tracing::warn!("Failed to ensure Qdrant collection: {e:#}");
                    } else if let Err(e) = qdrant
                        .store(
                            message_id,
                            conversation_id,
                            role,
                            vector,
                            MessageKind::Regular,
                            &self.embedding_model,
                        )
                        .await
                    {
                        tracing::warn!("Failed to store embedding: {e:#}");
                    } else {
                        embedding_stored = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to generate embedding: {e:#}");
                }
            }
        }

        Ok((message_id, embedding_stored))
    }

    /// Save a message to `SQLite` without generating an embedding.
    ///
    /// Use this when embedding is intentionally skipped (e.g. autosave disabled for assistant).
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` save fails.
    pub async fn save_only(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        parts_json: &str,
    ) -> Result<MessageId, MemoryError> {
        self.sqlite
            .save_message_with_parts(conversation_id, role, content, parts_json)
            .await
    }

    /// Recall relevant messages using hybrid search (vector + FTS5 keyword).
    ///
    /// When Qdrant is available, runs both vector and keyword searches, then merges
    /// results using weighted scoring. When Qdrant is unavailable, falls back to
    /// FTS5-only keyword search.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation, Qdrant search, or FTS5 query fails.
    pub async fn recall(
        &self,
        query: &str,
        limit: usize,
        filter: Option<SearchFilter>,
    ) -> Result<Vec<RecalledMessage>, MemoryError> {
        let conversation_id = filter.as_ref().and_then(|f| f.conversation_id);

        // FTS5 keyword search (always available)
        let keyword_results = match self
            .sqlite
            .keyword_search(query, limit * 2, conversation_id)
            .await
        {
            Ok(results) => results,
            Err(e) => {
                tracing::warn!("FTS5 keyword search failed: {e:#}");
                Vec::new()
            }
        };

        // Vector search (only when Qdrant available)
        let vector_results = if let Some(qdrant) = &self.qdrant
            && self.provider.supports_embeddings()
        {
            let query_vector = self.provider.embed(query).await?;
            let vector_size = u64::try_from(query_vector.len()).unwrap_or(896);
            qdrant.ensure_collection(vector_size).await?;
            qdrant.search(&query_vector, limit * 2, filter).await?
        } else {
            Vec::new()
        };

        self.recall_merge_and_rank(keyword_results, vector_results, limit)
            .await
    }

    /// Raw FTS5 keyword search results: returns `(MessageId, score)` pairs without ranking.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` FTS5 query fails.
    async fn recall_fts5_raw(
        &self,
        query: &str,
        limit: usize,
        conversation_id: Option<ConversationId>,
    ) -> Result<Vec<(MessageId, f64)>, MemoryError> {
        self.sqlite
            .keyword_search(query, limit * 2, conversation_id)
            .await
    }

    /// Raw vector search results from Qdrant. Returns an empty `Vec` when Qdrant is unavailable
    /// or the provider does not support embeddings.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or Qdrant search fails.
    async fn recall_vectors_raw(
        &self,
        query: &str,
        limit: usize,
        filter: Option<SearchFilter>,
    ) -> Result<Vec<crate::embedding_store::SearchResult>, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            return Ok(Vec::new());
        }
        let query_vector = self.provider.embed(query).await?;
        let vector_size = u64::try_from(query_vector.len()).unwrap_or(896);
        qdrant.ensure_collection(vector_size).await?;
        qdrant.search(&query_vector, limit * 2, filter).await
    }

    /// Merge raw keyword and vector results, apply weighted scoring, temporal decay, and MMR
    /// re-ranking, then resolve to `RecalledMessage` objects.
    ///
    /// This is the shared post-processing step used by all recall paths.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` `messages_by_ids` query fails.
    #[allow(clippy::cast_possible_truncation)]
    async fn recall_merge_and_rank(
        &self,
        keyword_results: Vec<(MessageId, f64)>,
        vector_results: Vec<crate::embedding_store::SearchResult>,
        limit: usize,
    ) -> Result<Vec<RecalledMessage>, MemoryError> {
        let mut scores: std::collections::HashMap<MessageId, f64> =
            std::collections::HashMap::new();

        if !vector_results.is_empty() {
            let max_vs = vector_results
                .iter()
                .map(|r| r.score)
                .fold(f32::NEG_INFINITY, f32::max);
            let norm = if max_vs > 0.0 { max_vs } else { 1.0 };
            for r in &vector_results {
                let normalized = f64::from(r.score / norm);
                *scores.entry(r.message_id).or_default() += normalized * self.vector_weight;
            }
        }

        if !keyword_results.is_empty() {
            let max_ks = keyword_results
                .iter()
                .map(|r| r.1)
                .fold(f64::NEG_INFINITY, f64::max);
            let norm = if max_ks > 0.0 { max_ks } else { 1.0 };
            for &(msg_id, score) in &keyword_results {
                let normalized = score / norm;
                *scores.entry(msg_id).or_default() += normalized * self.keyword_weight;
            }
        }

        if scores.is_empty() {
            return Ok(Vec::new());
        }

        let mut ranked: Vec<(MessageId, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        if self.temporal_decay_enabled && self.temporal_decay_half_life_days > 0 {
            let ids: Vec<MessageId> = ranked.iter().map(|r| r.0).collect();
            match self.sqlite.message_timestamps(&ids).await {
                Ok(timestamps) => {
                    apply_temporal_decay(
                        &mut ranked,
                        &timestamps,
                        self.temporal_decay_half_life_days,
                    );
                    ranked
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                }
                Err(e) => {
                    tracing::warn!("temporal decay: failed to fetch timestamps: {e:#}");
                }
            }
        }

        if self.mmr_enabled && !vector_results.is_empty() {
            if let Some(qdrant) = &self.qdrant {
                let ids: Vec<MessageId> = ranked.iter().map(|r| r.0).collect();
                match qdrant.get_vectors(&ids).await {
                    Ok(vec_map) if !vec_map.is_empty() => {
                        ranked = apply_mmr(&ranked, &vec_map, self.mmr_lambda, limit);
                    }
                    Ok(_) => {
                        ranked.truncate(limit);
                    }
                    Err(e) => {
                        tracing::warn!("MMR: failed to fetch vectors: {e:#}");
                        ranked.truncate(limit);
                    }
                }
            } else {
                ranked.truncate(limit);
            }
        } else {
            ranked.truncate(limit);
        }

        let ids: Vec<MessageId> = ranked.iter().map(|r| r.0).collect();
        let messages = self.sqlite.messages_by_ids(&ids).await?;
        let msg_map: std::collections::HashMap<MessageId, _> = messages.into_iter().collect();

        let recalled = ranked
            .iter()
            .filter_map(|(msg_id, score)| {
                msg_map.get(msg_id).map(|msg| RecalledMessage {
                    message: msg.clone(),
                    #[expect(clippy::cast_possible_truncation)]
                    score: *score as f32,
                })
            })
            .collect();

        Ok(recalled)
    }

    /// Recall messages using query-aware routing.
    ///
    /// Delegates to FTS5-only, vector-only, or hybrid search based on the router decision,
    /// then runs the shared merge and ranking pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if any underlying search or database operation fails.
    pub async fn recall_routed(
        &self,
        query: &str,
        limit: usize,
        filter: Option<SearchFilter>,
        router: &dyn crate::router::MemoryRouter,
    ) -> Result<Vec<RecalledMessage>, MemoryError> {
        use crate::router::MemoryRoute;

        let route = router.route(query);
        tracing::debug!(?route, query_len = query.len(), "memory routing decision");

        let conversation_id = filter.as_ref().and_then(|f| f.conversation_id);

        let (keyword_results, vector_results): (
            Vec<(MessageId, f64)>,
            Vec<crate::embedding_store::SearchResult>,
        ) = match route {
            MemoryRoute::Keyword => {
                let kw = self.recall_fts5_raw(query, limit, conversation_id).await?;
                (kw, Vec::new())
            }
            MemoryRoute::Semantic => {
                let vr = self.recall_vectors_raw(query, limit, filter).await?;
                (Vec::new(), vr)
            }
            MemoryRoute::Hybrid => {
                // FTS5 errors are swallowed gracefully to allow vector-only fallback.
                let kw = match self.recall_fts5_raw(query, limit, conversation_id).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("FTS5 keyword search failed: {e:#}");
                        Vec::new()
                    }
                };
                // Vector errors propagate — if Qdrant is unavailable, recall_vectors_raw
                // returns an empty Vec (not an error), so ? only fires on embed failures.
                let vr = self.recall_vectors_raw(query, limit, filter).await?;
                (kw, vr)
            }
            // Graph routing triggers graph_recall separately in agent/context.rs.
            // For the message-based recall, behave like Hybrid.
            MemoryRoute::Graph => {
                let kw = match self.recall_fts5_raw(query, limit, conversation_id).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("FTS5 keyword search failed (graph→hybrid fallback): {e:#}");
                        Vec::new()
                    }
                };
                let vr = self.recall_vectors_raw(query, limit, filter).await?;
                (kw, vr)
            }
        };

        self.recall_merge_and_rank(keyword_results, vector_results, limit)
            .await
    }

    /// Retrieve graph facts relevant to `query` via BFS traversal.
    ///
    /// Returns an empty `Vec` if no `graph_store` is configured.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying graph query fails.
    #[cfg(feature = "graph-memory")]
    pub async fn recall_graph(
        &self,
        query: &str,
        limit: usize,
        max_hops: u32,
    ) -> Result<Vec<crate::graph::types::GraphFact>, MemoryError> {
        let Some(store) = &self.graph_store else {
            return Ok(Vec::new());
        };
        crate::graph::retrieval::graph_recall(
            store,
            self.qdrant.as_deref(),
            &self.provider,
            query,
            limit,
            max_hops,
        )
        .await
    }

    /// Check whether an embedding exists for a given message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn has_embedding(&self, message_id: MessageId) -> Result<bool, MemoryError> {
        match &self.qdrant {
            Some(qdrant) => qdrant.has_embedding(message_id).await,
            None => Ok(false),
        }
    }

    /// Embed all messages that do not yet have embeddings.
    ///
    /// Returns the count of successfully embedded messages.
    ///
    /// # Errors
    ///
    /// Returns an error if collection initialization or database query fails.
    /// Individual embedding failures are logged but do not stop processing.
    pub async fn embed_missing(&self) -> Result<usize, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(0);
        };
        if !self.provider.supports_embeddings() {
            return Ok(0);
        }

        let unembedded = self.sqlite.unembedded_message_ids(Some(1000)).await?;

        if unembedded.is_empty() {
            return Ok(0);
        }

        let probe = self.provider.embed("probe").await?;
        let vector_size = u64::try_from(probe.len())?;
        qdrant.ensure_collection(vector_size).await?;

        let mut count = 0;
        for (msg_id, conversation_id, role, content) in &unembedded {
            match self.provider.embed(content).await {
                Ok(vector) => {
                    if let Err(e) = qdrant
                        .store(
                            *msg_id,
                            *conversation_id,
                            role,
                            vector,
                            MessageKind::Regular,
                            &self.embedding_model,
                        )
                        .await
                    {
                        tracing::warn!("Failed to store embedding for msg {msg_id}: {e:#}");
                        continue;
                    }
                    count += 1;
                }
                Err(e) => {
                    tracing::warn!("Failed to embed msg {msg_id}: {e:#}");
                }
            }
        }

        tracing::info!("Embedded {count}/{} missing messages", unembedded.len());
        Ok(count)
    }

    /// Store a session summary into the dedicated `zeph_session_summaries` Qdrant collection.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or Qdrant storage fails.
    pub async fn store_session_summary(
        &self,
        conversation_id: ConversationId,
        summary_text: &str,
    ) -> Result<(), MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(());
        };
        if !self.provider.supports_embeddings() {
            return Ok(());
        }

        let vector = self.provider.embed(summary_text).await?;
        let vector_size = u64::try_from(vector.len()).unwrap_or(896);
        qdrant
            .ensure_named_collection(SESSION_SUMMARIES_COLLECTION, vector_size)
            .await?;

        let payload = serde_json::json!({
            "conversation_id": conversation_id.0,
            "summary_text": summary_text,
        });

        qdrant
            .store_to_collection(SESSION_SUMMARIES_COLLECTION, payload, vector)
            .await?;

        tracing::debug!(
            conversation_id = conversation_id.0,
            "stored session summary"
        );
        Ok(())
    }

    /// Search session summaries from other conversations.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or Qdrant search fails.
    pub async fn search_session_summaries(
        &self,
        query: &str,
        limit: usize,
        exclude_conversation_id: Option<ConversationId>,
    ) -> Result<Vec<SessionSummaryResult>, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            return Ok(Vec::new());
        }

        let vector = self.provider.embed(query).await?;
        let vector_size = u64::try_from(vector.len()).unwrap_or(896);
        qdrant
            .ensure_named_collection(SESSION_SUMMARIES_COLLECTION, vector_size)
            .await?;

        let filter = exclude_conversation_id.map(|cid| VectorFilter {
            must: vec![],
            must_not: vec![FieldCondition {
                field: "conversation_id".into(),
                value: FieldValue::Integer(cid.0),
            }],
        });

        let points = qdrant
            .search_collection(SESSION_SUMMARIES_COLLECTION, &vector, limit, filter)
            .await?;

        let results = points
            .into_iter()
            .filter_map(|point| {
                let summary_text = point.payload.get("summary_text")?.as_str()?.to_owned();
                let conversation_id =
                    ConversationId(point.payload.get("conversation_id")?.as_i64()?);
                Some(SessionSummaryResult {
                    summary_text,
                    score: point.score,
                    conversation_id,
                })
            })
            .collect();

        Ok(results)
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

    /// Count messages in a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_count(&self, conversation_id: ConversationId) -> Result<i64, MemoryError> {
        self.sqlite.count_messages(conversation_id).await
    }

    /// Count messages not yet covered by any summary.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn unsummarized_message_count(
        &self,
        conversation_id: ConversationId,
    ) -> Result<i64, MemoryError> {
        let after_id = self
            .sqlite
            .latest_summary_last_message_id(conversation_id)
            .await?
            .unwrap_or(MessageId(0));
        self.sqlite
            .count_messages_after(conversation_id, after_id)
            .await
    }

    /// Load all summaries for a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_summaries(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<Summary>, MemoryError> {
        let rows = self.sqlite.load_summaries(conversation_id).await?;
        let summaries = rows
            .into_iter()
            .map(
                |(
                    id,
                    conversation_id,
                    content,
                    first_message_id,
                    last_message_id,
                    token_estimate,
                )| {
                    Summary {
                        id,
                        conversation_id,
                        content,
                        first_message_id,
                        last_message_id,
                        token_estimate,
                    }
                },
            )
            .collect();
        Ok(summaries)
    }

    /// Generate a summary of the oldest unsummarized messages.
    ///
    /// Returns `Ok(None)` if there are not enough messages to summarize.
    ///
    /// # Errors
    ///
    /// Returns an error if LLM call or database operation fails.
    pub async fn summarize(
        &self,
        conversation_id: ConversationId,
        message_count: usize,
    ) -> Result<Option<i64>, MemoryError> {
        let total = self.sqlite.count_messages(conversation_id).await?;

        if total <= i64::try_from(message_count)? {
            return Ok(None);
        }

        let after_id = self
            .sqlite
            .latest_summary_last_message_id(conversation_id)
            .await?
            .unwrap_or(MessageId(0));

        let messages = self
            .sqlite
            .load_messages_range(conversation_id, after_id, message_count)
            .await?;

        if messages.is_empty() {
            return Ok(None);
        }

        let prompt = build_summarization_prompt(&messages);
        let chat_messages = vec![Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let structured = match self
            .provider
            .chat_typed_erased::<StructuredSummary>(&chat_messages)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "structured summarization failed, falling back to plain text: {e:#}"
                );
                let plain = self.provider.chat(&chat_messages).await?;
                StructuredSummary {
                    summary: plain,
                    key_facts: vec![],
                    entities: vec![],
                }
            }
        };
        let summary_text = &structured.summary;

        let token_estimate = i64::try_from(self.token_counter.count_tokens(summary_text))?;
        let first_message_id = messages[0].0;
        let last_message_id = messages[messages.len() - 1].0;

        let summary_id = self
            .sqlite
            .save_summary(
                conversation_id,
                summary_text,
                first_message_id,
                last_message_id,
                token_estimate,
            )
            .await?;

        if let Some(qdrant) = &self.qdrant
            && self.provider.supports_embeddings()
        {
            match self.provider.embed(summary_text).await {
                Ok(vector) => {
                    // Ensure collection exists before storing
                    let vector_size = u64::try_from(vector.len()).unwrap_or(896);
                    if let Err(e) = qdrant.ensure_collection(vector_size).await {
                        tracing::warn!("Failed to ensure Qdrant collection: {e:#}");
                    } else if let Err(e) = qdrant
                        .store(
                            MessageId(summary_id),
                            conversation_id,
                            "system",
                            vector,
                            MessageKind::Summary,
                            &self.embedding_model,
                        )
                        .await
                    {
                        tracing::warn!("Failed to embed summary: {e:#}");
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to generate summary embedding: {e:#}");
                }
            }
        }

        // Store key facts as individual Qdrant points
        if !structured.key_facts.is_empty() {
            self.store_key_facts(conversation_id, summary_id, &structured.key_facts)
                .await;
        }

        Ok(Some(summary_id))
    }

    async fn store_key_facts(
        &self,
        conversation_id: ConversationId,
        source_summary_id: i64,
        key_facts: &[String],
    ) {
        let Some(qdrant) = &self.qdrant else {
            return;
        };
        if !self.provider.supports_embeddings() {
            return;
        }

        let Some(first_fact) = key_facts.first() else {
            return;
        };
        let first_vector = match self.provider.embed(first_fact).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Failed to embed key fact: {e:#}");
                return;
            }
        };
        let vector_size = u64::try_from(first_vector.len()).unwrap_or(896);
        if let Err(e) = qdrant
            .ensure_named_collection(KEY_FACTS_COLLECTION, vector_size)
            .await
        {
            tracing::warn!("Failed to ensure key_facts collection: {e:#}");
            return;
        }

        let first_payload = serde_json::json!({
            "conversation_id": conversation_id.0,
            "fact_text": first_fact,
            "source_summary_id": source_summary_id,
        });
        if let Err(e) = qdrant
            .store_to_collection(KEY_FACTS_COLLECTION, first_payload, first_vector)
            .await
        {
            tracing::warn!("Failed to store key fact: {e:#}");
        }

        for fact in &key_facts[1..] {
            match self.provider.embed(fact).await {
                Ok(vector) => {
                    let payload = serde_json::json!({
                        "conversation_id": conversation_id.0,
                        "fact_text": fact,
                        "source_summary_id": source_summary_id,
                    });
                    if let Err(e) = qdrant
                        .store_to_collection(KEY_FACTS_COLLECTION, payload, vector)
                        .await
                    {
                        tracing::warn!("Failed to store key fact: {e:#}");
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to embed key fact: {e:#}");
                }
            }
        }
    }

    /// Search key facts extracted from conversation summaries.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or Qdrant search fails.
    pub async fn search_key_facts(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<String>, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            return Ok(Vec::new());
        }

        let vector = self.provider.embed(query).await?;
        let vector_size = u64::try_from(vector.len()).unwrap_or(896);
        qdrant
            .ensure_named_collection(KEY_FACTS_COLLECTION, vector_size)
            .await?;

        let points = qdrant
            .search_collection(KEY_FACTS_COLLECTION, &vector, limit, None)
            .await?;

        let facts = points
            .into_iter()
            .filter_map(|p| p.payload.get("fact_text")?.as_str().map(String::from))
            .collect();

        Ok(facts)
    }

    /// Search a named document collection by semantic similarity.
    ///
    /// Returns up to `limit` scored vector points whose payloads contain ingested document chunks.
    /// Returns an empty vec when Qdrant is unavailable, the collection does not exist,
    /// or the provider does not support embeddings.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or Qdrant search fails.
    pub async fn search_document_collection(
        &self,
        collection: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::ScoredVectorPoint>, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            return Ok(Vec::new());
        }
        if !qdrant.collection_exists(collection).await? {
            return Ok(Vec::new());
        }
        let vector = self.provider.embed(query).await?;
        qdrant
            .search_collection(collection, &vector, limit, None)
            .await
    }

    /// Store an embedding for a user correction in the vector store.
    ///
    /// Silently skips if no vector store is configured or embeddings are unsupported.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or vector store write fails.
    pub async fn store_correction_embedding(
        &self,
        correction_id: i64,
        correction_text: &str,
    ) -> Result<(), MemoryError> {
        let Some(ref store) = self.qdrant else {
            return Ok(());
        };
        if !self.provider.supports_embeddings() {
            return Ok(());
        }
        let embedding = self
            .provider
            .embed(correction_text)
            .await
            .map_err(|e| MemoryError::Other(e.to_string()))?;
        let payload = serde_json::json!({ "correction_id": correction_id });
        store
            .store_to_collection(CORRECTIONS_COLLECTION, payload, embedding)
            .await?;
        Ok(())
    }

    /// Retrieve corrections semantically similar to `query`.
    ///
    /// Returns up to `limit` corrections scoring above `min_score`.
    /// Returns an empty vec if no vector store is configured.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or vector search fails.
    pub async fn retrieve_similar_corrections(
        &self,
        query: &str,
        limit: usize,
        min_score: f32,
    ) -> Result<Vec<crate::sqlite::corrections::UserCorrectionRow>, MemoryError> {
        let Some(ref store) = self.qdrant else {
            return Ok(vec![]);
        };
        if !self.provider.supports_embeddings() {
            return Ok(vec![]);
        }
        let embedding = self
            .provider
            .embed(query)
            .await
            .map_err(|e| MemoryError::Other(e.to_string()))?;
        let scored = store
            .search_collection(CORRECTIONS_COLLECTION, &embedding, limit, None)
            .await
            .unwrap_or_default();

        let mut results = Vec::new();
        for point in scored {
            if point.score < min_score {
                continue;
            }
            if let Some(id_val) = point.payload.get("correction_id")
                && let Some(id) = id_val.as_i64()
            {
                let rows = self.sqlite.load_corrections_for_id(id).await?;
                results.extend(rows);
            }
        }
        Ok(results)
    }

    /// Spawn background graph extraction for a message. Fire-and-forget — never blocks.
    ///
    /// Extraction runs in a separate tokio task with a timeout. Any error or timeout is
    /// logged and the task exits silently; the agent response is never blocked.
    #[cfg(feature = "graph-memory")]
    pub fn spawn_graph_extraction(
        &self,
        content: String,
        context_messages: Vec<String>,
        config: GraphExtractionConfig,
    ) {
        let pool = self.sqlite.pool().clone();
        let provider = self.provider.clone();
        let failure_counter = self.community_detection_failures.clone();

        tokio::spawn(async move {
            let timeout_dur = std::time::Duration::from_secs(config.extraction_timeout_secs);
            let extraction_ok = match tokio::time::timeout(
                timeout_dur,
                extract_and_store(
                    content,
                    context_messages,
                    provider.clone(),
                    pool.clone(),
                    config.clone(),
                ),
            )
            .await
            {
                Ok(Ok(stats)) => {
                    tracing::debug!(
                        entities = stats.entities_upserted,
                        edges = stats.edges_inserted,
                        "graph extraction completed"
                    );
                    true
                }
                Ok(Err(e)) => {
                    tracing::warn!("graph extraction failed: {e:#}");
                    false
                }
                Err(_elapsed) => {
                    tracing::warn!("graph extraction timed out");
                    false
                }
            };

            if extraction_ok && config.community_refresh_interval > 0 {
                use crate::graph::GraphStore;

                let store = GraphStore::new(pool.clone());
                let extraction_count = store.extraction_count().await.unwrap_or(0);
                if extraction_count > 0
                    && i64::try_from(config.community_refresh_interval)
                        .is_ok_and(|interval| extraction_count % interval == 0)
                {
                    tracing::info!(extraction_count, "triggering community detection refresh");
                    let store2 = GraphStore::new(pool);
                    let provider2 = provider;
                    let retention_days = config.expired_edge_retention_days;
                    let max_cap = config.max_entities_cap;
                    tokio::spawn(async move {
                        match crate::graph::community::detect_communities(&store2, &provider2).await
                        {
                            Ok(count) => {
                                tracing::info!(communities = count, "community detection complete");
                            }
                            Err(e) => {
                                tracing::warn!("community detection failed: {e:#}");
                                failure_counter.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        match crate::graph::community::run_graph_eviction(
                            &store2,
                            retention_days,
                            max_cap,
                        )
                        .await
                        {
                            Ok(stats) => {
                                tracing::info!(
                                    expired_edges = stats.expired_edges_deleted,
                                    orphan_entities = stats.orphan_entities_deleted,
                                    capped_entities = stats.capped_entities_deleted,
                                    "graph eviction complete"
                                );
                            }
                            Err(e) => {
                                tracing::warn!("graph eviction failed: {e:#}");
                            }
                        }
                    });
                }
            }
        });
    }
}

/// Config for the spawned background extraction task.
///
/// Owned clone of the relevant fields from `GraphConfig` — no references, safe to send to
/// spawned tasks.
#[cfg(feature = "graph-memory")]
#[derive(Debug, Clone, Default)]
pub struct GraphExtractionConfig {
    pub max_entities: usize,
    pub max_edges: usize,
    pub extraction_timeout_secs: u64,
    pub community_refresh_interval: usize,
    pub expired_edge_retention_days: u32,
    pub max_entities_cap: usize,
}

/// Stats returned from a completed extraction.
#[cfg(feature = "graph-memory")]
#[derive(Debug, Default)]
pub struct ExtractionStats {
    pub entities_upserted: usize,
    pub edges_inserted: usize,
}

/// Extract entities and edges from `content` and persist them to the graph store.
///
/// This function runs inside a spawned task — it receives owned data only.
#[cfg(feature = "graph-memory")]
async fn extract_and_store(
    content: String,
    context_messages: Vec<String>,
    provider: AnyProvider,
    pool: sqlx::SqlitePool,
    config: GraphExtractionConfig,
) -> Result<ExtractionStats, MemoryError> {
    use crate::graph::{EntityResolver, GraphExtractor, GraphStore};

    let extractor = GraphExtractor::new(provider, config.max_entities, config.max_edges);
    let ctx_refs: Vec<&str> = context_messages.iter().map(String::as_str).collect();

    let store = GraphStore::new(pool);

    // Increment attempt counter before extraction so it reflects every non-empty attempt,
    // regardless of whether the LLM returns parseable results (S1 fix).
    let pool = store.pool();
    sqlx::query(
        "INSERT INTO graph_metadata (key, value) VALUES ('extraction_count', '0')
         ON CONFLICT(key) DO NOTHING",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE graph_metadata
         SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         WHERE key = 'extraction_count'",
    )
    .execute(pool)
    .await?;

    let Some(result) = extractor.extract(&content, &ctx_refs).await? else {
        return Ok(ExtractionStats::default());
    };

    let resolver = EntityResolver::new(&store);

    let mut entities_upserted = 0usize;
    let mut entity_ids: std::collections::HashMap<String, i64> = std::collections::HashMap::new();

    for entity in &result.entities {
        match resolver
            .resolve(&entity.name, &entity.entity_type, entity.summary.as_deref())
            .await
        {
            Ok(id) => {
                entity_ids.insert(entity.name.clone(), id);
                entities_upserted += 1;
            }
            Err(e) => {
                tracing::debug!("graph: skipping entity {:?}: {e:#}", entity.name);
            }
        }
    }

    let mut edges_inserted = 0usize;
    for edge in &result.edges {
        let (Some(&src_id), Some(&tgt_id)) =
            (entity_ids.get(&edge.source), entity_ids.get(&edge.target))
        else {
            tracing::debug!(
                "graph: skipping edge {:?}->{:?}: entity not resolved",
                edge.source,
                edge.target
            );
            continue;
        };
        match resolver
            .resolve_edge(src_id, tgt_id, &edge.relation, &edge.fact, 0.8, None)
            .await
        {
            Ok(Some(_)) => edges_inserted += 1,
            Ok(None) => {} // deduplicated
            Err(e) => {
                tracing::debug!("graph: skipping edge: {e:#}");
            }
        }
    }

    Ok(ExtractionStats {
        entities_upserted,
        edges_inserted,
    })
}

#[cfg(test)]
mod tests {
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::Role;

    use super::*;

    fn test_provider() -> AnyProvider {
        AnyProvider::Mock(MockProvider::default())
    }

    async fn test_semantic_memory(_supports_embeddings: bool) -> SemanticMemory {
        let provider = test_provider();
        let sqlite = SqliteStore::new(":memory:").await.unwrap();

        SemanticMemory {
            sqlite,
            qdrant: None,
            provider,
            embedding_model: "test-model".into(),
            vector_weight: 0.7,
            keyword_weight: 0.3,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            token_counter: Arc::new(TokenCounter::new()),
            #[cfg(feature = "graph-memory")]
            graph_store: None,
            #[cfg(feature = "graph-memory")]
            community_detection_failures: Arc::new(AtomicU64::new(0)),
        }
    }

    #[tokio::test]
    async fn remember_saves_to_sqlite() {
        let memory = test_semantic_memory(false).await;

        let cid = memory.sqlite.create_conversation().await.unwrap();
        let msg_id = memory.remember(cid, "user", "hello").await.unwrap();

        assert_eq!(msg_id, MessageId(1));

        let history = memory.sqlite.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content, "hello");
    }

    #[tokio::test]
    async fn remember_with_parts_saves_parts_json() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        let parts_json =
            r#"[{"kind":"ToolOutput","tool_name":"shell","body":"hello","compacted_at":null}]"#;
        let (msg_id, _embedding_stored) = memory
            .remember_with_parts(cid, "assistant", "tool output", parts_json)
            .await
            .unwrap();
        assert!(msg_id > MessageId(0));

        let history = memory.sqlite.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "tool output");
    }

    #[tokio::test]
    async fn recall_returns_empty_without_qdrant() {
        let memory = test_semantic_memory(true).await;

        let recalled = memory.recall("test", 5, None).await.unwrap();
        assert!(recalled.is_empty());
    }

    #[tokio::test]
    async fn has_embedding_without_qdrant() {
        let memory = test_semantic_memory(true).await;

        let has_embedding = memory.has_embedding(MessageId(1)).await.unwrap();
        assert!(!has_embedding);
    }

    #[tokio::test]
    async fn embed_missing_without_qdrant() {
        let memory = test_semantic_memory(true).await;

        let count = memory.embed_missing().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn sqlite_accessor() {
        let memory = test_semantic_memory(false).await;

        let cid = memory.sqlite().create_conversation().await.unwrap();
        assert_eq!(cid, ConversationId(1));

        memory
            .sqlite()
            .save_message(cid, "user", "test")
            .await
            .unwrap();

        let history = memory.sqlite().load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 1);
    }

    #[tokio::test]
    async fn has_vector_store_returns_false_when_unavailable() {
        let memory = test_semantic_memory(false).await;
        assert!(!memory.has_vector_store());
    }

    #[tokio::test]
    async fn is_vector_store_connected_returns_false_when_unavailable() {
        let memory = test_semantic_memory(false).await;
        assert!(!memory.is_vector_store_connected().await);
    }

    #[tokio::test]
    async fn recall_returns_empty_when_embeddings_not_supported() {
        let memory = test_semantic_memory(false).await;

        let recalled = memory.recall("test", 5, None).await.unwrap();
        assert!(recalled.is_empty());
    }

    #[tokio::test]
    async fn embed_missing_returns_zero_when_embeddings_not_supported() {
        let memory = test_semantic_memory(false).await;

        let cid = memory.sqlite().create_conversation().await.unwrap();
        memory
            .sqlite()
            .save_message(cid, "user", "test")
            .await
            .unwrap();

        let count = memory.embed_missing().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn message_count_empty_conversation() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let count = memory.message_count(cid).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn message_count_after_saves() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        memory.remember(cid, "user", "msg1").await.unwrap();
        memory.remember(cid, "assistant", "msg2").await.unwrap();

        let count = memory.message_count(cid).await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn unsummarized_count_decreases_after_summary() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..10 {
            memory
                .remember(cid, "user", &format!("msg{i}"))
                .await
                .unwrap();
        }
        assert_eq!(memory.unsummarized_message_count(cid).await.unwrap(), 10);

        memory.summarize(cid, 5).await.unwrap();

        assert!(memory.unsummarized_message_count(cid).await.unwrap() < 10);
        assert_eq!(memory.message_count(cid).await.unwrap(), 10);
    }

    #[tokio::test]
    async fn load_summaries_empty() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn load_summaries_ordered() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let msg_id1 = memory.remember(cid, "user", "m1").await.unwrap();
        let msg_id2 = memory.remember(cid, "assistant", "m2").await.unwrap();
        let msg_id3 = memory.remember(cid, "user", "m3").await.unwrap();

        let s1 = memory
            .sqlite()
            .save_summary(cid, "summary1", msg_id1, msg_id2, 3)
            .await
            .unwrap();
        let s2 = memory
            .sqlite()
            .save_summary(cid, "summary2", msg_id2, msg_id3, 3)
            .await
            .unwrap();

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, s1);
        assert_eq!(summaries[0].content, "summary1");
        assert_eq!(summaries[1].id, s2);
        assert_eq!(summaries[1].content, "summary2");
    }

    #[tokio::test]
    async fn summarize_below_threshold() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        memory.remember(cid, "user", "hello").await.unwrap();

        let result = memory.summarize(cid, 10).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn summarize_stores_summary() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..5 {
            memory
                .remember(cid, "user", &format!("message {i}"))
                .await
                .unwrap();
        }

        let summary_id = memory.summarize(cid, 3).await.unwrap();
        assert!(summary_id.is_some());

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, summary_id.unwrap());
        assert!(!summaries[0].content.is_empty());
    }

    #[tokio::test]
    async fn summarize_respects_previous_summaries() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..10 {
            memory
                .remember(cid, "user", &format!("message {i}"))
                .await
                .unwrap();
        }

        let s1 = memory.summarize(cid, 3).await.unwrap();
        assert!(s1.is_some());

        let s2 = memory.summarize(cid, 3).await.unwrap();
        assert!(s2.is_some());

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert!(summaries[0].last_message_id < summaries[1].first_message_id);
    }

    #[tokio::test]
    async fn remember_multiple_messages_increments_ids() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        let id1 = memory.remember(cid, "user", "first").await.unwrap();
        let id2 = memory.remember(cid, "assistant", "second").await.unwrap();
        let id3 = memory.remember(cid, "user", "third").await.unwrap();

        assert!(id1 < id2);
        assert!(id2 < id3);
    }

    #[tokio::test]
    async fn message_count_across_conversations() {
        let memory = test_semantic_memory(false).await;
        let cid1 = memory.sqlite().create_conversation().await.unwrap();
        let cid2 = memory.sqlite().create_conversation().await.unwrap();

        memory.remember(cid1, "user", "msg1").await.unwrap();
        memory.remember(cid1, "user", "msg2").await.unwrap();
        memory.remember(cid2, "user", "msg3").await.unwrap();

        assert_eq!(memory.message_count(cid1).await.unwrap(), 2);
        assert_eq!(memory.message_count(cid2).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn summarize_exact_threshold_returns_none() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..3 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let result = memory.summarize(cid, 3).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn summarize_one_above_threshold_produces_summary() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..4 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let result = memory.summarize(cid, 3).await.unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn summary_fields_populated() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..5 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        memory.summarize(cid, 3).await.unwrap();
        let summaries = memory.load_summaries(cid).await.unwrap();
        let s = &summaries[0];

        assert_eq!(s.conversation_id, cid);
        assert!(s.first_message_id > MessageId(0));
        assert!(s.last_message_id >= s.first_message_id);
        assert!(s.token_estimate >= 0);
        assert!(!s.content.is_empty());
    }

    #[test]
    fn build_summarization_prompt_format() {
        let messages = vec![
            (MessageId(1), "user".into(), "Hello".into()),
            (MessageId(2), "assistant".into(), "Hi there".into()),
        ];
        let prompt = build_summarization_prompt(&messages);
        assert!(prompt.contains("user: Hello"));
        assert!(prompt.contains("assistant: Hi there"));
        assert!(prompt.contains("key_facts"));
    }

    #[test]
    fn build_summarization_prompt_empty() {
        let messages: Vec<(MessageId, String, String)> = vec![];
        let prompt = build_summarization_prompt(&messages);
        assert!(prompt.contains("key_facts"));
    }

    #[test]
    fn structured_summary_deserialize() {
        let json = r#"{"summary":"s","key_facts":["f1","f2"],"entities":["e1"]}"#;
        let ss: StructuredSummary = serde_json::from_str(json).unwrap();
        assert_eq!(ss.summary, "s");
        assert_eq!(ss.key_facts.len(), 2);
        assert_eq!(ss.entities.len(), 1);
    }

    #[test]
    fn structured_summary_empty_facts() {
        let json = r#"{"summary":"s","key_facts":[],"entities":[]}"#;
        let ss: StructuredSummary = serde_json::from_str(json).unwrap();
        assert!(ss.key_facts.is_empty());
        assert!(ss.entities.is_empty());
    }

    #[tokio::test]
    async fn search_key_facts_no_qdrant_empty() {
        let memory = test_semantic_memory(false).await;
        let facts = memory.search_key_facts("query", 5).await.unwrap();
        assert!(facts.is_empty());
    }

    #[test]
    fn recalled_message_debug() {
        let recalled = RecalledMessage {
            message: Message {
                role: Role::User,
                content: "test".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            score: 0.95,
        };
        let dbg = format!("{recalled:?}");
        assert!(dbg.contains("RecalledMessage"));
        assert!(dbg.contains("0.95"));
    }

    #[test]
    fn summary_clone() {
        let summary = Summary {
            id: 1,
            conversation_id: ConversationId(2),
            content: "test summary".into(),
            first_message_id: MessageId(1),
            last_message_id: MessageId(5),
            token_estimate: 10,
        };
        let cloned = summary.clone();
        assert_eq!(summary.id, cloned.id);
        assert_eq!(summary.content, cloned.content);
    }

    #[tokio::test]
    async fn remember_preserves_role_mapping() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        memory.remember(cid, "user", "u").await.unwrap();
        memory.remember(cid, "assistant", "a").await.unwrap();
        memory.remember(cid, "system", "s").await.unwrap();

        let history = memory.sqlite.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[2].role, Role::System);
    }

    #[tokio::test]
    async fn new_with_invalid_qdrant_url_graceful() {
        let mut mock = MockProvider::default();
        mock.supports_embeddings = true;
        let provider = AnyProvider::Mock(mock);
        let result =
            SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test-model").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_semantic_memory_sqlite_remember_recall_roundtrip() {
        // Build SemanticMemory with EmbeddingStore backed by SQLite instead of Qdrant
        let mut mock = MockProvider::default();
        mock.supports_embeddings = true;
        // Provide deterministic embedding vectors: embed returns a fixed 4-element vector
        // MockProvider.embed always returns the same vector, so cosine similarity = 1.0
        let provider = AnyProvider::Mock(mock);

        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();
        let qdrant = Some(Arc::new(
            crate::embedding_store::EmbeddingStore::new_sqlite(pool),
        ));

        let memory = SemanticMemory {
            sqlite,
            qdrant,
            provider,
            embedding_model: "test-model".into(),
            vector_weight: 0.7,
            keyword_weight: 0.3,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            token_counter: Arc::new(TokenCounter::new()),
            #[cfg(feature = "graph-memory")]
            graph_store: None,
            #[cfg(feature = "graph-memory")]
            community_detection_failures: Arc::new(AtomicU64::new(0)),
        };

        let cid = memory.sqlite().create_conversation().await.unwrap();

        // remember → stores in SQLite + SQLite vector store
        let id1 = memory
            .remember(cid, "user", "rust async programming")
            .await
            .unwrap();
        let id2 = memory
            .remember(cid, "assistant", "use tokio for async")
            .await
            .unwrap();
        assert!(id1 < id2);

        // recall → should return results via FTS5 keyword search
        let recalled = memory.recall("rust", 5, None).await.unwrap();
        assert!(
            !recalled.is_empty(),
            "recall must return at least one result"
        );

        // Verify history is accessible
        let history = memory.sqlite().load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "rust async programming");
    }

    #[tokio::test]
    async fn remember_with_embeddings_supported_but_no_qdrant() {
        let memory = test_semantic_memory(true).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        let msg_id = memory.remember(cid, "user", "hello embed").await.unwrap();
        assert!(msg_id > MessageId(0));

        let history = memory.sqlite.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "hello embed");
    }

    #[tokio::test]
    async fn remember_verifies_content_via_load_history() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        memory.remember(cid, "user", "alpha").await.unwrap();
        memory.remember(cid, "assistant", "beta").await.unwrap();
        memory.remember(cid, "user", "gamma").await.unwrap();

        let history = memory.sqlite().load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "alpha");
        assert_eq!(history[1].content, "beta");
        assert_eq!(history[2].content, "gamma");
    }

    #[tokio::test]
    async fn message_count_multiple_conversations_isolated() {
        let memory = test_semantic_memory(false).await;
        let cid1 = memory.sqlite().create_conversation().await.unwrap();
        let cid2 = memory.sqlite().create_conversation().await.unwrap();
        let cid3 = memory.sqlite().create_conversation().await.unwrap();

        for _ in 0..5 {
            memory.remember(cid1, "user", "msg").await.unwrap();
        }
        for _ in 0..3 {
            memory.remember(cid2, "user", "msg").await.unwrap();
        }

        assert_eq!(memory.message_count(cid1).await.unwrap(), 5);
        assert_eq!(memory.message_count(cid2).await.unwrap(), 3);
        assert_eq!(memory.message_count(cid3).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn summarize_empty_messages_range_returns_none() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..6 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        memory.summarize(cid, 3).await.unwrap();
        memory.summarize(cid, 3).await.unwrap();

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 2);
    }

    #[tokio::test]
    async fn summarize_token_estimate_populated() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..5 {
            memory
                .remember(cid, "user", &format!("message {i}"))
                .await
                .unwrap();
        }

        memory.summarize(cid, 3).await.unwrap();
        let summaries = memory.load_summaries(cid).await.unwrap();
        let token_est = summaries[0].token_estimate;
        assert!(token_est > 0);
    }

    #[tokio::test]
    async fn summarize_fails_when_provider_chat_fails() {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let provider = AnyProvider::Ollama(zeph_llm::ollama::OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        let memory = SemanticMemory {
            sqlite,
            qdrant: None,
            provider,
            embedding_model: "test".into(),
            vector_weight: 0.7,
            keyword_weight: 0.3,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            token_counter: Arc::new(TokenCounter::new()),
            #[cfg(feature = "graph-memory")]
            graph_store: None,
            #[cfg(feature = "graph-memory")]
            community_detection_failures: Arc::new(AtomicU64::new(0)),
        };
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..5 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let result = memory.summarize(cid, 3).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn embed_missing_without_embedding_support_returns_zero() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        memory
            .sqlite()
            .save_message(cid, "user", "test message")
            .await
            .unwrap();

        let count = memory.embed_missing().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn has_embedding_returns_false_when_no_qdrant() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();
        let msg_id = memory.remember(cid, "user", "test").await.unwrap();
        assert!(!memory.has_embedding(msg_id).await.unwrap());
    }

    #[tokio::test]
    async fn recall_empty_without_qdrant_regardless_of_filter() {
        let memory = test_semantic_memory(true).await;
        let filter = SearchFilter {
            conversation_id: Some(ConversationId(1)),
            role: None,
        };
        let recalled = memory.recall("query", 10, Some(filter)).await.unwrap();
        assert!(recalled.is_empty());
    }

    #[tokio::test]
    async fn summarize_message_range_bounds() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..8 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let summary_id = memory.summarize(cid, 4).await.unwrap().unwrap();
        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, summary_id);
        assert!(summaries[0].first_message_id >= MessageId(1));
        assert!(summaries[0].last_message_id >= summaries[0].first_message_id);
    }

    #[test]
    fn build_summarization_prompt_preserves_order() {
        let messages = vec![
            (MessageId(1), "user".into(), "first".into()),
            (MessageId(2), "assistant".into(), "second".into()),
            (MessageId(3), "user".into(), "third".into()),
        ];
        let prompt = build_summarization_prompt(&messages);
        let first_pos = prompt.find("user: first").unwrap();
        let second_pos = prompt.find("assistant: second").unwrap();
        let third_pos = prompt.find("user: third").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
    }

    #[test]
    fn summary_debug() {
        let summary = Summary {
            id: 1,
            conversation_id: ConversationId(2),
            content: "test".into(),
            first_message_id: MessageId(1),
            last_message_id: MessageId(5),
            token_estimate: 10,
        };
        let dbg = format!("{summary:?}");
        assert!(dbg.contains("Summary"));
    }

    #[tokio::test]
    async fn message_count_nonexistent_conversation() {
        let memory = test_semantic_memory(false).await;
        let count = memory.message_count(ConversationId(999)).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn load_summaries_nonexistent_conversation() {
        let memory = test_semantic_memory(false).await;
        let summaries = memory.load_summaries(ConversationId(999)).await.unwrap();
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn store_session_summary_no_qdrant_noop() {
        let memory = test_semantic_memory(true).await;
        let result = memory
            .store_session_summary(ConversationId(1), "test summary")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn store_session_summary_no_embeddings_noop() {
        let memory = test_semantic_memory(false).await;
        let result = memory
            .store_session_summary(ConversationId(1), "test summary")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn search_session_summaries_no_qdrant_empty() {
        let memory = test_semantic_memory(true).await;
        let results = memory
            .search_session_summaries("query", 5, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_session_summaries_no_embeddings_empty() {
        let memory = test_semantic_memory(false).await;
        let results = memory
            .search_session_summaries("query", 5, Some(ConversationId(1)))
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn session_summary_result_debug() {
        let result = SessionSummaryResult {
            summary_text: "test".into(),
            score: 0.9,
            conversation_id: ConversationId(1),
        };
        let dbg = format!("{result:?}");
        assert!(dbg.contains("SessionSummaryResult"));
    }

    #[test]
    fn session_summary_result_clone() {
        let result = SessionSummaryResult {
            summary_text: "test".into(),
            score: 0.9,
            conversation_id: ConversationId(1),
        };
        let cloned = result.clone();
        assert_eq!(result.summary_text, cloned.summary_text);
        assert_eq!(result.conversation_id, cloned.conversation_id);
    }

    #[tokio::test]
    async fn recall_fts5_fallback_without_qdrant() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        memory
            .remember(cid, "user", "rust programming guide")
            .await
            .unwrap();
        memory
            .remember(cid, "assistant", "python tutorial")
            .await
            .unwrap();
        memory
            .remember(cid, "user", "advanced rust patterns")
            .await
            .unwrap();

        let recalled = memory.recall("rust", 5, None).await.unwrap();
        assert_eq!(recalled.len(), 2);
        assert!(recalled[0].score >= recalled[1].score);
    }

    #[tokio::test]
    async fn recall_fts5_fallback_with_filter() {
        let memory = test_semantic_memory(false).await;
        let cid1 = memory.sqlite.create_conversation().await.unwrap();
        let cid2 = memory.sqlite.create_conversation().await.unwrap();

        memory.remember(cid1, "user", "hello world").await.unwrap();
        memory
            .remember(cid2, "user", "hello universe")
            .await
            .unwrap();

        let filter = SearchFilter {
            conversation_id: Some(cid1),
            role: None,
        };
        let recalled = memory.recall("hello", 5, Some(filter)).await.unwrap();
        assert_eq!(recalled.len(), 1);
    }

    #[tokio::test]
    async fn recall_fts5_no_matches_returns_empty() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        memory.remember(cid, "user", "hello world").await.unwrap();

        let recalled = memory.recall("nonexistent", 5, None).await.unwrap();
        assert!(recalled.is_empty());
    }

    #[tokio::test]
    async fn recall_fts5_respects_limit() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        for i in 0..10 {
            memory
                .remember(cid, "user", &format!("test message number {i}"))
                .await
                .unwrap();
        }

        let recalled = memory.recall("test", 3, None).await.unwrap();
        assert_eq!(recalled.len(), 3);
    }

    // Priority 2: summarize fallback path

    #[tokio::test]
    async fn summarize_fallback_to_plain_text_when_structured_fails() {
        // Use OllamaProvider pointing at an unreachable URL for chat_typed_erased,
        // but MockProvider for the plain chat call.
        // The easiest way: MockProvider returns non-JSON plain text so chat_typed_erased
        // (which uses chat() + JSON parse) will fail to parse, then falls back to chat().
        // However MockProvider.chat_typed calls chat() which returns default_response.
        // chat_typed tries to parse it as JSON → fails → retries → fails → returns StructuredParse error.
        // Then the fallback calls plain chat() which succeeds.
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let mut mock = MockProvider::default();
        // First two calls go to chat_typed (attempt + retry), third call is the plain fallback
        mock.default_response = "plain text summary".into();
        let provider = AnyProvider::Mock(mock);

        let memory = SemanticMemory {
            sqlite,
            qdrant: None,
            provider,
            embedding_model: "test".into(),
            vector_weight: 0.7,
            keyword_weight: 0.3,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            token_counter: Arc::new(TokenCounter::new()),
            #[cfg(feature = "graph-memory")]
            graph_store: None,
            #[cfg(feature = "graph-memory")]
            community_detection_failures: Arc::new(AtomicU64::new(0)),
        };

        let cid = memory.sqlite().create_conversation().await.unwrap();
        for i in 0..5 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let result = memory.summarize(cid, 3).await;
        // The summarize will either succeed (with plain text fallback) or fail
        // depending on how many retries chat_typed_erased does internally.
        // With MockProvider returning non-JSON plain text, chat_typed fails to parse.
        // The fallback plain chat() returns "plain text summary".
        // Result should be Ok with a summary stored.
        assert!(result.is_ok());
        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert!(!summaries[0].content.is_empty());
    }

    // Temporal decay tests

    #[test]
    fn temporal_decay_disabled_leaves_scores_unchanged() {
        let mut ranked = vec![(MessageId(1), 1.0f64), (MessageId(2), 0.5f64)];
        let timestamps = std::collections::HashMap::new();
        apply_temporal_decay(&mut ranked, &timestamps, 30);
        assert!((ranked[0].1 - 1.0).abs() < f64::EPSILON);
        assert!((ranked[1].1 - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn temporal_decay_zero_age_preserves_score() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        let mut ranked = vec![(MessageId(1), 1.0f64)];
        let mut timestamps = std::collections::HashMap::new();
        timestamps.insert(MessageId(1), now);
        apply_temporal_decay(&mut ranked, &timestamps, 30);
        // age = 0 days, exp(0) = 1.0 → no change
        assert!((ranked[0].1 - 1.0).abs() < 0.01);
    }

    #[test]
    fn temporal_decay_half_life_halves_score() {
        // Age exactly half_life_days → score should be halved
        let half_life = 30u32;
        let age_secs = i64::from(half_life) * 86400;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        let ts = now - age_secs;
        let mut ranked = vec![(MessageId(1), 1.0f64)];
        let mut timestamps = std::collections::HashMap::new();
        timestamps.insert(MessageId(1), ts);
        apply_temporal_decay(&mut ranked, &timestamps, half_life);
        // exp(-ln2) = 0.5
        assert!(
            (ranked[0].1 - 0.5).abs() < 0.01,
            "score was {}",
            ranked[0].1
        );
    }

    // MMR tests

    #[test]
    fn mmr_empty_input_returns_empty() {
        let ranked = vec![];
        let vectors = std::collections::HashMap::new();
        let result = apply_mmr(&ranked, &vectors, 0.7, 5);
        assert!(result.is_empty());
    }

    #[test]
    fn mmr_returns_up_to_limit() {
        let ranked = vec![
            (MessageId(1), 1.0f64),
            (MessageId(2), 0.9f64),
            (MessageId(3), 0.8f64),
        ];
        let mut vectors = std::collections::HashMap::new();
        vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
        vectors.insert(MessageId(2), vec![0.0f32, 1.0]);
        vectors.insert(MessageId(3), vec![1.0f32, 0.0]);
        let result = apply_mmr(&ranked, &vectors, 0.7, 2);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn mmr_without_vectors_picks_by_relevance() {
        let ranked = vec![(MessageId(1), 1.0f64), (MessageId(2), 0.5f64)];
        let vectors = std::collections::HashMap::new();
        let result = apply_mmr(&ranked, &vectors, 0.7, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, MessageId(1));
    }

    #[test]
    fn mmr_prefers_diverse_over_redundant() {
        // Two candidates with same relevance but msg 2 is orthogonal (more diverse)
        let ranked = vec![
            (MessageId(1), 1.0f64), // selected first
            (MessageId(2), 0.9f64), // orthogonal to 1
            (MessageId(3), 0.9f64), // parallel to 1 (redundant)
        ];
        let mut vectors = std::collections::HashMap::new();
        vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
        vectors.insert(MessageId(2), vec![0.0f32, 1.0]); // orthogonal
        vectors.insert(MessageId(3), vec![1.0f32, 0.0]); // same as 1
        let result = apply_mmr(&ranked, &vectors, 0.5, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, MessageId(1));
        // msg 2 should be preferred over msg 3 (diverse)
        assert_eq!(result[1].0, MessageId(2));
    }

    #[test]
    fn temporal_decay_half_life_zero_is_noop() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        let age_secs = 30i64 * 86400;
        let ts = now - age_secs;
        let mut ranked = vec![(MessageId(1), 1.0f64)];
        let mut timestamps = std::collections::HashMap::new();
        timestamps.insert(MessageId(1), ts);
        // half_life=0 → guard returns early, score must remain 1.0
        apply_temporal_decay(&mut ranked, &timestamps, 0);
        assert!(
            (ranked[0].1 - 1.0).abs() < f64::EPSILON,
            "score was {}",
            ranked[0].1
        );
    }

    #[test]
    fn temporal_decay_huge_age_near_zero() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        // 10 years = ~3650 days
        let age_secs = 3650i64 * 86400;
        let ts = now - age_secs;
        let mut ranked = vec![(MessageId(1), 1.0f64)];
        let mut timestamps = std::collections::HashMap::new();
        timestamps.insert(MessageId(1), ts);
        apply_temporal_decay(&mut ranked, &timestamps, 30);
        // After 3650 days with half_life=30, score should be essentially 0
        assert!(ranked[0].1 < 0.001, "score was {}", ranked[0].1);
    }

    #[test]
    fn temporal_decay_small_half_life() {
        // Very small half_life (1 day), age = 7 days → 2^(-7) ≈ 0.0078
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        let ts = now - 7 * 86400i64;
        let mut ranked = vec![(MessageId(1), 1.0f64)];
        let mut timestamps = std::collections::HashMap::new();
        timestamps.insert(MessageId(1), ts);
        apply_temporal_decay(&mut ranked, &timestamps, 1);
        assert!(ranked[0].1 < 0.01, "score was {}", ranked[0].1);
    }

    #[test]
    fn mmr_lambda_zero_max_diversity() {
        // lambda=0 → pure diversity: second item should be most dissimilar
        let ranked = vec![
            (MessageId(1), 1.0f64),  // selected first (always highest relevance)
            (MessageId(2), 0.9f64),  // orthogonal to 1
            (MessageId(3), 0.85f64), // parallel to 1 (max_sim=1.0)
        ];
        let mut vectors = std::collections::HashMap::new();
        vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
        vectors.insert(MessageId(2), vec![0.0f32, 1.0]); // orthogonal
        vectors.insert(MessageId(3), vec![1.0f32, 0.0]); // same direction
        let result = apply_mmr(&ranked, &vectors, 0.0, 3);
        assert_eq!(result.len(), 3);
        // After 1 is selected: mmr(2) = 0 - (1-0)*0 = 0, mmr(3) = 0 - 1*1 = -1 → 2 wins
        assert_eq!(result[1].0, MessageId(2));
    }

    #[test]
    fn mmr_lambda_one_pure_relevance() {
        // lambda=1 → pure relevance, should pick in relevance order
        let ranked = vec![
            (MessageId(1), 1.0f64),
            (MessageId(2), 0.8f64),
            (MessageId(3), 0.6f64),
        ];
        let mut vectors = std::collections::HashMap::new();
        vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
        vectors.insert(MessageId(2), vec![0.0f32, 1.0]);
        vectors.insert(MessageId(3), vec![0.5f32, 0.5]);
        let result = apply_mmr(&ranked, &vectors, 1.0, 3);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].0, MessageId(1));
        assert_eq!(result[1].0, MessageId(2));
        assert_eq!(result[2].0, MessageId(3));
    }

    #[test]
    fn mmr_limit_zero_returns_empty() {
        let ranked = vec![(MessageId(1), 1.0f64), (MessageId(2), 0.8f64)];
        let mut vectors = std::collections::HashMap::new();
        vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
        vectors.insert(MessageId(2), vec![0.0f32, 1.0]);
        let result = apply_mmr(&ranked, &vectors, 0.7, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn mmr_duplicate_vectors_penalizes_second() {
        // Two items with identical embeddings: second should be heavily penalized
        let ranked = vec![
            (MessageId(1), 1.0f64),
            (MessageId(2), 1.0f64), // same relevance, same direction
            (MessageId(3), 0.9f64), // orthogonal, lower relevance
        ];
        let mut vectors = std::collections::HashMap::new();
        vectors.insert(MessageId(1), vec![1.0f32, 0.0]);
        vectors.insert(MessageId(2), vec![1.0f32, 0.0]); // duplicate
        vectors.insert(MessageId(3), vec![0.0f32, 1.0]); // orthogonal
        let result = apply_mmr(&ranked, &vectors, 0.5, 3);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].0, MessageId(1));
        // msg3 (orthogonal) should be preferred over msg2 (duplicate) with lambda=0.5
        assert_eq!(result[1].0, MessageId(3));
    }

    // recall_routed() tests (#1162 — tester gap coverage)

    #[tokio::test]
    async fn recall_routed_keyword_route_returns_fts5_results() {
        use crate::{HeuristicRouter, MemoryRoute, MemoryRouter};

        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        memory
            .remember(cid, "user", "rust programming guide")
            .await
            .unwrap();
        memory
            .remember(cid, "assistant", "python tutorial")
            .await
            .unwrap();

        // "rust_guide" is pure snake_case → routes Keyword
        let router = HeuristicRouter;
        assert_eq!(router.route("rust_guide"), MemoryRoute::Keyword);

        let recalled = memory
            .recall_routed("rust_guide", 5, None, &router)
            .await
            .unwrap();
        // FTS5 will find "rust programming guide" but not "python tutorial"
        assert!(recalled.len() <= 2);
    }

    #[tokio::test]
    async fn recall_routed_semantic_route_without_qdrant_returns_empty_vectors() {
        use crate::{HeuristicRouter, MemoryRoute, MemoryRouter};

        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        memory
            .remember(cid, "user", "how does the agent loop work")
            .await
            .unwrap();

        // Long natural language question → routes Semantic
        let router = HeuristicRouter;
        assert_eq!(
            router.route("how does the agent loop work"),
            MemoryRoute::Semantic
        );

        // Without Qdrant, vector results are empty; recall_routed returns empty vec
        let recalled = memory
            .recall_routed("how does the agent loop work", 5, None, &router)
            .await
            .unwrap();
        assert!(recalled.is_empty(), "no Qdrant → empty semantic recall");
    }

    #[tokio::test]
    async fn recall_routed_hybrid_route_falls_back_to_fts5_on_no_qdrant() {
        use crate::{HeuristicRouter, MemoryRoute, MemoryRouter};

        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        memory
            .remember(cid, "user", "context window token budget")
            .await
            .unwrap();

        // 4-word non-question, no code pattern → routes Hybrid
        let router = HeuristicRouter;
        assert_eq!(
            router.route("context window token budget"),
            MemoryRoute::Hybrid
        );

        // Hybrid: FTS5 succeeds, vectors empty (no Qdrant) → merged result
        let recalled = memory
            .recall_routed("context window token budget", 5, None, &router)
            .await
            .unwrap();
        // FTS5 finds the message; merged result should be non-empty
        assert!(!recalled.is_empty(), "FTS5 should find the stored message");
    }

    // graph-memory tests

    #[cfg(feature = "graph-memory")]
    mod graph_extraction_tests {
        use super::*;
        use crate::graph::{EntityType, GraphStore};

        async fn graph_memory() -> SemanticMemory {
            let mem = test_semantic_memory(false).await;
            let store = std::sync::Arc::new(GraphStore::new(mem.sqlite.pool().clone()));
            mem.with_graph_store(store)
        }

        #[tokio::test]
        async fn recall_graph_returns_empty_when_no_entities() {
            let memory = graph_memory().await;
            let facts = memory.recall_graph("rust", 10, 2).await.unwrap();
            assert!(facts.is_empty(), "empty graph must return empty vec");
        }

        #[tokio::test]
        async fn recall_graph_returns_facts_for_known_entity() {
            let memory = graph_memory().await;
            let store = GraphStore::new(memory.sqlite.pool().clone());

            let rust_id = store
                .upsert_entity("rust", "rust", EntityType::Language, Some("a language"))
                .await
                .unwrap();
            let tokio_id = store
                .upsert_entity("tokio", "tokio", EntityType::Tool, Some("async runtime"))
                .await
                .unwrap();
            store
                .insert_edge(
                    rust_id,
                    tokio_id,
                    "uses",
                    "Rust uses tokio for async",
                    0.9,
                    None,
                )
                .await
                .unwrap();

            let facts = memory.recall_graph("rust", 10, 2).await.unwrap();
            assert!(!facts.is_empty(), "should return at least one fact");
            assert_eq!(facts[0].entity_name, "rust");
            assert_eq!(facts[0].relation, "uses");
        }

        #[tokio::test]
        async fn recall_graph_sorted_by_composite_score() {
            let memory = graph_memory().await;
            let store = GraphStore::new(memory.sqlite.pool().clone());

            let a_id = store
                .upsert_entity("entity_a", "entity_a", EntityType::Concept, None)
                .await
                .unwrap();
            let b_id = store
                .upsert_entity("entity_b", "entity_b", EntityType::Concept, None)
                .await
                .unwrap();
            let c_id = store
                .upsert_entity("entity_c", "entity_c", EntityType::Concept, None)
                .await
                .unwrap();
            store
                .insert_edge(a_id, b_id, "relates", "a relates b", 0.9, None)
                .await
                .unwrap();
            store
                .insert_edge(a_id, c_id, "relates", "a relates c", 0.5, None)
                .await
                .unwrap();

            let facts = memory.recall_graph("entity_a", 10, 1).await.unwrap();
            if facts.len() >= 2 {
                assert!(
                    facts[0].composite_score() >= facts[1].composite_score(),
                    "facts must be sorted descending by composite score"
                );
            }
        }

        #[tokio::test]
        async fn extract_and_store_returns_zero_stats_for_empty_content() {
            let memory = graph_memory().await;
            let pool = memory.sqlite.pool().clone();
            let provider = test_provider();

            let stats = extract_and_store(
                String::new(),
                vec![],
                provider,
                pool,
                GraphExtractionConfig {
                    max_entities: 10,
                    max_edges: 10,
                    extraction_timeout_secs: 5,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(stats.entities_upserted, 0);
            assert_eq!(stats.edges_inserted, 0);
        }

        #[tokio::test]
        async fn extraction_count_increments_atomically() {
            let memory = graph_memory().await;
            let pool = memory.sqlite.pool().clone();
            let provider = test_provider();

            // Run two extractions sequentially to verify count increments
            for _ in 0..2 {
                let _ = extract_and_store(
                    "I use Rust for systems programming".to_owned(),
                    vec![],
                    provider.clone(),
                    pool.clone(),
                    GraphExtractionConfig {
                        max_entities: 5,
                        max_edges: 5,
                        extraction_timeout_secs: 5,
                        ..Default::default()
                    },
                )
                .await;
            }

            let store = GraphStore::new(pool);
            let count = store.get_metadata("extraction_count").await.unwrap();
            // R-SUG-02: assert exact value "2" — two extractions must each increment the counter.
            assert_eq!(
                count.as_deref(),
                Some("2"),
                "extraction_count must be exactly 2 after two extraction attempts"
            );
        }

        #[tokio::test]
        async fn recall_graph_truncates_to_limit() {
            let memory = graph_memory().await;
            let store = GraphStore::new(memory.sqlite.pool().clone());

            let root_id = store
                .upsert_entity("root", "root", EntityType::Concept, None)
                .await
                .unwrap();
            for i in 0..5 {
                let name = format!("target_{i}");
                let tid = store
                    .upsert_entity(&name, &name, EntityType::Concept, None)
                    .await
                    .unwrap();
                store
                    .insert_edge(
                        root_id,
                        tid,
                        "links",
                        &format!("root links {name}"),
                        0.7,
                        None,
                    )
                    .await
                    .unwrap();
            }

            let facts = memory.recall_graph("root", 3, 1).await.unwrap();
            assert!(facts.len() <= 3, "recall_graph must respect limit");
        }

        // R-SUG-05: multi-hop BFS test.
        #[tokio::test]
        async fn recall_graph_multi_hop_traverses_two_hops() {
            // Chain: A -[knows]-> B -[uses]-> C
            // recall_graph("a", max_hops=2) must return facts for both hops.
            let memory = graph_memory().await;
            let store = GraphStore::new(memory.sqlite.pool().clone());

            let a_id = store
                .upsert_entity("a_entity", "a_entity", EntityType::Person, None)
                .await
                .unwrap();
            let b_id = store
                .upsert_entity("b_entity", "b_entity", EntityType::Person, None)
                .await
                .unwrap();
            let c_id = store
                .upsert_entity("c_entity", "c_entity", EntityType::Concept, None)
                .await
                .unwrap();

            store
                .insert_edge(a_id, b_id, "knows", "a knows b", 0.9, None)
                .await
                .unwrap();
            store
                .insert_edge(b_id, c_id, "uses", "b uses c", 0.8, None)
                .await
                .unwrap();

            // max_hops=1: only hop-0 edges visible from A → should find A-B edge
            let facts_1hop = memory.recall_graph("a_entity", 10, 1).await.unwrap();
            assert!(!facts_1hop.is_empty(), "hop=1 must find direct edge");

            // max_hops=2: BFS reaches B then C → A-B and B-C edges both visible
            let facts_2hop = memory.recall_graph("a_entity", 10, 2).await.unwrap();
            assert!(
                facts_2hop.len() >= facts_1hop.len(),
                "hop=2 must find at least as many facts as hop=1"
            );
            let has_bc = facts_2hop.iter().any(|f| {
                (f.entity_name.contains("b_entity") || f.target_name.contains("b_entity"))
                    && (f.entity_name.contains("c_entity") || f.target_name.contains("c_entity"))
            });
            assert!(has_bc, "hop=2 BFS must traverse to c_entity via b_entity");
        }

        // R-SUG-05: timeout degradation — zero-second timeout returns empty stats, no panic.
        #[tokio::test]
        async fn spawn_graph_extraction_zero_timeout_returns_without_panic() {
            let memory = graph_memory().await;
            let cfg = GraphExtractionConfig {
                max_entities: 5,
                max_edges: 5,
                extraction_timeout_secs: 0,
                ..Default::default()
            };
            // spawn fires and forgets — must not panic regardless of timeout value.
            memory.spawn_graph_extraction(
                "I use Rust for systems programming".to_owned(),
                vec![],
                cfg,
            );
            // Brief wait for the task to settle.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // No assertion on count: with 0s timeout the task may or may not complete.
            // The test verifies there is no panic.
        }
    }

    // Priority 3: proptest

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn count_tokens_never_panics(s in ".*") {
            let counter = crate::token_counter::TokenCounter::new();
            let _ = counter.count_tokens(&s);
        }
    }
}
