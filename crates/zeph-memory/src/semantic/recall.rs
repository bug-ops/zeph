// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::{StreamExt as _, TryStreamExt as _};
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
        // Next chunk starts with overlap, but must always advance past the
        // current position to prevent infinite loops when rfind finds a match
        // very early in the slice (end barely advances, overlap rewinds start).
        let next = end.saturating_sub(CHUNK_OVERLAP_CHARS);
        let new_start = text.ceil_char_boundary(next);
        start = if new_start > start { new_start } else { end };
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

/// Maximum number of concurrent background embed tasks per `SemanticMemory` instance.
const MAX_EMBED_BG_TASKS: usize = 64;

/// Shared arguments for background embed tasks.
struct EmbedBgArgs {
    qdrant: Arc<crate::embedding_store::EmbeddingStore>,
    embed_provider: zeph_llm::any::AnyProvider,
    embedding_model: String,
    message_id: MessageId,
    conversation_id: ConversationId,
    role: String,
    content: String,
    last_qdrant_warn: Arc<AtomicU64>,
}

/// Background task: embed chunks and store as regular message vectors.
///
/// All errors are logged as warnings; the function never panics.
async fn embed_and_store_regular_bg(args: EmbedBgArgs) {
    let EmbedBgArgs {
        qdrant,
        embed_provider,
        embedding_model,
        message_id,
        conversation_id,
        role,
        content,
        last_qdrant_warn,
    } = args;
    let chunks = chunk_text(&content);
    let chunk_count = chunks.len();

    let vectors = match embed_provider.embed_batch(&chunks).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("bg embed_regular: failed to embed chunks for msg {message_id}: {e:#}");
            return;
        }
    };

    let Some(first) = vectors.first() else {
        return;
    };
    let vector_size = first.len() as u64;
    if let Err(e) = qdrant.ensure_collection(vector_size).await {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = last_qdrant_warn.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 10 {
            last_qdrant_warn.store(now, Ordering::Relaxed);
            tracing::warn!("bg embed_regular: failed to ensure Qdrant collection: {e:#}");
        } else {
            tracing::debug!(
                "bg embed_regular: failed to ensure Qdrant collection (suppressed): {e:#}"
            );
        }
        return;
    }

    for (chunk_index, vector) in vectors.into_iter().enumerate() {
        let chunk_index_u32 = u32::try_from(chunk_index).unwrap_or(u32::MAX);
        if let Err(e) = qdrant
            .store(
                message_id,
                conversation_id,
                &role,
                vector,
                MessageKind::Regular,
                &embedding_model,
                chunk_index_u32,
            )
            .await
        {
            tracing::warn!(
                "bg embed_regular: failed to store chunk {chunk_index}/{chunk_count} \
                 for msg {message_id}: {e:#}"
            );
        }
    }
}

/// Background task: embed chunks with tool context metadata and store in Qdrant.
///
/// All errors are logged as warnings; the function never panics.
async fn embed_chunks_with_tool_context_bg(args: EmbedBgArgs, embed_ctx: EmbedContext) {
    let EmbedBgArgs {
        qdrant,
        embed_provider,
        embedding_model,
        message_id,
        conversation_id,
        role,
        content,
        last_qdrant_warn,
    } = args;
    let chunks = chunk_text(&content);
    let chunk_count = chunks.len();

    let vectors = match embed_provider.embed_batch(&chunks).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "bg embed_tool: failed to embed tool-output chunks for msg {message_id}: {e:#}"
            );
            return;
        }
    };

    if let Some(first) = vectors.first() {
        let vector_size = first.len() as u64;
        if let Err(e) = qdrant.ensure_collection(vector_size).await {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let last = last_qdrant_warn.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= 10 {
                last_qdrant_warn.store(now, Ordering::Relaxed);
                tracing::warn!("bg embed_tool: failed to ensure Qdrant collection: {e:#}");
            } else {
                tracing::debug!(
                    "bg embed_tool: failed to ensure Qdrant collection (suppressed): {e:#}"
                );
            }
            return;
        }
    }

    for (chunk_index, vector) in vectors.into_iter().enumerate() {
        let chunk_index_u32 = u32::try_from(chunk_index).unwrap_or(u32::MAX);
        let result = if let Some(ref tool_name) = embed_ctx.tool_name {
            qdrant
                .store_with_tool_context(
                    message_id,
                    conversation_id,
                    &role,
                    vector,
                    MessageKind::Regular,
                    &embedding_model,
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
                    &role,
                    vector,
                    MessageKind::Regular,
                    &embedding_model,
                    chunk_index_u32,
                )
                .await
                .map(|_| ())
        };
        if let Err(e) = result {
            tracing::warn!(
                "bg embed_tool: failed to store chunk {chunk_index}/{chunk_count} \
                 for msg {message_id}: {e:#}"
            );
        }
    }
}

