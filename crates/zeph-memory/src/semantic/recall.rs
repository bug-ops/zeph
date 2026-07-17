// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::future::Future;
use std::pin::Pin;
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

use crate::admission::{AdmissionDecision, log_admission_decision};
use crate::embedding_store::{MessageKind, SearchFilter};
use crate::error::MemoryError;
use crate::store::admission_training::AdmissionTrainingInput;
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

/// Rate-limit window (seconds) for the "failed to ensure Qdrant collection" warning.
const QDRANT_WARN_WINDOW_SECS: u64 = 10;

/// Whether enough time has passed since the last suppressed warning to emit a new one.
fn should_emit_qdrant_warn(last: u64, now: u64, window_secs: u64) -> bool {
    now.saturating_sub(last) >= window_secs
}

/// Log a Qdrant `ensure_collection` failure, rate-limited to one WARN per
/// [`QDRANT_WARN_WINDOW_SECS`] across all background embed call sites sharing `last_warn`.
fn warn_qdrant_ensure_failure(last_warn: &AtomicU64, log_tag: &str, err: &MemoryError) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last = last_warn.load(Ordering::Relaxed);
    if should_emit_qdrant_warn(last, now, QDRANT_WARN_WINDOW_SECS) {
        last_warn.store(now, Ordering::Relaxed);
        tracing::warn!("{log_tag}: failed to ensure Qdrant collection: {err:#}");
    } else {
        tracing::debug!("{log_tag}: failed to ensure Qdrant collection (suppressed): {err:#}");
    }
}

/// Shared arguments for background embed tasks.
///
/// Deliberately slim: only what [`embed_chunk_and_store_bg`] itself needs. Per-chunk store
/// arguments (`embedding_model`, `conversation_id`, `role`, category/tool metadata) are
/// captured by the caller-supplied `store_chunk` closure instead, since they vary by variant.
struct EmbedBgArgs {
    qdrant: Arc<crate::embedding_store::EmbeddingStore>,
    embed_provider: zeph_llm::any::AnyProvider,
    message_id: MessageId,
    content: String,
    last_qdrant_warn: Arc<AtomicU64>,
}

/// Background task: embed content chunks and store each via `store_chunk`.
///
/// All errors are logged as warnings; the function never panics. Shared by
/// `embed_and_store_regular`, `embed_chunks_with_tool_context`, and
/// `embed_and_store_with_category` — the only difference between them is how each chunk
/// is stored, expressed here as a boxed-future closure to sidestep borrow-checker fights
/// over an in-loop `.await` on a stored future type.
async fn embed_chunk_and_store_bg<F>(args: EmbedBgArgs, log_tag: &'static str, store_chunk: F)
where
    F: Fn(u32, Vec<f32>) -> Pin<Box<dyn Future<Output = Result<(), MemoryError>> + Send>> + Send,
{
    let EmbedBgArgs {
        qdrant,
        embed_provider,
        message_id,
        content,
        last_qdrant_warn,
    } = args;
    let chunks = chunk_text(&content);
    let chunk_count = chunks.len();

    let vectors = match embed_provider.embed_batch(&chunks).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("{log_tag}: failed to embed chunks for msg {message_id}: {e:#}");
            return;
        }
    };

    let Some(first) = vectors.first() else {
        return;
    };
    if let Err(e) = qdrant.ensure_collection_for_vector(first).await {
        warn_qdrant_ensure_failure(&last_qdrant_warn, log_tag, &e);
        return;
    }

    for (chunk_index, vector) in vectors.into_iter().enumerate() {
        let chunk_index_u32 = u32::try_from(chunk_index).unwrap_or(u32::MAX);
        if let Err(e) = store_chunk(chunk_index_u32, vector).await {
            tracing::warn!(
                "{log_tag}: failed to store chunk {chunk_index}/{chunk_count} \
                 for msg {message_id}: {e:#}"
            );
        }
    }
}

/// Outcome of [`SemanticMemory::run_admission_gate`].
enum AdmissionOutcome {
    /// A-MAC rejected the message; the training sample was already recorded.
    Reject,
    /// A-MAC admitted the message (or no `AdmissionControl` is configured), carrying the
    /// decision onward so the caller can record the training sample once the outcome of
    /// any downstream quality gate and the `SQLite` write are known.
    Proceed(Option<AdmissionDecision>),
}

