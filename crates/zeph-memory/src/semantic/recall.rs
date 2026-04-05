// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{LlmProvider as _, Message};

/// Approximate characters per token (conservative estimate for mixed content).
const CHARS_PER_TOKEN: usize = 4;

/// Target chunk size in characters (~400 tokens).
const CHUNK_CHARS: usize = 400 * CHARS_PER_TOKEN;

/// Overlap between adjacent chunks in characters (~80 tokens).
const CHUNK_OVERLAP_CHARS: usize = 80 * CHARS_PER_TOKEN;

/// Split `text` into overlapping chunks suitable for embedding.
///
/// For text shorter than `CHUNK_CHARS`, returns a single chunk.
/// Splits at UTF-8 character boundaries on paragraph (`\n\n`), line (`\n`),
/// space (` `), or raw character boundaries as a last resort.
fn chunk_text(text: &str) -> Vec<&str> {
    if text.len() <= CHUNK_CHARS {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let end = if start + CHUNK_CHARS >= text.len() {
            text.len()
        } else {
            // Find a clean UTF-8 char boundary at or before start + CHUNK_CHARS.
            let boundary = text.floor_char_boundary(start + CHUNK_CHARS);
            // Prefer to split at a paragraph or line break for cleaner chunks.
            let slice = &text[start..boundary];
            if let Some(pos) = slice.rfind("\n\n") {
                start + pos + 2
            } else if let Some(pos) = slice.rfind('\n') {
                start + pos + 1
            } else if let Some(pos) = slice.rfind(' ') {
                start + pos + 1
            } else {
                boundary
            }
        };

        chunks.push(&text[start..end]);
        if end >= text.len() {
            break;
        }
        // Next chunk starts with overlap.
        let next = end.saturating_sub(CHUNK_OVERLAP_CHARS);
        start = text.ceil_char_boundary(next);
        if start >= end {
            start = end; // safeguard against infinite loop
        }
    }

    chunks
}

use crate::admission::log_admission_decision;
use crate::embedding_store::{MessageKind, SearchFilter};
use crate::error::MemoryError;
use crate::types::{ConversationId, MessageId};

use super::SemanticMemory;
use super::algorithms::{apply_mmr, apply_temporal_decay};

/// Tool execution metadata stored as Qdrant payload fields alongside embeddings.
///
/// Stored as payload — NOT prepended to content — to avoid corrupting embedding vectors.
#[derive(Debug, Clone, Default)]
pub struct EmbedContext {
    pub tool_name: Option<String>,
    pub exit_code: Option<i32>,
    pub timestamp: Option<String>,
}

#[derive(Debug)]
pub struct RecalledMessage {
    pub message: Message,
    pub score: f32,
}

