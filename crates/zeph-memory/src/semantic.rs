// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role};

use std::sync::Arc;

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
    qdrant: Option<EmbeddingStore>,
    provider: AnyProvider,
    embedding_model: String,
    vector_weight: f64,
    keyword_weight: f64,
    temporal_decay_enabled: bool,
    temporal_decay_half_life_days: u32,
    mmr_enabled: bool,
    mmr_lambda: f32,
    pub token_counter: Arc<TokenCounter>,
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
            Ok(store) => Some(store),
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
        })
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
        qdrant: Option<EmbeddingStore>,
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
            qdrant: Some(store),
            provider,
            embedding_model: embedding_model.into(),
            vector_weight,
            keyword_weight,
            temporal_decay_enabled: false,
            temporal_decay_half_life_days: 30,
            mmr_enabled: false,
            mmr_lambda: 0.7,
            token_counter: Arc::new(TokenCounter::new()),
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
    #[allow(clippy::too_many_lines)]
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

        // Merge results with weighted scoring
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

        // Sort by combined score descending
        let mut ranked: Vec<(MessageId, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Apply temporal decay (before MMR)
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

        // Apply MMR re-ranking (after decay, before truncation)
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
        let qdrant = Some(crate::embedding_store::EmbeddingStore::new_sqlite(pool));

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