/// Compute `recall_graph_hela`'s outer hard timeout from the same [`crate::graph::HelaSpreadParams`]
/// that bound the inner call, so the outer bound can never again be tighter than what it wraps
/// (#5785). `hela_spreading_recall` checks `step_budget` once for the anchor ANN, once per BFS hop
/// (up to `spread_depth`, clamped to `[1, 6]`) for the edge-fetch, and once for the final
/// vectors-batch — `spread_depth + 2` gated stages in the worst case, not a fixed count.
fn hela_outer_timeout(params: &crate::graph::HelaSpreadParams) -> std::time::Duration {
    let embed_component = params
        .embed_timeout
        .unwrap_or(std::time::Duration::from_secs(5));
    let step_stages = params.spread_depth.clamp(1, 6) + 2;
    let step_component = params
        .step_budget
        .unwrap_or(std::time::Duration::from_millis(80))
        * step_stages;
    embed_component + step_component + std::time::Duration::from_millis(250)
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
        let admission_decision = match self
            .run_admission_gate(conversation_id, role, content, goal_text)
            .await
        {
            AdmissionOutcome::Reject => return Ok(None),
            AdmissionOutcome::Proceed(decision) => decision,
        };

        if self
            .run_quality_gate(conversation_id, role, content, admission_decision.as_ref())
            .await
        {
            return Ok(None);
        }

        let message_id = self
            .sqlite
            .save_message(conversation_id, role, content)
            .await?;

        self.record_admission_sample_opt(
            conversation_id,
            role,
            content,
            admission_decision.as_ref(),
            Some(message_id),
        )
        .await;

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
        let admission_decision = match self
            .run_admission_gate(conversation_id, role, content, goal_text)
            .await
        {
            AdmissionOutcome::Reject => return Ok((None, false)),
            AdmissionOutcome::Proceed(decision) => decision,
        };

        if self
            .run_quality_gate(conversation_id, role, content, admission_decision.as_ref())
            .await
        {
            return Ok((None, false));
        }

        let message_id = self
            .sqlite
            .save_message_with_parts(conversation_id, role, content, parts_json)
            .await?;

        self.record_admission_sample_opt(
            conversation_id,
            role,
            content,
            admission_decision.as_ref(),
            Some(message_id),
        )
        .await;

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
        // No quality gate here: tool output is not subject to the reference-completeness /
        // information-value checks applied to conversational messages.
        let admission_decision = match self
            .run_admission_gate(conversation_id, role, content, None)
            .await
        {
            AdmissionOutcome::Reject => return Ok((None, false)),
            AdmissionOutcome::Proceed(decision) => decision,
        };

        let message_id = self
            .sqlite
            .save_message_with_parts(conversation_id, role, content, parts_json)
            .await?;

        self.record_admission_sample_opt(
            conversation_id,
            role,
            content,
            admission_decision.as_ref(),
            Some(message_id),
        )
        .await;

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
        // No quality gate here: categorized writes (e.g. persona facts, structured summaries)
        // bypass the reference-completeness / information-value checks applied to `remember`.
        let admission_decision = match self
            .run_admission_gate(conversation_id, role, content, goal_text)
            .await
        {
            AdmissionOutcome::Reject => return Ok(None),
            AdmissionOutcome::Proceed(decision) => decision,
        };

        let message_id = self
            .sqlite
            .save_message_with_category(conversation_id, role, content, category)
            .await?;

        self.record_admission_sample_opt(
            conversation_id,
            role,
            content,
            admission_decision.as_ref(),
            Some(message_id),
        )
        .await;

        self.embed_and_store_with_category(message_id, conversation_id, role, content, category);

        Ok(Some(message_id))
    }

    /// Evaluate the A-MAC admission gate shared by all `remember*` variants.
    ///
    /// On rejection, records the training sample (with no `message_id`, since the message
    /// is never persisted) and returns [`AdmissionOutcome::Reject`]. When no
    /// [`crate::admission::AdmissionControl`] is configured, always proceeds with `None`.
    async fn run_admission_gate(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        goal_text: Option<&str>,
    ) -> AdmissionOutcome {
        let Some(admission) = &self.admission_control else {
            return AdmissionOutcome::Proceed(None);
        };
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
            self.record_admission_sample(conversation_id, role, content, &decision, None)
                .await;
            return AdmissionOutcome::Reject;
        }
        AdmissionOutcome::Proceed(Some(decision))
    }

    /// Evaluate the optional quality gate. Only called by [`Self::remember`] and
    /// [`Self::remember_with_parts`] — `remember_tool_output` and `remember_categorized`
    /// deliberately skip it (see their doc comments).
    ///
    /// Returns `true` when the gate rejects the content, having already recorded the
    /// training sample (with no `message_id`, since the message is never persisted).
    async fn run_quality_gate(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        admission_decision: Option<&AdmissionDecision>,
    ) -> bool {
        let Some(gate) = &self.quality_gate else {
            return false;
        };
        let recent_embeddings = self
            .fetch_recent_embeddings(conversation_id, gate.config().recent_window)
            .await;
        if gate
            .evaluate(content, self.effective_embed_provider(), &recent_embeddings)
            .await
            .is_none()
        {
            return false;
        }
        if let Some(decision) = admission_decision {
            self.record_admission_sample(conversation_id, role, content, decision, None)
                .await;
        }
        true
    }

    /// Fetch embeddings for the most recent `limit` messages in `conversation_id`, for use as
    /// the `recent_embeddings` window in [`crate::quality_gate::QualityGate::evaluate`] (#6387).
    ///
    /// Reuses the same `SqliteStore::load_history` + `EmbeddingStore::get_vectors` pair the MMR
    /// re-ranking path uses (see [`Self::recall_merge_and_rank`]) rather than a bespoke query.
    /// Called before the candidate message is persisted, so the returned window never includes it.
    ///
    /// Fails open: returns an empty vec (which makes `information_value` score as novel) when no
    /// vector store is attached, `limit == 0`, or any lookup step errors.
    async fn fetch_recent_embeddings(
        &self,
        conversation_id: ConversationId,
        limit: usize,
    ) -> Vec<Vec<f32>> {
        let Some(qdrant) = &self.qdrant else {
            return Vec::new();
        };
        if limit == 0 {
            return Vec::new();
        }
        let limit_u32 = u32::try_from(limit).unwrap_or(u32::MAX);
        let recent_messages = match self.sqlite.load_history(conversation_id, limit_u32).await {
            Ok(messages) => messages,
            Err(e) => {
                tracing::warn!("quality_gate: failed to load recent history: {e:#}");
                return Vec::new();
            }
        };
        let ids: Vec<MessageId> = recent_messages
            .iter()
            .filter_map(|m| m.metadata.db_id)
            .map(MessageId)
            .collect();
        if ids.is_empty() {
            return Vec::new();
        }
        match qdrant.get_vectors(&ids).await {
            Ok(vec_map) => vec_map.into_values().collect(),
            Err(e) => {
                tracing::warn!("quality_gate: failed to fetch recent embeddings: {e:#}");
                Vec::new()
            }
        }
    }

    /// Record the admission training sample for a message that passed every gate, when an
    /// A-MAC decision was made (no-op when `admission_control` is unconfigured).
    async fn record_admission_sample_opt(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        decision: Option<&AdmissionDecision>,
        message_id: Option<MessageId>,
    ) {
        if let Some(decision) = decision {
            self.record_admission_sample(conversation_id, role, content, decision, message_id)
                .await;
        }
    }

    /// Record an A-MAC admission decision as an RL training sample.
    ///
    /// Best-effort: failures are logged at debug level and never propagated, since training
    /// data collection must not affect the write path it observes. Records both admitted and
    /// rejected decisions so the training set avoids survivorship bias (see
    /// `crate::store::admission_training` module docs). `message_id` is `None` when the
    /// message was rejected (by A-MAC or a downstream quality gate) and never persisted.
    async fn record_admission_sample(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        decision: &AdmissionDecision,
        message_id: Option<MessageId>,
    ) {
        let features_json = match serde_json::to_string(&decision.factors) {
            Ok(json) => json,
            Err(e) => {
                tracing::debug!(error = %e, "admission training: failed to serialize factors");
                return;
            }
        };
        if let Err(e) = self
            .sqlite
            .record_admission_training(AdmissionTrainingInput {
                message_id,
                conversation_id,
                content,
                role,
                composite_score: decision.composite_score,
                was_admitted: decision.admitted,
                features_json: &features_json,
            })
            .await
        {
            tracing::debug!(error = %e, "admission training: failed to record sample (non-fatal)");
        }
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
        let store_qdrant = Arc::clone(&qdrant);
        let embedding_model = self.embedding_model.clone();
        let role = role.to_owned();
        let category = category.map(str::to_owned);
        let store_chunk =
            move |chunk_index: u32,
                  vector: Vec<f32>|
                  -> Pin<Box<dyn Future<Output = Result<(), MemoryError>> + Send>> {
                let qdrant = Arc::clone(&store_qdrant);
                let embedding_model = embedding_model.clone();
                let role = role.clone();
                let category = category.clone();
                Box::pin(async move {
                    qdrant
                        .store_with_category(
                            message_id,
                            conversation_id,
                            &role,
                            vector,
                            MessageKind::Regular,
                            &embedding_model,
                            chunk_index,
                            category.as_deref(),
                        )
                        .await
                        .map(|_| ())
                })
            };
        self.spawn_embed_bg(embed_chunk_and_store_bg(
            EmbedBgArgs {
                qdrant,
                embed_provider,
                message_id,
                content: content.to_owned(),
                last_qdrant_warn: Arc::clone(&self.last_qdrant_warn),
            },
            "bg embed_category",
            store_chunk,
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
        let store_qdrant = Arc::clone(&qdrant);
        let embedding_model = self.embedding_model.clone();
        let role = role.to_owned();
        let store_chunk =
            move |chunk_index: u32,
                  vector: Vec<f32>|
                  -> Pin<Box<dyn Future<Output = Result<(), MemoryError>> + Send>> {
                let qdrant = Arc::clone(&store_qdrant);
                let embedding_model = embedding_model.clone();
                let role = role.clone();
                Box::pin(async move {
                    qdrant
                        .store(
                            message_id,
                            conversation_id,
                            &role,
                            vector,
                            MessageKind::Regular,
                            &embedding_model,
                            chunk_index,
                        )
                        .await
                        .map(|_| ())
                })
            };
        self.spawn_embed_bg(embed_chunk_and_store_bg(
            EmbedBgArgs {
                qdrant,
                embed_provider,
                message_id,
                content: content.to_owned(),
                last_qdrant_warn: Arc::clone(&self.last_qdrant_warn),
            },
            "bg embed_regular",
            store_chunk,
        ))
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
        let store_qdrant = Arc::clone(&qdrant);
        let embedding_model = self.embedding_model.clone();
        let role = role.to_owned();
        let store_chunk =
            move |chunk_index: u32,
                  vector: Vec<f32>|
                  -> Pin<Box<dyn Future<Output = Result<(), MemoryError>> + Send>> {
                let qdrant = Arc::clone(&store_qdrant);
                let embedding_model = embedding_model.clone();
                let role = role.clone();
                let embed_ctx = embed_ctx.clone();
                Box::pin(async move {
                    if let Some(tool_name) = embed_ctx.tool_name {
                        qdrant
                            .store_with_tool_context(
                                message_id,
                                conversation_id,
                                &role,
                                vector,
                                MessageKind::Regular,
                                &embedding_model,
                                chunk_index,
                                &tool_name,
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
                                chunk_index,
                            )
                            .await
                            .map(|_| ())
                    }
                })
            };
        self.spawn_embed_bg(embed_chunk_and_store_bg(
            EmbedBgArgs {
                qdrant,
                embed_provider,
                message_id,
                content: content.to_owned(),
                last_qdrant_warn: Arc::clone(&self.last_qdrant_warn),
            },
            "bg embed_tool",
            store_chunk,
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
            let query_vector = match tokio::time::timeout(
                self.embed_timeout,
                self.effective_embed_provider().embed(&embed_input),
            )
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    tracing::warn!("recall_semantic: embed timed out, returning empty results");
                    return Ok(Vec::new());
                }
            };
            let query_vector = self.apply_query_bias(query, query_vector).await;
            qdrant.ensure_collection_for_vector(&query_vector).await?;
            qdrant
                .search(&query_vector, self.effective_depth(limit), filter)
                .await?
        } else {
            Vec::new()
        };

        let results = self
            .recall_merge_and_rank(keyword_results, vector_results, limit, None)
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

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.recall.fts5", skip_all, fields(query_len = %query.len()))
    )]
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

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.recall.vectors", skip_all, fields(query_len = %query.len()))
    )]
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
        let query_vector = match tokio::time::timeout(
            self.embed_timeout,
            self.effective_embed_provider().embed(&embed_input),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                tracing::warn!("recall_vectors_raw: embed timed out, returning empty results");
                return Ok(Vec::new());
            }
        };
        let query_vector = self.apply_query_bias(query, query_vector).await;
        qdrant.ensure_collection_for_vector(&query_vector).await?;
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
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.recall.merge_and_rank", skip_all, fields(kw_count = keyword_results.len(), vec_count = vector_results.len()))
    )]
    #[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
    pub(super) async fn recall_merge_and_rank(
        &self,
        keyword_results: Vec<(MessageId, f64)>,
        vector_results: Vec<crate::embedding_store::SearchResult>,
        limit: usize,
        goal_entity_id: Option<i64>,
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

        // Five-signal scoring (issue #4374): gated by enabled flag and non-baseline weights.
        if let Some(fs) = &self.five_signal
            && !fs.weights.is_baseline()
        {
            self.apply_five_signal_scoring(&mut ranked, fs, goal_entity_id)
                .await;
        }

        let ids: Vec<MessageId> = ranked.iter().map(|r| r.0).collect();

        // Log access events for the returned facts.
        if let Some(fs) = &self.five_signal {
            for id in &ids {
                fs.access_cache
                    .log_access(*id, "message", &fs.session_id)
                    .await;
            }
            fs.metrics.inc_recall();
        }

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

    /// Apply five-signal scoring to the ranked candidate list (issue #4374).
    ///
    /// Fetches access frequency, causal distance, and novelty signals. Access frequency
    /// and novelty require DB I/O; causal distance requires a BFS traversal (cached per
    /// goal entity). All three signals use per-candidate values — no static neutral fallback.
    async fn apply_five_signal_scoring(
        &self,
        ranked: &mut [(MessageId, f64)],
        fs: &crate::five_signal::FiveSignalRuntime,
        goal_entity_id: Option<i64>,
    ) {
        use crate::five_signal::causal_distance::CausalDistanceComputer;
        use crate::five_signal::scoring::{CandidateSignals, apply_five_signal_scoring};
        use sqlx::Row as _;

        let ids: Vec<MessageId> = ranked.iter().map(|r| r.0).collect();

        // Load per-candidate access frequency scores.
        let freq_map = match fs
            .access_cache
            .load_for_candidates(&fs.session_id, &ids)
            .await
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "five_signal: failed to load access frequencies (skipping)");
                return;
            }
        };

        // Batch-fetch `created_at` timestamps for novelty computation.
        let created_at_map: std::collections::HashMap<MessageId, i64> = {
            let id_vals: Vec<i64> = ids.iter().map(|id| id.0).collect();
            let placeholders = zeph_db::placeholder_list(1, id_vals.len());
            let created_at_epoch =
                <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::epoch_from_col("created_at");
            let sql = format!(
                "SELECT id, {created_at_epoch} AS created_at FROM messages \
                 WHERE id IN ({placeholders}) AND deleted_at IS NULL"
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for id in &id_vals {
                q = q.bind(id);
            }
            match q.fetch_all(&fs.pool).await {
                Ok(rows) => rows
                    .iter()
                    .map(|row| {
                        (
                            MessageId(row.get::<i64, _>("id")),
                            row.get::<i64, _>("created_at"),
                        )
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "five_signal: failed to fetch created_at (skipping novelty)");
                    std::collections::HashMap::new()
                }
            }
        };

        // Compute per-candidate causal distances (BFS from current goal entity).
        // FR-006: when goal_entity_id is None, compute() returns an empty map and all
        // candidates receive the neutral causal score via distance_to_score(neutral_distance).
        let causal_distance_map: std::collections::HashMap<i64, u32> = {
            let entity_ids: Vec<i64> = ids.iter().map(|id| id.0).collect();
            match fs
                .causal_computer
                .compute(goal_entity_id, &entity_ids)
                .await
            {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "five_signal: causal BFS failed (using neutral)");
                    std::collections::HashMap::new()
                }
            }
        };
        let neutral_causal_score =
            CausalDistanceComputer::distance_to_score(fs.config.neutral_causal_distance);

        let mut signals_map = std::collections::HashMap::with_capacity(ids.len());
        for &(msg_id, base_score) in ranked.iter() {
            let frequency = freq_map.get(&msg_id).copied().unwrap_or(0.0);
            // Recency and relevance are approximated from the hybrid score: since the
            // existing score blends both signals equally, half each preserves baseline ranking.
            let half = base_score / 2.0;
            let fact_created_at = created_at_map
                .get(&msg_id)
                .copied()
                .unwrap_or(fs.session_start);
            let novelty = fs.novelty_computer.compute(fact_created_at);
            let causal = causal_distance_map
                .get(&msg_id.0)
                .map_or(neutral_causal_score, |&d| {
                    CausalDistanceComputer::distance_to_score(d)
                });
            signals_map.insert(
                msg_id,
                CandidateSignals {
                    recency: half,
                    relevance: half,
                    frequency,
                    causal,
                    novelty,
                },
            );
        }

        apply_five_signal_scoring(ranked, &fs.weights, &signals_map);

        tracing::debug!(
            candidate_count = ids.len(),
            "recall: five-signal scoring applied"
        );
    }

    /// Routed search stage: dispatch to keyword-only, vector-only, or hybrid retrieval
    /// per `route`, returning the raw `(keyword, vector)` pair for the shared
    /// merge-and-rank pipeline. Shared by [`Self::recall_routed`] and
    /// [`Self::recall_routed_async`] — those differ only in how `route` is obtained
    /// (sync `MemoryRouter::route` vs async `AsyncMemoryRouter::route_async`).
    async fn recall_by_route(
        &self,
        route: crate::router::MemoryRoute,
        query: &str,
        limit: usize,
        filter: Option<crate::embedding_store::SearchFilter>,
    ) -> Result<
        (
            Vec<(crate::types::MessageId, f64)>,
            Vec<crate::embedding_store::SearchResult>,
        ),
        MemoryError,
    > {
        use crate::router::MemoryRoute;

        let conversation_id = filter.as_ref().and_then(|f| f.conversation_id);

        let results: (
            Vec<(crate::types::MessageId, f64)>,
            Vec<crate::embedding_store::SearchResult>,
        ) = match route {
            MemoryRoute::Keyword => {
                let kw = self.recall_fts5_raw(query, limit, conversation_id).await?;
                (kw, Vec::new())
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
            _ => {
                let vr = self.recall_vectors_raw(query, limit, filter).await?;
                (Vec::new(), vr)
            }
        };
        Ok(results)
    }

    /// Recall messages using query-aware routing.
    ///
    /// Delegates to FTS5-only, vector-only, or hybrid search based on the router decision,
    /// then runs the shared merge and ranking pipeline.
    ///
    /// * `goal_entity_id` — optional goal entity for causal distance scoring; when `None`, the
    ///   causal distance signal contribution is zero (FR-006).
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
        goal_entity_id: Option<i64>,
    ) -> Result<Vec<RecalledMessage>, MemoryError> {
        let route = router.route(query);
        tracing::debug!(?route, query_len = query.len(), "memory routing decision");

        let (keyword_results, vector_results) =
            self.recall_by_route(route, query, limit, filter).await?;

        tracing::debug!(
            keyword_count = keyword_results.len(),
            vector_count = vector_results.len(),
            "recall: routed search results"
        );

        self.recall_merge_and_rank(keyword_results, vector_results, limit, goal_entity_id)
            .await
    }

    /// Async variant of [`recall_routed`](Self::recall_routed) that uses
    /// [`AsyncMemoryRouter::route_async`](crate::router::AsyncMemoryRouter::route_async) when
    /// available, enabling LLM-based routing for `LlmRouter` and `HybridRouter`.
    ///
    /// Falls back to [`recall_routed`](Self::recall_routed) for routers that only implement
    /// the sync `MemoryRouter` trait (e.g. `HeuristicRouter`).
    ///
    /// * `goal_entity_id` — optional goal entity for causal distance scoring; when `None`, the
    ///   causal distance signal contribution is zero (FR-006).
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
        goal_entity_id: Option<i64>,
    ) -> Result<Vec<RecalledMessage>, MemoryError> {
        let decision = router.route_async(query).await;
        let route = decision.route;
        tracing::debug!(
            ?route,
            confidence = decision.confidence,
            query_len = query.len(),
            "memory routing decision (async)"
        );

        let (keyword_results, vector_results) =
            self.recall_by_route(route, query, limit, filter).await?;

        tracing::debug!(
            keyword_count = keyword_results.len(),
            vector_count = vector_results.len(),
            "recall: routed search results (async)"
        );

        self.recall_merge_and_rank(keyword_results, vector_results, limit, goal_entity_id)
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
            self.embed_timeout,
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
            self.embed_timeout,
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
            self.query_sensitive_cost,
            self.embed_timeout,
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
            self.embed_timeout,
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
            self.embed_timeout,
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
    /// or `hebbian_spread.enabled = false`. The outer timeout is derived from `params`
    /// (embed timeout + `(spread_depth.clamp(1, 6) + 2)` × step budget + a fixed margin) so
    /// it always stays strictly larger than the inner timeouts it wraps — a hardcoded outer
    /// bound tighter than the inner `embed_timeout` default silently aborted every call
    /// (#5785). This still ensures the agent loop is never blocked indefinitely by a stalled
    /// Qdrant response.
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

        // Single source of truth for the outer bound: see `hela_outer_timeout` — it must
        // exceed everything it wraps (the embed call plus every step-budget-gated stage,
        // scaled by `spread_depth`), or the outer timeout fires before the inner ones ever
        // get a chance to run (#5785).
        let outer_timeout = hela_outer_timeout(&params);

        let results = tokio::time::timeout(
            outer_timeout,
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
            tracing::warn!(
                outer_timeout_ms = outer_timeout.as_millis(),
                "memory.recall_graph_hela: outer timeout exceeded"
            );
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

    /// #5785 edge case: the outer timeout multiplier must scale with `spread_depth`, not stay
    /// fixed at 3 — `hela_spreading_recall` gates `spread_depth + 2` stages (anchor ANN + one
    /// edge-fetch check per BFS hop + vectors-batch), so a fixed 3× multiplier under-provisions
    /// the outer bound for any `spread_depth > 1` and can reintroduce the outer/inner inversion.
    #[test]
    fn hela_outer_timeout_scales_with_spread_depth() {
        let step_budget = std::time::Duration::from_millis(80);
        let embed_timeout = std::time::Duration::from_secs(5);
        let margin = std::time::Duration::from_millis(250);

        for depth in 1..=6u32 {
            let params = crate::graph::HelaSpreadParams {
                spread_depth: depth,
                step_budget: Some(step_budget),
                embed_timeout: Some(embed_timeout),
                ..Default::default()
            };
            let expected = embed_timeout + step_budget * (depth + 2) + margin;
            assert_eq!(
                hela_outer_timeout(&params),
                expected,
                "outer timeout must scale with spread_depth={depth}"
            );
        }
    }

    /// `spread_depth` above the algorithm's own `[1, 6]` clamp must not blow up the outer
    /// timeout unboundedly — the formula clamps identically to `hela_spreading_recall`'s own
    /// `spread_depth.clamp(1, 6)`.
    #[test]
    fn hela_outer_timeout_clamps_spread_depth_above_six() {
        let step_budget = std::time::Duration::from_millis(80);
        let embed_timeout = std::time::Duration::from_secs(5);
        let params_over = crate::graph::HelaSpreadParams {
            spread_depth: 50,
            step_budget: Some(step_budget),
            embed_timeout: Some(embed_timeout),
            ..Default::default()
        };
        let params_clamped = crate::graph::HelaSpreadParams {
            spread_depth: 6,
            step_budget: Some(step_budget),
            embed_timeout: Some(embed_timeout),
            ..Default::default()
        };
        assert_eq!(
            hela_outer_timeout(&params_over),
            hela_outer_timeout(&params_clamped),
            "spread_depth above 6 must clamp identically to the algorithm's own [1, 6] bound"
        );
    }

    /// When `step_budget`/`embed_timeout` are `None` (disabled), the outer timeout must fall
    /// back to safe finite defaults rather than becoming unbounded — the outer bound is a hard
    /// safety net independent of whether the caller opted out of the inner per-step guards.
    #[test]
    fn hela_outer_timeout_falls_back_when_params_disabled() {
        let params = crate::graph::HelaSpreadParams {
            spread_depth: 2,
            step_budget: None,
            embed_timeout: None,
            ..Default::default()
        };
        let expected = std::time::Duration::from_secs(5)
            + std::time::Duration::from_millis(80) * 4
            + std::time::Duration::from_millis(250);
        assert_eq!(hela_outer_timeout(&params), expected);
    }

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
        let sqlite = crate::store::SqliteStore::new(":memory:").await.unwrap();
        make_semantic_memory_with_sqlite(sqlite)
    }

    /// Build a `SemanticMemory` around a caller-supplied store.
    ///
    /// Split out of [`make_semantic_memory`] so tests that need a real `PostgreSQL`-backed
    /// pool (e.g. `apply_five_signal_scoring_decodes_created_at_on_postgres`) can supply one
    /// directly instead of going through `SqliteStore::new(":memory:")`, which always routes
    /// through `ActiveDriver` and fails to parse `:memory:` as a Postgres URL once the
    /// `postgres` feature is active.
    fn make_semantic_memory_with_sqlite(
        sqlite: crate::store::SqliteStore,
    ) -> crate::semantic::SemanticMemory {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let provider = AnyProvider::Mock(MockProvider::default());
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
            summarization_llm_timeout_secs: 60,
            query_sensitive_cost: false,
            five_signal: None,
            embed_timeout: std::time::Duration::from_secs(5),
            graph_cancel: std::sync::Mutex::new(Vec::new()),
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
        // First call: last=0, now=100 → should emit (diff >= 10)
        assert!(
            should_emit_qdrant_warn(0, 100, 10),
            "first call must not be suppressed"
        );

        // Second call 5s later: now=105, last=100 → should be suppressed (diff < 10)
        assert!(
            !should_emit_qdrant_warn(100, 105, 10),
            "call within 10s window must be suppressed"
        );

        // Third call 10s after first: now=110, last=100 → should emit again
        assert!(
            should_emit_qdrant_warn(100, 110, 10),
            "call after window expiry must not be suppressed"
        );
    }

    /// Regression test for issue #5364: `apply_five_signal_scoring`'s `created_at` batch
    /// fetch built its `IN (...)` list correctly via `placeholder_list`, but decoded the
    /// `created_at` column directly as `i64` — which fails on `PostgreSQL`, where
    /// `messages.created_at` is `TIMESTAMPTZ`, not an integer. Fixed by wrapping the
    /// column in the dialect's `epoch_from_col` (the same helper already used by
    /// `graph_store::decay_edge_retrieval_counts` and `snapshot::export_snapshot`).
    ///
    /// `apply_five_signal_scoring` does not read `self` — only the `fs` parameter and
    /// `ranked` — so a plain in-memory `self` receiver is fine; only `fs` needs the real
    /// PostgreSQL-backed pool that the `created_at` query actually executes against.
    #[cfg(feature = "test-utils")]
    #[tokio::test]
    #[ignore = "requires Docker"]
    async fn apply_five_signal_scoring_decodes_created_at_on_postgres() {
        use std::sync::Arc;
        use testcontainers::runners::AsyncRunner as _;
        use testcontainers_modules::postgres::Postgres;
        use zeph_config::memory::FiveSignalConfig;

        let image = Postgres::default();
        let container = image.start().await.expect("docker must be available");
        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let pool = zeph_db::DbConfig { url, pool_size: 5 }
            .connect()
            .await
            .expect("failed to connect to PG");

        let pg_store = crate::store::SqliteStore::from_pool(pool.clone())
            .await
            .unwrap();
        let cid = pg_store.create_conversation().await.unwrap();

        // Session starts at a fixed epoch; one message is "fresh" (created at session
        // start, novelty ~= 1.0), one is "stale" (created 20 days later, novelty << 1.0
        // at decay_rate=0.1) — isolates the novelty signal so the score difference is
        // attributable only to the created_at value actually fetched from Postgres.
        let session_start = 1_700_000_000_i64;
        let fresh = pg_store.save_message(cid, "user", "fresh").await.unwrap();
        let stale = pg_store.save_message(cid, "user", "stale").await.unwrap();

        for (id, epoch) in [(fresh, session_start), (stale, session_start + 20 * 86_400)] {
            #[expect(clippy::cast_precision_loss)]
            let epoch_f = epoch as f64;
            sqlx::query(zeph_db::sql!(
                "UPDATE messages SET created_at = to_timestamp(?) WHERE id = ?"
            ))
            .bind(epoch_f)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }

        let graph_store = Arc::new(crate::graph::GraphStore::new(pool.clone()));
        let config = FiveSignalConfig {
            w_recency: 0.0,
            w_relevance: 0.0,
            w_frequency: 0.0,
            w_causal: 0.0,
            w_novelty: 1.0,
            ..FiveSignalConfig::default()
        };
        let fs = crate::five_signal::FiveSignalRuntime::new(
            config,
            pool,
            graph_store,
            None,
            session_start,
            "sess-novelty-test",
        );

        // `make_semantic_memory()` cannot be used here: it calls `SqliteStore::new(":memory:")`,
        // which under the `postgres` feature routes through `ActiveDriver = PostgresDriver` and
        // fails trying to parse `:memory:` as a Postgres URL. `apply_five_signal_scoring` does
        // not read `self`, so any valid receiver works — build it directly from the same
        // Postgres-backed `pg_store` used above instead.
        let memory = make_semantic_memory_with_sqlite(pg_store);
        let mut ranked = vec![(fresh, 0.0), (stale, 0.0)];
        memory
            .apply_five_signal_scoring(&mut ranked, &fs, None)
            .await;

        assert_eq!(
            ranked[0].0, fresh,
            "fresher message (created_at == session_start) must rank first by novelty"
        );
        assert!(
            (ranked[0].1 - 1.0).abs() < 1e-6,
            "message created at session_start must have novelty ~1.0, got {}",
            ranked[0].1
        );
        assert!(
            ranked[1].1 < ranked[0].1,
            "message created 20 days later must have strictly lower novelty"
        );
    }

    #[test]
    fn qdrant_warn_rate_limit_shared_across_concurrent_sites() {
        // All 3 WARN sites (bg embed_regular/embed_tool/embed_category) share one
        // Arc<AtomicU64> via `SemanticMemory::last_qdrant_warn`. Simulate site A warning
        // at t=100, then site B attempting at t=105 — must be suppressed, mirroring the
        // exact check `warn_qdrant_ensure_failure` performs against the shared atomic.
        let shared = Arc::new(AtomicU64::new(0));

        let site_a = Arc::clone(&shared);
        let site_b = Arc::clone(&shared);

        let now_a = 100u64;
        let last_a = site_a.load(Ordering::Relaxed);
        if should_emit_qdrant_warn(last_a, now_a, QDRANT_WARN_WINDOW_SECS) {
            site_a.store(now_a, Ordering::Relaxed);
        }

        let now_b = 105u64;
        let last_b = site_b.load(Ordering::Relaxed);
        let warn_b = should_emit_qdrant_warn(last_b, now_b, QDRANT_WARN_WINDOW_SECS);
        assert!(
            !warn_b,
            "site B must be suppressed because site A already warned within the window"
        );
    }

    #[test]
    fn warn_qdrant_ensure_failure_updates_shared_atomic_once() {
        let shared = Arc::new(AtomicU64::new(0));
        let err = MemoryError::InvalidInput("boom".into());

        warn_qdrant_ensure_failure(&shared, "site A", &err);
        let after_first = shared.load(Ordering::Relaxed);
        assert!(after_first > 0, "first call must record a warn timestamp");

        // Immediately calling again (same instant, well within the window) must not
        // move the stored timestamp forward, since the second call is suppressed.
        warn_qdrant_ensure_failure(&shared, "site B", &err);
        assert_eq!(
            shared.load(Ordering::Relaxed),
            after_first,
            "suppressed call must not overwrite the shared warn timestamp"
        );
    }
}