/// Background task: embed chunks with optional category and store in Qdrant.
///
/// All errors are logged as warnings; the function never panics.
async fn embed_and_store_with_category_bg(args: EmbedBgArgs, category: Option<String>) {
    let EmbedBgArgs {
        qdrant,
        embed_provider,
        embedding_model,
        message_id,
        conversation_id,
        role,
        content,
        last_qdrant_warn,
    } = args;
    let chunks = chunk_text(&content);
    let chunk_count = chunks.len();

    let vectors = match embed_provider.embed_batch(&chunks).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "bg embed_category: failed to embed categorized chunks for msg {message_id}: {e:#}"
            );
            return;
        }
    };

    let Some(first) = vectors.first() else {
        return;
    };
    let vector_size = first.len() as u64;
    if let Err(e) = qdrant.ensure_collection(vector_size).await {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let last = last_qdrant_warn.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 10 {
            last_qdrant_warn.store(now, Ordering::Relaxed);
            tracing::warn!("bg embed_category: failed to ensure Qdrant collection: {e:#}");
        } else {
            tracing::debug!(
                "bg embed_category: failed to ensure Qdrant collection (suppressed): {e:#}"
            );
        }
        return;
    }

    for (chunk_index, vector) in vectors.into_iter().enumerate() {
        let chunk_index_u32 = u32::try_from(chunk_index).unwrap_or(u32::MAX);
        if let Err(e) = qdrant
            .store_with_category(
                message_id,
                conversation_id,
                &role,
                vector,
                MessageKind::Regular,
                &embedding_model,
                chunk_index_u32,
                category.as_deref(),
            )
            .await
        {
            tracing::warn!(
                "bg embed_category: failed to store chunk {chunk_index}/{chunk_count} \
                 for msg {message_id}: {e:#}"
            );
        }
    }
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
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.remember", skip_all, fields(content_len = %content.len()))
    )]
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
                    self.effective_embed_provider(),
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

        if let Some(gate) = &self.quality_gate
            && gate
                .evaluate(content, self.effective_embed_provider(), &[])
                .await
                .is_some()
        {
            return Ok(None);
        }

        let message_id = self
            .sqlite
            .save_message(conversation_id, role, content)
            .await?;

        self.embed_and_store_regular(message_id, conversation_id, role, content);

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
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.remember", skip_all, fields(content_len = %content.len()))
    )]
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
                    self.effective_embed_provider(),
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

        if let Some(gate) = &self.quality_gate
            && gate
                .evaluate(content, self.effective_embed_provider(), &[])
                .await
                .is_some()
        {
            return Ok((None, false));
        }

        let message_id = self
            .sqlite
            .save_message_with_parts(conversation_id, role, content, parts_json)
            .await?;

        let embedding_stored =
            self.embed_and_store_regular(message_id, conversation_id, role, content);

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
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.remember", skip_all, fields(content_len = %content.len()))
    )]
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
                .evaluate(
                    content,
                    role,
                    self.effective_embed_provider(),
                    self.qdrant.as_ref(),
                    None,
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

        let embedding_stored = self.embed_chunks_with_tool_context(
            message_id,
            conversation_id,
            role,
            content,
            embed_ctx,
        );

        Ok((Some(message_id), embedding_stored))
    }

    /// Save a categorized message to `SQLite` and embed with category payload in Qdrant.
    ///
    /// The `category` is stored in both the `messages.category` column and as a Qdrant payload
    /// field for recall filtering. Uses A-MAC admission gate.
    ///
    /// Returns `Ok(Some(message_id))` when admitted; `Ok(None)` when rejected.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` save fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.remember", skip_all, fields(content_len = %content.len()))
    )]
    pub async fn remember_categorized(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        category: Option<&str>,
        goal_text: Option<&str>,
    ) -> Result<Option<MessageId>, MemoryError> {
        if let Some(ref admission) = self.admission_control {
            let decision = admission
                .evaluate(
                    content,
                    role,
                    self.effective_embed_provider(),
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
            .save_message_with_category(conversation_id, role, content, category)
            .await?;

        self.embed_and_store_with_category(message_id, conversation_id, role, content, category);

        Ok(Some(message_id))
    }

    /// Recall messages filtered by category.
    ///
    /// When `category` is `None`, behaves identically to [`Self::recall`].
    ///
    /// # Errors
    ///
    /// Returns an error if the search fails.
    pub async fn recall_with_category(
        &self,
        query: &str,
        limit: usize,
        filter: Option<SearchFilter>,
        category: Option<&str>,
    ) -> Result<Vec<RecalledMessage>, MemoryError> {
        let filter_with_category = filter.map(|mut f| {
            f.category = category.map(str::to_owned);
            f
        });
        self.recall(query, limit, filter_with_category).await
    }

    /// Reap completed background embed tasks (non-blocking).
    ///
    /// Call at turn boundaries to release handles for finished tasks.
    pub fn reap_embed_tasks(&self) {
        if let Ok(mut tasks) = self.embed_tasks.lock() {
            while tasks.try_join_next().is_some() {}
        }
    }

    /// Spawn `fut` as a bounded background embed task.
    ///
    /// If the task limit is reached, the task is dropped and a debug message is logged.
    fn spawn_embed_bg<F>(&self, fut: F) -> bool
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let Ok(mut tasks) = self.embed_tasks.lock() else {
            return false;
        };
        // Reap any finished tasks before checking capacity.
        while tasks.try_join_next().is_some() {}
        if tasks.len() >= MAX_EMBED_BG_TASKS {
            tracing::debug!("background embed task limit reached, skipping");
            return false;
        }
        tasks.spawn(fut);
        true
    }

    /// Embed content chunks and store each with an optional category payload field.
    ///
    /// Spawns a bounded background task; returns immediately.
    fn embed_and_store_with_category(
        &self,
        message_id: MessageId,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        category: Option<&str>,
    ) -> bool {
        let Some(qdrant) = self.qdrant.clone() else {
            return false;
        };
        let embed_provider = self.effective_embed_provider().clone();
        if !embed_provider.supports_embeddings() {
            return false;
        }
        self.spawn_embed_bg(embed_and_store_with_category_bg(
            EmbedBgArgs {
                qdrant,
                embed_provider,
                embedding_model: self.embedding_model.clone(),
                message_id,
                conversation_id,
                role: role.to_owned(),
                content: content.to_owned(),
                last_qdrant_warn: Arc::clone(&self.last_qdrant_warn),
            },
            category.map(str::to_owned),
        ))
    }

    /// Embed content chunks and store each as a regular (non-tool) message vector.
    ///
    /// Spawns a bounded background task; returns immediately.
    fn embed_and_store_regular(
        &self,
        message_id: MessageId,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
    ) -> bool {
        let Some(qdrant) = self.qdrant.clone() else {
            return false;
        };
        let embed_provider = self.effective_embed_provider().clone();
        if !embed_provider.supports_embeddings() {
            return false;
        }
        self.spawn_embed_bg(embed_and_store_regular_bg(EmbedBgArgs {
            qdrant,
            embed_provider,
            embedding_model: self.embedding_model.clone(),
            message_id,
            conversation_id,
            role: role.to_owned(),
            content: content.to_owned(),
            last_qdrant_warn: Arc::clone(&self.last_qdrant_warn),
        }))
    }

    /// Embed content chunks, enriching Qdrant payload with tool metadata when present.
    ///
    /// Spawns a bounded background task; returns immediately.
    fn embed_chunks_with_tool_context(
        &self,
        message_id: MessageId,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        embed_ctx: EmbedContext,
    ) -> bool {
        let Some(qdrant) = self.qdrant.clone() else {
            return false;
        };
        let embed_provider = self.effective_embed_provider().clone();
        if !embed_provider.supports_embeddings() {
            return false;
        }
        self.spawn_embed_bg(embed_chunks_with_tool_context_bg(
            EmbedBgArgs {
                qdrant,
                embed_provider,
                embedding_model: self.embedding_model.clone(),
                message_id,
                conversation_id,
                role: role.to_owned(),
                content: content.to_owned(),
                last_qdrant_warn: Arc::clone(&self.last_qdrant_warn),
            },
            embed_ctx,
        ))
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
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.recall", skip_all, fields(query_len = %query.len(), result_count = tracing::field::Empty, top_score = tracing::field::Empty))
    )]
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
            .keyword_search(query, self.effective_depth(limit), conversation_id)
            .await
        {
            Ok(results) => results,
            Err(e) => {
                tracing::warn!("FTS5 keyword search failed: {e:#}");
                Vec::new()
            }
        };

        let vector_results = if let Some(qdrant) = &self.qdrant
            && self.effective_embed_provider().supports_embeddings()
        {
            let embed_input = self.apply_search_prompt(query);
            let query_vector = self.effective_embed_provider().embed(&embed_input).await?;
            let query_vector = self.apply_query_bias(query, query_vector).await;
            let vector_size = u64::try_from(query_vector.len()).unwrap_or(896);
            qdrant.ensure_collection(vector_size).await?;
            qdrant
                .search(&query_vector, self.effective_depth(limit), filter)
                .await?
        } else {
            Vec::new()
        };

        let results = self
            .recall_merge_and_rank(keyword_results, vector_results, limit)
            .await?;
        #[cfg(feature = "profiling")]
        {
            let span = tracing::Span::current();
            span.record("result_count", results.len());
            if let Some(top) = results.first() {
                span.record("top_score", top.score);
            }
        }
        Ok(results)
    }

    pub(super) async fn recall_fts5_raw(
        &self,
        query: &str,
        limit: usize,
        conversation_id: Option<ConversationId>,
    ) -> Result<Vec<(MessageId, f64)>, MemoryError> {
        self.sqlite
            .keyword_search(query, self.effective_depth(limit), conversation_id)
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
        if !self.effective_embed_provider().supports_embeddings() {
            return Ok(Vec::new());
        }
        let embed_input = self.apply_search_prompt(query);
        let query_vector = self.effective_embed_provider().embed(&embed_input).await?;
        let query_vector = self.apply_query_bias(query, query_vector).await;
        let vector_size = u64::try_from(query_vector.len()).unwrap_or(896);
        qdrant.ensure_collection(vector_size).await?;
        qdrant
            .search(&query_vector, self.effective_depth(limit), filter)
            .await
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

        if self.temporal_decay.is_enabled() && self.temporal_decay_half_life_days > 0 {
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

        if self.mmr_reranking.is_enabled() && !vector_results.is_empty() {
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

        if self.importance_scoring.is_enabled() && !ranked.is_empty() {
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
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.recall", skip_all, fields(query_len = %query.len(), result_count = tracing::field::Empty))
    )]
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
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.recall", skip_all, fields(query_len = %query.len(), result_count = tracing::field::Empty))
    )]
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
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.recall_graph", skip_all, fields(result_count = tracing::field::Empty))
    )]
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
            self.hebbian_reinforcement.is_enabled(),
            self.hebbian_lr,
        )
        .await?;

        tracing::debug!(result_count = results.len(), "graph: recall complete");
        #[cfg(feature = "profiling")]
        tracing::Span::current().record("result_count", results.len());

        Ok(results)
    }

    /// Retrieve graph facts via SYNAPSE spreading activation.
    ///
    /// Delegates to [`crate::graph::retrieval::graph_recall_activated`].
    /// Used in place of [`Self::recall_graph`] when `spreading_activation.enabled = true`.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying graph query fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.recall_graph", skip_all, fields(result_count = tracing::field::Empty))
    )]
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
            self.hebbian_reinforcement.is_enabled(),
            self.hebbian_lr,
        )
        .await?;

        tracing::debug!(
            result_count = results.len(),
            "spreading activation: graph recall complete"
        );

        Ok(results)
    }

    /// View-aware graph recall covering both spreading-activation and BFS code paths.
    ///
    /// - When `sa_params.is_some()`: delegates to [`Self::recall_graph_activated`],
    ///   mapping each `ActivatedFact` into a `RecalledFact` with `activation_score: Some(_)`.
    /// - When `sa_params.is_none()`: delegates to [`Self::recall_graph`],
    ///   mapping each `GraphFact` into a `RecalledFact` with `activation_score: None`.
    ///
    /// View enrichment runs **after** the base retrieval step on the returned set:
    /// - `Head`: no additional I/O; output is byte-equivalent to the legacy paths.
    /// - `ZoomIn`: fetches source-message snippet for provenance (bulk SQL).
    /// - `ZoomOut`: expands 1-hop neighbors per fact (capped at `neighbor_cap`).
    ///
    /// When `view = Head` and `sa_params = None`, this function is **byte-identical** to
    /// calling `recall_graph` directly and then formatting with the assembler helper.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::MemoryError`] if the base recall or any enrichment query fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_memory::{RecallView, RecalledFact};
    ///
    /// # async fn example(mem: &zeph_memory::semantic::SemanticMemory) {
    /// let facts = mem
    ///     .recall_graph_view("tell me about Rust", 5, RecallView::Head, 3, 2, 0.0, &[], None)
    ///     .await
    ///     .unwrap_or_default();
    /// # }
    /// ```
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // single-pass enrichment pipeline: splitting would lose readability
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(
            name = "memory.recall.graph_view",
            skip_all,
            fields(view = ?view, result_count = tracing::field::Empty)
        )
    )]
    pub async fn recall_graph_view(
        &self,
        query: &str,
        limit: usize,
        view: crate::recall_view::RecallView,
        neighbor_cap: usize,
        bfs_max_hops: u32,
        temporal_decay_rate: f64,
        edge_types: &[crate::graph::EdgeType],
        sa_params: Option<crate::graph::SpreadingActivationParams>,
    ) -> Result<Vec<crate::recall_view::RecalledFact>, MemoryError> {
        use crate::recall_view::{RecallView, RecalledFact};

        // Step 1: base retrieval.
        let mut recalled: Vec<RecalledFact> = if let Some(params) = sa_params {
            let activated = self
                .recall_graph_activated(query, limit, params, edge_types)
                .await?;
            activated
                .into_iter()
                .map(|af| {
                    // ActivatedFact carries an Edge with id, fact, confidence, etc.
                    // Build a RecalledFact preserving activation score and provenance.
                    let activation_score = af.activation_score;
                    let edge = &af.edge;
                    let fact = crate::graph::types::GraphFact {
                        entity_name: String::new(), // SA does not resolve entity names; assembler formats via `edge.fact`
                        relation: edge.canonical_relation.clone(),
                        target_name: String::new(),
                        fact: edge.fact.clone(),
                        entity_match_score: activation_score,
                        hop_distance: 0,
                        confidence: edge.confidence,
                        valid_from: if edge.valid_from.is_empty() {
                            None
                        } else {
                            Some(edge.valid_from.clone())
                        },
                        edge_type: edge.edge_type,
                        retrieval_count: edge.retrieval_count,
                        edge_id: Some(edge.id),
                    };
                    RecalledFact {
                        fact,
                        activation_score: Some(activation_score),
                        provenance_message_id: edge.source_message_id,
                        provenance_snippet: None,
                        neighbors: Vec::new(),
                    }
                })
                .collect()
        } else {
            let facts = self
                .recall_graph(
                    query,
                    limit,
                    bfs_max_hops,
                    None,
                    temporal_decay_rate,
                    edge_types,
                )
                .await?;
            facts
                .into_iter()
                .map(RecalledFact::from_graph_fact)
                .collect()
        };

        // Step 2: Head view — no enrichment needed.
        if view == RecallView::Head {
            #[cfg(feature = "profiling")]
            tracing::Span::current().record("result_count", recalled.len());
            return Ok(recalled);
        }

        // Steps 3/4: Zoom-In / Zoom-Out — fetch provenance snippets.
        if matches!(view, RecallView::ZoomIn | RecallView::ZoomOut) {
            let edge_ids: Vec<i64> = recalled.iter().filter_map(|r| r.fact.edge_id).collect();

            if !edge_ids.is_empty()
                && let Some(ref store) = self.graph_store
            {
                // Bulk fetch source_message_id for all edge ids.
                const MAX_IDS: usize = 490;
                let mut edge_to_msg: std::collections::HashMap<i64, MessageId> =
                    std::collections::HashMap::new();
                for chunk in edge_ids.chunks(MAX_IDS) {
                    match store.source_message_ids_for_edges(chunk).await {
                        Ok(pairs) => {
                            for (eid, mid) in pairs {
                                edge_to_msg.insert(eid, mid);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "recall_graph_view: provenance fetch failed");
                        }
                    }
                }

                // For facts that have a source_message_id (from SA path), prefer that.
                for rf in &mut recalled {
                    if rf.provenance_message_id.is_none()
                        && let Some(eid) = rf.fact.edge_id
                    {
                        rf.provenance_message_id = edge_to_msg.get(&eid).copied();
                    }
                }

                // Bulk fetch message snippets.
                let msg_ids: Vec<MessageId> = recalled
                    .iter()
                    .filter_map(|r| r.provenance_message_id)
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();

                if !msg_ids.is_empty() {
                    match self.sqlite.messages_by_ids(&msg_ids).await {
                        Ok(messages) => {
                            let mut mid_to_snippet: std::collections::HashMap<MessageId, String> =
                                messages
                                    .into_iter()
                                    .map(|(id, msg)| {
                                        let raw = &msg.content;
                                        let scrubbed: String = raw
                                            .chars()
                                            .map(|c| match c {
                                                '\n' | '\r' | '<' | '>' => ' ',
                                                other => other,
                                            })
                                            .take(200)
                                            .collect();
                                        (id, scrubbed)
                                    })
                                    .collect();
                            for rf in &mut recalled {
                                if let Some(mid) = rf.provenance_message_id {
                                    rf.provenance_snippet = mid_to_snippet.remove(&mid);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "recall_graph_view: message snippet fetch failed");
                        }
                    }
                }
            }
        }

        // Step 5: Zoom-Out — expand 1-hop neighbors.
        if view == RecallView::ZoomOut
            && let Some(ref store) = self.graph_store
        {
            // Dedup key: use the canonical fact text when entity names are absent (SA path
            // does not resolve entity names, leaving them as empty strings, which would cause
            // all SA-path facts to collide on the ("", rel, "", type) key).
            type DedupeKey = (String, String, String, crate::graph::EdgeType);
            let make_key = |f: &crate::graph::types::GraphFact| -> DedupeKey {
                if f.entity_name.is_empty() || f.target_name.is_empty() {
                    (
                        f.fact.clone(),
                        f.relation.clone(),
                        String::new(),
                        f.edge_type,
                    )
                } else {
                    (
                        f.entity_name.clone(),
                        f.relation.clone(),
                        f.target_name.clone(),
                        f.edge_type,
                    )
                }
            };
            let mut seen: std::collections::HashSet<DedupeKey> =
                recalled.iter().map(|r| make_key(&r.fact)).collect();

            let total_neighbor_cap = limit * neighbor_cap;
            let mut total_neighbors = 0usize;

            for rf in &mut recalled {
                if total_neighbors >= total_neighbor_cap {
                    break;
                }
                // Use edge_id as seed for 1-hop BFS via the source entity.
                // We retrieve neighbors using the graph store's BFS on the source entity.
                let source_entity_id = match rf.fact.edge_id {
                    Some(eid) => match store.source_entity_id_for_edge(eid).await {
                        Ok(Some(id)) => id,
                        _ => continue,
                    },
                    None => continue,
                };

                let neighbors = match store
                    .bfs_edges_at_depth(source_entity_id, 1, edge_types)
                    .await
                {
                    Ok(edges) => edges,
                    Err(e) => {
                        tracing::warn!(error = %e, "recall_graph_view: zoom_out bfs failed");
                        continue;
                    }
                };

                let mut added = 0usize;
                for n_edge in neighbors {
                    if added >= neighbor_cap || total_neighbors >= total_neighbor_cap {
                        break;
                    }
                    let key = make_key(&n_edge.fact);
                    if seen.insert(key) {
                        rf.neighbors.push(n_edge.fact);
                        added += 1;
                        total_neighbors += 1;
                    }
                }
            }
        }

        #[cfg(feature = "profiling")]
        tracing::Span::current().record("result_count", recalled.len());
        Ok(recalled)
    }

    /// Retrieve graph facts via A* shortest-path traversal.
    ///
    /// Delegates to [`crate::graph::retrieval_astar::graph_recall_astar`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying graph query fails.
    pub async fn recall_graph_astar(
        &self,
        query: &str,
        limit: usize,
        max_hops: u32,
        temporal_decay_rate: f64,
        edge_types: &[crate::graph::EdgeType],
    ) -> Result<Vec<crate::graph::types::GraphFact>, MemoryError> {
        let Some(store) = &self.graph_store else {
            return Ok(Vec::new());
        };
        crate::graph::retrieval_astar::graph_recall_astar(
            store,
            self.qdrant.as_deref(),
            &self.provider,
            query,
            limit,
            max_hops,
            edge_types,
            temporal_decay_rate,
            self.hebbian_reinforcement.is_enabled(),
            self.hebbian_lr,
        )
        .await
    }

    /// Retrieve graph facts via `WaterCircles` concentric BFS.
    ///
    /// Delegates to [`crate::graph::retrieval_watercircles::graph_recall_watercircles`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying graph query fails.
    pub async fn recall_graph_watercircles(
        &self,
        query: &str,
        limit: usize,
        max_hops: u32,
        ring_limit: usize,
        temporal_decay_rate: f64,
        edge_types: &[crate::graph::EdgeType],
    ) -> Result<Vec<crate::graph::types::GraphFact>, MemoryError> {
        let Some(store) = &self.graph_store else {
            return Ok(Vec::new());
        };
        crate::graph::retrieval_watercircles::graph_recall_watercircles(
            store,
            self.qdrant.as_deref(),
            &self.provider,
            query,
            limit,
            max_hops,
            ring_limit,
            edge_types,
            temporal_decay_rate,
            self.hebbian_reinforcement.is_enabled(),
            self.hebbian_lr,
        )
        .await
    }

    /// Retrieve graph facts via beam search.
    ///
    /// Delegates to [`crate::graph::retrieval_beam::graph_recall_beam`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying graph query fails.
    pub async fn recall_graph_beam(
        &self,
        query: &str,
        limit: usize,
        beam_width: usize,
        max_hops: u32,
        temporal_decay_rate: f64,
        edge_types: &[crate::graph::EdgeType],
    ) -> Result<Vec<crate::graph::types::GraphFact>, MemoryError> {
        let Some(store) = &self.graph_store else {
            return Ok(Vec::new());
        };
        crate::graph::retrieval_beam::graph_recall_beam(
            store,
            self.qdrant.as_deref(),
            &self.provider,
            query,
            limit,
            beam_width,
            max_hops,
            edge_types,
            temporal_decay_rate,
            self.hebbian_reinforcement.is_enabled(),
            self.hebbian_lr,
        )
        .await
    }

    /// Classify query intent and return the strategy name for hybrid dispatch.
    ///
    /// Returns one of: `"astar"`, `"watercircles"`, `"beam_search"`, `"synapse"`.
    /// Falls back to `"synapse"` on any LLM error.
    pub async fn classify_graph_strategy(&self, query: &str) -> String {
        crate::graph::strategy_classifier::classify_retrieval_strategy(&self.provider, query).await
    }

    /// Retrieve graph facts via HL-F5 spreading activation from the top-1 ANN anchor (#3346).
    ///
    /// Returns an empty vec when no graph store is configured, Qdrant is unavailable,
    /// or `hebbian_spread.enabled = false`.  The outer 200 ms timeout ensures the
    /// agent loop is never blocked by a slow Qdrant response.
    ///
    /// # Errors
    ///
    /// Returns an error if the embed call or any database query fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(
            name = "memory.recall_graph_hela",
            skip_all,
            fields(result_count = tracing::field::Empty)
        )
    )]
    pub async fn recall_graph_hela(
        &self,
        query: &str,
        limit: usize,
        params: crate::graph::HelaSpreadParams,
    ) -> Result<Vec<crate::graph::HelaFact>, MemoryError> {
        let Some(store) = &self.graph_store else {
            return Ok(Vec::new());
        };
        let Some(embeddings) = &self.qdrant else {
            return Ok(Vec::new());
        };

        let store = Arc::clone(store);
        let embeddings = Arc::clone(embeddings);
        let provider = self.provider.clone();
        let hebbian_enabled = self.hebbian_reinforcement.is_enabled();
        let hebbian_lr = self.hebbian_lr;

        let results = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            crate::graph::hela_spreading_recall(
                &store,
                &embeddings,
                &provider,
                query,
                limit,
                &params,
                hebbian_enabled,
                hebbian_lr,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            tracing::warn!("memory.recall_graph_hela: outer 200ms timeout exceeded");
            Ok(Vec::new())
        })?;

        #[cfg(feature = "profiling")]
        tracing::Span::current().record("result_count", results.len());

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
    /// Processes unembedded messages in micro-batches of 32, using `buffer_unordered(4)` for
    /// concurrent embedding within each batch. Bounded peak memory: at most 32 messages of content
    /// plus their embedding vectors are live at any time.
    ///
    /// When `progress_tx` is `Some`, sends `Some(BackfillProgress)` after each message and
    /// `None` on completion (or on timeout/error in the caller).
    ///
    /// Returns the count of successfully embedded messages.
    ///
    /// # Errors
    ///
    /// Returns an error if collection initialization or the streaming query setup fails.
    /// Individual embedding failures are logged but do not stop processing.
    pub async fn embed_missing(
        &self,
        progress_tx: Option<tokio::sync::watch::Sender<Option<super::BackfillProgress>>>,
    ) -> Result<usize, MemoryError> {
        if self.qdrant.is_none() || !self.effective_embed_provider().supports_embeddings() {
            return Ok(0);
        }

        let total = self.sqlite.count_unembedded_messages().await?;
        if total == 0 {
            return Ok(0);
        }

        if let Some(tx) = &progress_tx {
            let _ = tx.send(Some(super::BackfillProgress { done: 0, total }));
        }

        let mut done = 0usize;
        let mut succeeded = 0usize;

        loop {
            const BATCH_SIZE: usize = 32;
            const BATCH_SIZE_I64: i64 = 32;
            let rows: Vec<_> = self
                .sqlite
                .stream_unembedded_messages(BATCH_SIZE_I64)
                .try_collect()
                .await?;

            if rows.is_empty() {
                break;
            }

            let batch_len = rows.len();

            let results: Vec<bool> = futures::stream::iter(rows)
                .map(|(msg_id, conv_id, role, content)| async move {
                    self.embed_and_store_regular(msg_id, conv_id, &role, &content)
                })
                .buffer_unordered(4)
                .collect()
                .await;

            for ok in &results {
                done += 1;
                if *ok {
                    succeeded += 1;
                }
                if let Some(tx) = &progress_tx {
                    let _ = tx.send(Some(super::BackfillProgress { done, total }));
                }
            }

            let batch_succeeded = results.iter().filter(|&&b| b).count();
            if batch_succeeded > 0 {
                tracing::debug!("Backfill batch: {batch_succeeded}/{batch_len} embedded");
            }

            if batch_len < BATCH_SIZE {
                break;
            }
        }

        if let Some(tx) = &progress_tx {
            let _ = tx.send(None);
        }

        if done > 0 {
            tracing::info!("Embedded {succeeded}/{total} missing messages");
        }
        Ok(succeeded)
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

    async fn make_semantic_memory() -> crate::semantic::SemanticMemory {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let provider = AnyProvider::Mock(MockProvider::default());
        let sqlite = crate::store::SqliteStore::new(":memory:").await.unwrap();
        crate::semantic::SemanticMemory {
            sqlite,
            qdrant: None,
            provider,
            embed_provider: None,
            embedding_model: "test-model".into(),
            vector_weight: 0.7,
            keyword_weight: 0.3,
            temporal_decay: crate::semantic::TemporalDecay::Disabled,
            temporal_decay_half_life_days: 30,
            mmr_reranking: crate::semantic::MmrReranking::Disabled,
            mmr_lambda: 0.7,
            importance_scoring: crate::semantic::ImportanceScoring::Disabled,
            importance_weight: 0.15,
            token_counter: Arc::new(crate::token_counter::TokenCounter::new()),
            graph_store: None,
            experience: None,
            community_detection_failures: Arc::new(AtomicU64::new(0)),
            graph_extraction_count: Arc::new(AtomicU64::new(0)),
            graph_extraction_failures: Arc::new(AtomicU64::new(0)),
            last_qdrant_warn: Arc::new(AtomicU64::new(0)),
            tier_boost_semantic: 1.3,
            admission_control: None,
            quality_gate: None,
            key_facts_dedup_threshold: 0.95,
            embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
            retrieval_depth: 0,
            search_prompt_template: String::new(),
            depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reasoning: None,
            query_bias_correction: crate::semantic::QueryBiasCorrection::Disabled,
            query_bias_profile_weight: 0.25,
            profile_centroid: tokio::sync::RwLock::new(None),
            profile_centroid_ttl_secs: 300,
            hebbian_reinforcement: crate::semantic::HebbianReinforcement::Disabled,
            hebbian_lr: 0.1,
            hebbian_spread: crate::HelaSpreadRuntime::default(),
            retrieval_failure_logger: None,
        }
    }

    #[tokio::test]
    async fn spawn_embed_bg_returns_true_when_capacity_available() {
        let memory = make_semantic_memory().await;
        let dispatched = memory.spawn_embed_bg(std::future::ready(()));
        assert!(
            dispatched,
            "spawn_embed_bg must return true when a task was successfully spawned"
        );
    }

    #[tokio::test]
    async fn spawn_embed_bg_returns_false_at_capacity() {
        let memory = make_semantic_memory().await;

        // Fill the JoinSet to the limit with never-completing futures.
        {
            let mut tasks = memory.embed_tasks.lock().unwrap();
            for _ in 0..MAX_EMBED_BG_TASKS {
                tasks.spawn(std::future::pending::<()>());
            }
        }

        let dispatched = memory.spawn_embed_bg(std::future::ready(()));
        assert!(
            !dispatched,
            "spawn_embed_bg must return false when the task limit is reached"
        );
    }

    #[test]
    fn qdrant_warn_rate_limit_suppresses_within_window() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let last_warn = Arc::new(AtomicU64::new(0));
        let window_secs = 10u64;

        // Simulate first call: last=0, now=100 → should emit (diff >= 10)
        let now1 = 100u64;
        let last1 = last_warn.load(Ordering::Relaxed);
        let should_warn1 = now1.saturating_sub(last1) >= window_secs;
        assert!(should_warn1, "first call must not be suppressed");
        if should_warn1 {
            last_warn.store(now1, Ordering::Relaxed);
        }

        // Simulate second call 5s later: now=105 → should be suppressed (diff < 10)
        let now2 = 105u64;
        let last2 = last_warn.load(Ordering::Relaxed);
        let should_warn2 = now2.saturating_sub(last2) >= window_secs;
        assert!(!should_warn2, "call within 10s window must be suppressed");

        // Simulate third call 10s after first: now=110 → should emit again
        let now3 = 110u64;
        let last3 = last_warn.load(Ordering::Relaxed);
        let should_warn3 = now3.saturating_sub(last3) >= window_secs;
        assert!(
            should_warn3,
            "call after window expiry must not be suppressed"
        );
    }

    #[test]
    fn qdrant_warn_rate_limit_shared_across_concurrent_sites() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        // All 3 WARN sites share one Arc<AtomicU64>. Simulate site A warning at t=100,
        // then site B attempting at t=105 — must be suppressed.
        let shared = Arc::new(AtomicU64::new(0));
        let window_secs = 10u64;

        let site_a = Arc::clone(&shared);
        let site_b = Arc::clone(&shared);

        let now_a = 100u64;
        let last_a = site_a.load(Ordering::Relaxed);
        if now_a.saturating_sub(last_a) >= window_secs {
            site_a.store(now_a, Ordering::Relaxed);
        }

        let now_b = 105u64;
        let last_b = site_b.load(Ordering::Relaxed);
        let warn_b = now_b.saturating_sub(last_b) >= window_secs;
        assert!(
            !warn_b,
            "site B must be suppressed because site A already warned within the window"
        );
    }
}