impl SemanticMemory {
    /// Save a message to `SQLite` and optionally embed and store in Qdrant.
    ///
    /// Returns `Ok(Some(message_id))` when admitted and persisted.
    /// Returns `Ok(None)` when A-MAC admission control rejects the message (not an error).
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
        goal_text: Option<&str>,
    ) -> Result<Option<MessageId>, MemoryError> {
        // A-MAC admission gate.
        if let Some(ref admission) = self.admission_control {
            let decision = admission
                .evaluate(
                    content,
                    role,
                    &self.provider,
                    self.qdrant.as_ref(),
                    goal_text,
                )
                .await;
            let preview: String = content.chars().take(100).collect();
            log_admission_decision(&decision, &preview, role, admission.threshold());
            if !decision.admitted {
                return Ok(None);
            }
        }

        let message_id = self
            .sqlite
            .save_message(conversation_id, role, content)
            .await?;

        self.embed_and_store_regular(message_id, conversation_id, role, content)
            .await;

        Ok(Some(message_id))
    }

    /// Save a message with pre-serialized parts JSON to `SQLite` and optionally embed in Qdrant.
    ///
    /// Returns `Ok((Some(message_id), embedding_stored))` when admitted and persisted.
    /// Returns `Ok((None, false))` when A-MAC admission control rejects the message.
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
        goal_text: Option<&str>,
    ) -> Result<(Option<MessageId>, bool), MemoryError> {
        // A-MAC admission gate.
        if let Some(ref admission) = self.admission_control {
            let decision = admission
                .evaluate(
                    content,
                    role,
                    &self.provider,
                    self.qdrant.as_ref(),
                    goal_text,
                )
                .await;
            let preview: String = content.chars().take(100).collect();
            log_admission_decision(&decision, &preview, role, admission.threshold());
            if !decision.admitted {
                return Ok((None, false));
            }
        }

        let message_id = self
            .sqlite
            .save_message_with_parts(conversation_id, role, content, parts_json)
            .await?;

        let embedding_stored = self
            .embed_and_store_regular(message_id, conversation_id, role, content)
            .await;

        Ok((Some(message_id), embedding_stored))
    }

    /// Save a tool output to `SQLite` and embed with tool metadata in Qdrant payload.
    ///
    /// Tool metadata (`tool_name`, `exit_code`, `timestamp`) is stored as Qdrant payload fields
    /// so it is available for filtering without corrupting the embedding vector.
    ///
    /// Returns `Ok(Some(message_id))` when admitted and persisted.
    /// Returns `Ok(None)` when A-MAC admission control rejects the message.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` save fails.
    pub async fn remember_tool_output(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        parts_json: &str,
        embed_ctx: EmbedContext,
    ) -> Result<(Option<MessageId>, bool), MemoryError> {
        if let Some(ref admission) = self.admission_control {
            let decision = admission
                .evaluate(content, role, &self.provider, self.qdrant.as_ref(), None)
                .await;
            let preview: String = content.chars().take(100).collect();
            log_admission_decision(&decision, &preview, role, admission.threshold());
            if !decision.admitted {
                return Ok((None, false));
            }
        }

        let message_id = self
            .sqlite
            .save_message_with_parts(conversation_id, role, content, parts_json)
            .await?;

        let embedding_stored = self
            .embed_chunks_with_tool_context(message_id, conversation_id, role, content, embed_ctx)
            .await;

        Ok((Some(message_id), embedding_stored))
    }

    /// Embed content chunks and store each as a regular (non-tool) message vector.
    ///
    /// Handles: chunking → `embed_batch` → `ensure_collection` → per-chunk `store`.
    /// Returns `true` if at least one chunk was successfully stored.
    async fn embed_and_store_regular(
        &self,
        message_id: MessageId,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
    ) -> bool {
        let Some(qdrant) = &self.qdrant else {
            return false;
        };
        if !self.provider.supports_embeddings() {
            return false;
        }

        let chunks = chunk_text(content);
        let chunk_count = chunks.len();

        let vectors = match self.provider.embed_batch(&chunks).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Failed to embed chunks for msg {message_id}: {e:#}");
                return false;
            }
        };

        let Some(first) = vectors.first() else {
            return false;
        };
        let vector_size = u64::try_from(first.len()).unwrap_or(896);
        if let Err(e) = qdrant.ensure_collection(vector_size).await {
            tracing::warn!("Failed to ensure Qdrant collection: {e:#}");
            return false;
        }

        let mut stored = false;
        for (chunk_index, vector) in vectors.into_iter().enumerate() {
            let chunk_index_u32 = u32::try_from(chunk_index).unwrap_or(u32::MAX);
            match qdrant
                .store(
                    message_id,
                    conversation_id,
                    role,
                    vector,
                    MessageKind::Regular,
                    &self.embedding_model,
                    chunk_index_u32,
                )
                .await
            {
                Ok(_) => stored = true,
                Err(e) => tracing::warn!(
                    "Failed to store chunk {chunk_index}/{chunk_count} \
                     for msg {message_id}: {e:#}"
                ),
            }
        }

        stored
    }

    /// Embed content chunks, enriching Qdrant payload with tool metadata when present.
    ///
    /// Returns `true` if at least one chunk was successfully stored.
    async fn embed_chunks_with_tool_context(
        &self,
        message_id: MessageId,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        embed_ctx: EmbedContext,
    ) -> bool {
        let Some(qdrant) = &self.qdrant else {
            return false;
        };
        if !self.provider.supports_embeddings() {
            return false;
        }

        let chunks = chunk_text(content);
        let chunk_count = chunks.len();
        let mut stored = false;

        // Embed all chunks in a single batch call.
        // Batch semantics are atomic: if the batch fails, skip embedding for this message.
        let vectors = match self.provider.embed_batch(&chunks).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Failed to embed tool-output chunks for msg {message_id}: {e:#}");
                return false;
            }
        };

        if let Some(first) = vectors.first() {
            let vector_size = u64::try_from(first.len()).unwrap_or(896);
            if let Err(e) = qdrant.ensure_collection(vector_size).await {
                tracing::warn!("Failed to ensure Qdrant collection: {e:#}");
                return false;
            }
        }

        for (chunk_index, vector) in vectors.into_iter().enumerate() {
            let chunk_index_u32 = u32::try_from(chunk_index).unwrap_or(u32::MAX);
            let result = if let Some(ref tool_name) = embed_ctx.tool_name {
                qdrant
                    .store_with_tool_context(
                        message_id,
                        conversation_id,
                        role,
                        vector,
                        MessageKind::Regular,
                        &self.embedding_model,
                        chunk_index_u32,
                        tool_name,
                        embed_ctx.exit_code,
                        embed_ctx.timestamp.as_deref(),
                    )
                    .await
                    .map(|_| ())
            } else {
                qdrant
                    .store(
                        message_id,
                        conversation_id,
                        role,
                        vector,
                        MessageKind::Regular,
                        &self.embedding_model,
                        chunk_index_u32,
                    )
                    .await
                    .map(|_| ())
            };
            match result {
                Ok(()) => stored = true,
                Err(e) => tracing::warn!(
                    "Failed to store tool-output chunk {chunk_index}/{chunk_count} \
                     for msg {message_id}: {e:#}"
                ),
            }
        }

        stored
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

        tracing::debug!(
            query_len = query.len(),
            limit,
            has_filter = filter.is_some(),
            conversation_id = conversation_id.map(|c| c.0),
            has_qdrant = self.qdrant.is_some(),
            "recall: starting hybrid search"
        );

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

    pub(super) async fn recall_fts5_raw(
        &self,
        query: &str,
        limit: usize,
        conversation_id: Option<ConversationId>,
    ) -> Result<Vec<(MessageId, f64)>, MemoryError> {
        self.sqlite
            .keyword_search(query, limit * 2, conversation_id)
            .await
    }

    pub(super) async fn recall_vectors_raw(
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
    #[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
    pub(super) async fn recall_merge_and_rank(
        &self,
        keyword_results: Vec<(MessageId, f64)>,
        vector_results: Vec<crate::embedding_store::SearchResult>,
        limit: usize,
    ) -> Result<Vec<RecalledMessage>, MemoryError> {
        tracing::debug!(
            vector_count = vector_results.len(),
            keyword_count = keyword_results.len(),
            limit,
            "recall: merging search results"
        );

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
            tracing::debug!("recall: empty merge, no overlapping scores");
            return Ok(Vec::new());
        }

        let mut ranked: Vec<(MessageId, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        tracing::debug!(
            merged = ranked.len(),
            top_score = ranked.first().map(|r| r.1),
            bottom_score = ranked.last().map(|r| r.1),
            vector_weight = %self.vector_weight,
            keyword_weight = %self.keyword_weight,
            "recall: weighted merge complete"
        );

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
                    tracing::debug!(
                        half_life_days = self.temporal_decay_half_life_days,
                        top_score_after = ranked.first().map(|r| r.1),
                        "recall: temporal decay applied"
                    );
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
                        let ranked_len_before = ranked.len();
                        ranked = apply_mmr(&ranked, &vec_map, self.mmr_lambda, limit);
                        tracing::debug!(
                            before = ranked_len_before,
                            after = ranked.len(),
                            lambda = %self.mmr_lambda,
                            "recall: mmr re-ranked"
                        );
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

        if self.importance_enabled && !ranked.is_empty() {
            let ids: Vec<MessageId> = ranked.iter().map(|r| r.0).collect();
            match self.sqlite.fetch_importance_scores(&ids).await {
                Ok(scores) => {
                    for (msg_id, score) in &mut ranked {
                        if let Some(&imp) = scores.get(msg_id) {
                            *score += imp * self.importance_weight;
                        }
                    }
                    ranked
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                    tracing::debug!(
                        importance_weight = %self.importance_weight,
                        "recall: importance scores blended"
                    );
                }
                Err(e) => {
                    tracing::warn!("importance scoring: failed to fetch scores: {e:#}");
                }
            }
        }

        // Apply tier boost: semantic-tier messages receive an additive bonus so distilled facts
        // rank above episodic messages with the same base score. Additive (not multiplicative)
        // so the effect is consistent regardless of base score magnitude.
        if (self.tier_boost_semantic - 1.0).abs() > f64::EPSILON && !ranked.is_empty() {
            let ids: Vec<MessageId> = ranked.iter().map(|r| r.0).collect();
            match self.sqlite.fetch_tiers(&ids).await {
                Ok(tiers) => {
                    let bonus = self.tier_boost_semantic - 1.0;
                    let mut boosted = false;
                    for (msg_id, score) in &mut ranked {
                        if tiers.get(msg_id).map(String::as_str) == Some("semantic") {
                            *score += bonus;
                            boosted = true;
                        }
                    }
                    if boosted {
                        ranked.sort_by(|a, b| {
                            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                        });
                        tracing::debug!(
                            tier_boost = %self.tier_boost_semantic,
                            "recall: semantic tier boost applied"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("tier boost: failed to fetch tiers: {e:#}");
                }
            }
        }

        let ids: Vec<MessageId> = ranked.iter().map(|r| r.0).collect();

        if !ids.is_empty()
            && let Err(e) = self.batch_increment_access_count(ids.clone()).await
        {
            tracing::warn!("recall: failed to increment access counts: {e:#}");
        }

        // Update RL admission training data: mark recalled messages as positive examples.
        if let Err(e) = self.sqlite.mark_training_recalled(&ids).await {
            tracing::debug!(
                error = %e,
                "recall: failed to mark training data as recalled (non-fatal)"
            );
        }

        let messages = self.sqlite.messages_by_ids(&ids).await?;
        let msg_map: std::collections::HashMap<MessageId, _> = messages.into_iter().collect();

        let recalled: Vec<RecalledMessage> = ranked
            .iter()
            .filter_map(|(msg_id, score)| {
                msg_map.get(msg_id).map(|msg| RecalledMessage {
                    message: msg.clone(),
                    #[expect(clippy::cast_possible_truncation)]
                    score: *score as f32,
                })
            })
            .collect();

        tracing::debug!(final_count = recalled.len(), "recall: final results");

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
                let kw = match self.recall_fts5_raw(query, limit, conversation_id).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("FTS5 keyword search failed: {e:#}");
                        Vec::new()
                    }
                };
                let vr = self.recall_vectors_raw(query, limit, filter).await?;
                (kw, vr)
            }
            // Episodic: FTS5 keyword search with an optional timestamp-range filter.
            // Temporal keywords are stripped from the query before passing to FTS5 to
            // prevent BM25 score distortion (e.g. "yesterday" matching messages that
            // literally contain the word "yesterday" regardless of actual relevance).
            // Vector search is skipped for speed; temporal decay in recall_merge_and_rank
            // provides recency boosting for the FTS5 results.
            // Known trade-off (MVP): semantically similar but lexically different messages
            // may be missed. See issue #1629 for a future hybrid_temporal mode.
            MemoryRoute::Episodic => {
                let range = crate::router::resolve_temporal_range(query, chrono::Utc::now());
                let cleaned = crate::router::strip_temporal_keywords(query);
                let search_query = if cleaned.is_empty() { query } else { &cleaned };
                let kw = if let Some(ref r) = range {
                    self.sqlite
                        .keyword_search_with_time_range(
                            search_query,
                            limit,
                            conversation_id,
                            r.after.as_deref(),
                            r.before.as_deref(),
                        )
                        .await?
                } else {
                    self.recall_fts5_raw(search_query, limit, conversation_id)
                        .await?
                };
                tracing::debug!(
                    has_range = range.is_some(),
                    cleaned_query = %search_query,
                    keyword_count = kw.len(),
                    "recall: episodic path"
                );
                (kw, Vec::new())
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

        tracing::debug!(
            keyword_count = keyword_results.len(),
            vector_count = vector_results.len(),
            "recall: routed search results"
        );

        self.recall_merge_and_rank(keyword_results, vector_results, limit)
            .await
    }

    /// Async variant of [`recall_routed`](Self::recall_routed) that uses
    /// [`AsyncMemoryRouter::route_async`](crate::router::AsyncMemoryRouter::route_async) when
    /// available, enabling LLM-based routing for `LlmRouter` and `HybridRouter`.
    ///
    /// Falls back to [`recall_routed`](Self::recall_routed) for routers that only implement
    /// the sync `MemoryRouter` trait (e.g. `HeuristicRouter`).
    ///
    /// # Errors
    ///
    /// Returns an error if any underlying search or database operation fails.
    pub async fn recall_routed_async(
        &self,
        query: &str,
        limit: usize,
        filter: Option<crate::embedding_store::SearchFilter>,
        router: &dyn crate::router::AsyncMemoryRouter,
    ) -> Result<Vec<RecalledMessage>, MemoryError> {
        use crate::router::MemoryRoute;

        let decision = router.route_async(query).await;
        let route = decision.route;
        tracing::debug!(
            ?route,
            confidence = decision.confidence,
            query_len = query.len(),
            "memory routing decision (async)"
        );

        let conversation_id = filter.as_ref().and_then(|f| f.conversation_id);

        let (keyword_results, vector_results): (
            Vec<(crate::types::MessageId, f64)>,
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
                let kw = match self.recall_fts5_raw(query, limit, conversation_id).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("FTS5 keyword search failed: {e:#}");
                        Vec::new()
                    }
                };
                let vr = self.recall_vectors_raw(query, limit, filter).await?;
                (kw, vr)
            }
            MemoryRoute::Episodic => {
                let range = crate::router::resolve_temporal_range(query, chrono::Utc::now());
                let cleaned = crate::router::strip_temporal_keywords(query);
                let search_query = if cleaned.is_empty() { query } else { &cleaned };
                let kw = if let Some(ref r) = range {
                    self.sqlite
                        .keyword_search_with_time_range(
                            search_query,
                            limit,
                            conversation_id,
                            r.after.as_deref(),
                            r.before.as_deref(),
                        )
                        .await?
                } else {
                    self.recall_fts5_raw(search_query, limit, conversation_id)
                        .await?
                };
                (kw, Vec::new())
            }
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

        tracing::debug!(
            keyword_count = keyword_results.len(),
            vector_count = vector_results.len(),
            "recall: routed search results (async)"
        );

        self.recall_merge_and_rank(keyword_results, vector_results, limit)
            .await
    }

    /// Retrieve graph facts relevant to `query` via BFS traversal.
    ///
    /// Returns an empty `Vec` if no `graph_store` is configured.
    ///
    /// # Parameters
    ///
    /// - `at_timestamp`: when `Some`, only edges valid at that `SQLite` datetime string are returned.
    ///   When `None`, only currently active edges are used.
    /// - `temporal_decay_rate`: non-negative decay rate (1/day). `0.0` preserves original ordering.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying graph query fails.
    pub async fn recall_graph(
        &self,
        query: &str,
        limit: usize,
        max_hops: u32,
        at_timestamp: Option<&str>,
        temporal_decay_rate: f64,
        edge_types: &[crate::graph::EdgeType],
    ) -> Result<Vec<crate::graph::types::GraphFact>, MemoryError> {
        let Some(store) = &self.graph_store else {
            return Ok(Vec::new());
        };

        tracing::debug!(
            query_len = query.len(),
            limit,
            max_hops,
            "graph: starting recall"
        );

        let results = crate::graph::retrieval::graph_recall(
            store,
            self.qdrant.as_deref(),
            &self.provider,
            query,
            limit,
            max_hops,
            at_timestamp,
            temporal_decay_rate,
            edge_types,
        )
        .await?;

        tracing::debug!(result_count = results.len(), "graph: recall complete");

        Ok(results)
    }

    /// Retrieve graph facts via SYNAPSE spreading activation.
    ///
    /// Delegates to [`crate::graph::retrieval::graph_recall_activated`].
    /// Used in place of [`recall_graph`] when `spreading_activation.enabled = true`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying graph query fails.
    pub async fn recall_graph_activated(
        &self,
        query: &str,
        limit: usize,
        params: crate::graph::SpreadingActivationParams,
        edge_types: &[crate::graph::EdgeType],
    ) -> Result<Vec<crate::graph::activation::ActivatedFact>, MemoryError> {
        let Some(store) = &self.graph_store else {
            return Ok(Vec::new());
        };

        tracing::debug!(
            query_len = query.len(),
            limit,
            "spreading activation: starting graph recall"
        );

        let embeddings = self.qdrant.as_deref();
        let results = crate::graph::retrieval::graph_recall_activated(
            store,
            embeddings,
            &self.provider,
            query,
            limit,
            params,
            edge_types,
        )
        .await?;

        tracing::debug!(
            result_count = results.len(),
            "spreading activation: graph recall complete"
        );

        Ok(results)
    }

    /// Increment access count and update `last_accessed` for a batch of message IDs.
    ///
    /// Skips the update if `message_ids` is empty to avoid an invalid `IN ()` clause.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` update fails.
    async fn batch_increment_access_count(
        &self,
        message_ids: Vec<MessageId>,
    ) -> Result<(), MemoryError> {
        if message_ids.is_empty() {
            return Ok(());
        }
        self.sqlite.increment_access_counts(&message_ids).await
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
        if self.qdrant.is_none() || !self.provider.supports_embeddings() {
            return Ok(0);
        }

        let unembedded = self.sqlite.unembedded_message_ids(Some(1000)).await?;

        if unembedded.is_empty() {
            return Ok(0);
        }

        let mut count = 0;
        for (msg_id, conversation_id, role, content) in &unembedded {
            if self
                .embed_and_store_regular(*msg_id, *conversation_id, role, content)
                .await
            {
                count += 1;
            }
        }

        tracing::info!("Embedded {count}/{} missing messages", unembedded.len());
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_context_default_all_none() {
        let ctx = EmbedContext::default();
        assert!(ctx.tool_name.is_none());
        assert!(ctx.exit_code.is_none());
        assert!(ctx.timestamp.is_none());
    }

    #[test]
    fn embed_context_fields_set_correctly() {
        let ctx = EmbedContext {
            tool_name: Some("shell".to_string()),
            exit_code: Some(0),
            timestamp: Some("2026-04-04T00:00:00Z".to_string()),
        };
        assert_eq!(ctx.tool_name.as_deref(), Some("shell"));
        assert_eq!(ctx.exit_code, Some(0));
        assert_eq!(ctx.timestamp.as_deref(), Some("2026-04-04T00:00:00Z"));
    }

    #[test]
    fn embed_context_non_zero_exit_code() {
        let ctx = EmbedContext {
            tool_name: Some("shell".to_string()),
            exit_code: Some(1),
            timestamp: None,
        };
        assert_eq!(ctx.exit_code, Some(1));
        assert!(ctx.timestamp.is_none());
    }
}
