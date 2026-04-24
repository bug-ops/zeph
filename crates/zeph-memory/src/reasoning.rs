// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `ReasoningBank`: distilled reasoning strategy memory (#3342).
//!
//! After each completed agent turn a three-stage async pipeline runs off the hot path:
//!
//! 1. **Self-judge** ([`run_self_judge`]) — a fast LLM evaluates success/failure and
//!    extracts the key reasoning steps.
//! 2. **Distillation** ([`distill_strategy`]) — a strategy summary (≤ 3 sentences) is
//!    generated from the reasoning chain, capturing the transferable principle.
//! 3. **Storage** ([`ReasoningMemory::insert`]) — the summary is written to `SQLite`
//!    and, when Qdrant is available, embedded and indexed for vector retrieval.
//!
//! At context-build time [`ReasoningMemory::retrieve_by_embedding`] fetches top-k
//! strategies by embedding similarity. The caller (in `zeph-context`) calls
//! [`ReasoningMemory::mark_used`] only for strategies actually injected into the prompt,
//! after budget truncation (C4 split from architect plan).
//!
//! # LRU eviction
//!
//! [`ReasoningMemory::evict_lru`] protects rows with `use_count > HOT_STRATEGY_USE_COUNT`
//! (default 10) from normal eviction. When all rows are hot and the table exceeds
//! `2 × store_limit`, a forced eviction pass deletes the oldest rows unconditionally
//! and emits a `warn!` so operators can tune `store_limit` upward.
//!
//! # LRU eviction race note
//!
//! Two concurrent turns may race on the count check in `evict_lru`. Either both evict
//! (over-eviction by at most `top_k` rows) or neither. This is acceptable for MVP —
//! the table remains bounded.

use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::timeout;
use zeph_db::{ActiveDialect, DbPool, placeholder_list};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, Role};

use crate::error::MemoryError;
use crate::vector_store::VectorStore;

/// Minimum retrieval count to protect a strategy from normal LRU eviction.
///
/// Strategies with `use_count > HOT_STRATEGY_USE_COUNT` are skipped during normal
/// cold-eviction and only removed when the table exceeds `2 × store_limit`.
const HOT_STRATEGY_USE_COUNT: i64 = 10;

/// Maximum ids per `SQLite` `WHERE id IN (...)` bind list (`SQLite` variable limit is 999).
const MAX_IDS_PER_QUERY: usize = 490;

/// System prompt for the self-judge LLM step.
///
/// Instructs the LLM to evaluate success/failure and extract the reasoning chain
/// as structured JSON matching [`SelfJudgeOutcome`].
const SELF_JUDGE_SYSTEM: &str = "\
You are a task outcome evaluator. Given an agent turn transcript, analyze the conversation and determine:
1. Did the agent successfully complete the user's request? (true/false)
2. Extract the key reasoning steps the agent took (reasoning chain).
3. Summarize the task in one sentence (task hint).

Respond ONLY with valid JSON, no markdown fences, no prose:
{\"success\": bool, \"reasoning_chain\": \"string\", \"task_hint\": \"string\"}";

/// System prompt for the distillation LLM step.
///
/// Instructs the LLM to compress a reasoning chain into a short, generalizable strategy.
const DISTILL_SYSTEM: &str = "\
You are a strategy distiller. Given a reasoning chain from an agent turn, distill it into \
a short generalizable strategy (at most 3 sentences) that could help an agent facing a similar \
task. Focus on the transferable principle, not the specific instance. \
Respond with the strategy text only — no headers, no lists, no markdown.";

/// Outcome of a reasoning strategy: whether the agent succeeded or failed.
///
/// Stored as a `TEXT NOT NULL` column (`"success"` or `"failure"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The agent successfully completed the task.
    Success,
    /// The agent failed to complete the task.
    Failure,
}

impl Outcome {
    /// Returns the canonical string representation stored in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
        }
    }
}

/// Error returned when parsing an [`Outcome`] from a string fails.
#[derive(Debug, thiserror::Error)]
#[error("unknown outcome: {0}")]
pub struct OutcomeParseError(String);

impl FromStr for Outcome {
    type Err = OutcomeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "success" => Ok(Outcome::Success),
            "failure" => Ok(Outcome::Failure),
            other => {
                tracing::warn!(
                    value = other,
                    "reasoning: unknown outcome, defaulting to Failure"
                );
                Ok(Outcome::Failure)
            }
        }
    }
}

/// A distilled reasoning strategy row from the `reasoning_strategies` table.
///
/// Constructed after a successful self-judge + distillation pipeline run.
/// Persisted in `SQLite` and (when Qdrant is available) indexed as a vector embedding.
#[derive(Debug, Clone)]
pub struct ReasoningStrategy {
    /// UUID v4 primary key.
    pub id: String,
    /// Distilled strategy summary (≤ 3 sentences, ≤ 512 chars).
    pub summary: String,
    /// Whether the agent succeeded or failed on the source turn.
    pub outcome: Outcome,
    /// One-sentence description of the task that produced this strategy.
    pub task_hint: String,
    /// Unix timestamp (seconds) when this strategy was created.
    pub created_at: i64,
    /// Unix timestamp (seconds) of the last retrieval.
    pub last_used_at: i64,
    /// Number of times this strategy has been injected into context.
    pub use_count: i64,
    /// Unix timestamp (seconds) when the Qdrant embedding was created.
    ///
    /// `None` means this row has not been embedded yet (Qdrant was unavailable at insert time).
    pub embedded_at: Option<i64>,
}

/// Parsed response from the self-judge LLM call.
///
/// Deserialized from the LLM JSON response in [`run_self_judge`].
/// The `success` field drives [`Outcome`] selection; `reasoning_chain` and `task_hint`
/// are forwarded to the distillation step.
#[derive(Debug, Deserialize)]
pub struct SelfJudgeOutcome {
    /// Whether the agent successfully completed the task.
    pub success: bool,
    /// Key reasoning steps the agent took, as free-form text.
    pub reasoning_chain: String,
    /// One-sentence summary of the task.
    pub task_hint: String,
}

/// SQLite-backed store for distilled reasoning strategies.
///
/// Attach to [`crate::semantic::SemanticMemory`] via `with_reasoning`.
/// All write operations are best-effort: `SQLite` errors are propagated as
/// [`MemoryError`], Qdrant failures are logged and silently ignored.
pub struct ReasoningMemory {
    pool: DbPool,
    /// Optional vector store for embedding-similarity retrieval.
    ///
    /// `None` when Qdrant is unavailable; falls back to returning empty results.
    vector_store: Option<std::sync::Arc<dyn VectorStore>>,
}

/// Qdrant collection name used for reasoning-strategy embeddings.
pub const REASONING_COLLECTION: &str = "reasoning_strategies";

impl ReasoningMemory {
    /// Create a new `ReasoningMemory` backed by the given `SQLite` pool.
    ///
    /// Pass `vector_store = Some(arc)` to enable embedding-similarity retrieval via Qdrant.
    /// When `None`, [`Self::retrieve_by_embedding`] always returns an empty vec.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_memory::reasoning::ReasoningMemory;
    ///
    /// async fn demo(pool: zeph_db::DbPool) {
    ///     let memory = ReasoningMemory::new(pool, None);
    /// }
    /// ```
    #[must_use]
    pub fn new(pool: DbPool, vector_store: Option<std::sync::Arc<dyn VectorStore>>) -> Self {
        Self { pool, vector_store }
    }

    /// Insert a new strategy into `SQLite`.
    ///
    /// When a `vector_store` is configured, the strategy is also upserted into
    /// the Qdrant `reasoning_strategies` collection using the provided `embedding`.
    /// Qdrant failures are logged at `warn` level and do not fail the insert.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` insert fails.
    #[tracing::instrument(name = "memory.reasoning.insert", skip(self, embedding), fields(id = %strategy.id))]
    pub async fn insert(
        &self,
        strategy: &ReasoningStrategy,
        embedding: Vec<f32>,
    ) -> Result<(), MemoryError> {
        let epoch_now = <ActiveDialect as zeph_db::dialect::Dialect>::EPOCH_NOW;
        let raw = format!(
            "INSERT OR REPLACE INTO reasoning_strategies \
             (id, summary, outcome, task_hint, created_at, last_used_at, use_count, embedded_at) \
             VALUES (?, ?, ?, ?, {epoch_now}, {epoch_now}, 0, NULL)"
        );
        let sql = zeph_db::rewrite_placeholders(&raw);
        zeph_db::query(&sql)
            .bind(&strategy.id)
            .bind(&strategy.summary)
            .bind(strategy.outcome.as_str())
            .bind(&strategy.task_hint)
            .execute(&self.pool)
            .await?;

        // Qdrant upsert — best effort: SQLite row already written.
        if let Some(ref vs) = self.vector_store {
            let point = crate::vector_store::VectorPoint {
                id: strategy.id.clone(),
                vector: embedding,
                payload: std::collections::HashMap::from([
                    (
                        "outcome".to_owned(),
                        serde_json::Value::String(strategy.outcome.as_str().to_owned()),
                    ),
                    (
                        "task_hint".to_owned(),
                        serde_json::Value::String(strategy.task_hint.clone()),
                    ),
                ]),
            };
            if let Err(e) = vs.upsert(REASONING_COLLECTION, vec![point]).await {
                tracing::warn!(error = %e, id = %strategy.id, "reasoning: Qdrant upsert failed — SQLite-only mode");
            } else {
                // Mark embedded_at on success.
                let update_sql = zeph_db::rewrite_placeholders(&format!(
                    "UPDATE reasoning_strategies SET embedded_at = {epoch_now} WHERE id = ?"
                ));
                if let Err(e) = zeph_db::query(&update_sql)
                    .bind(&strategy.id)
                    .execute(&self.pool)
                    .await
                {
                    tracing::warn!(error = %e, "reasoning: failed to set embedded_at");
                }
            }
        }

        tracing::debug!(id = %strategy.id, outcome = strategy.outcome.as_str(), "reasoning: strategy inserted");
        Ok(())
    }

    /// Retrieve up to `top_k` strategies by embedding similarity.
    ///
    /// This method is **pure** — it does not update `use_count` or `last_used_at`.
    /// Call [`Self::mark_used`] with the ids of strategies actually injected into the
    /// prompt (after budget truncation) to maintain accurate retrieval bookkeeping.
    ///
    /// Returns an empty vec when no vector store is configured.
    ///
    /// # Errors
    ///
    /// Returns an error if the Qdrant search or `SQLite` fetch fails.
    #[tracing::instrument(
        name = "memory.reasoning.retrieve_by_embedding",
        skip(self, embedding),
        fields(top_k)
    )]
    pub async fn retrieve_by_embedding(
        &self,
        embedding: &[f32],
        top_k: u64,
    ) -> Result<Vec<ReasoningStrategy>, MemoryError> {
        let Some(ref vs) = self.vector_store else {
            return Ok(Vec::new());
        };

        let scored = vs
            .search(REASONING_COLLECTION, embedding.to_vec(), top_k, None)
            .await?;

        if scored.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<String> = scored.into_iter().map(|p| p.id).collect();
        self.fetch_by_ids(&ids).await
    }

    /// Increment `use_count` and update `last_used_at` for each id in the list.
    ///
    /// Safe to call with an empty slice — no SQL is issued.
    /// The list is chunked into batches of [`MAX_IDS_PER_QUERY`] to respect `SQLite`'s
    /// variable limit.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    #[tracing::instrument(name = "memory.reasoning.mark_used", skip(self), fields(n = ids.len()))]
    pub async fn mark_used(&self, ids: &[String]) -> Result<(), MemoryError> {
        if ids.is_empty() {
            return Ok(());
        }

        let epoch_now = <ActiveDialect as zeph_db::dialect::Dialect>::EPOCH_NOW;
        for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
            let ph = placeholder_list(1, chunk.len());
            // Note: placeholder_list already generates ?1,?2,... (SQLite) or $1,$2,... (postgres).
            // Do NOT call rewrite_placeholders here — that would corrupt ?1 into $11.
            let sql = format!(
                "UPDATE reasoning_strategies \
                 SET use_count = use_count + 1, last_used_at = {epoch_now} \
                 WHERE id IN ({ph})"
            );
            let mut q = zeph_db::query(&sql);
            for id in chunk {
                q = q.bind(id.as_str());
            }
            q.execute(&self.pool).await?;
        }

        Ok(())
    }

    /// Evict strategies when the table exceeds `store_limit`.
    ///
    /// **Normal path**: delete rows with `use_count <= HOT_STRATEGY_USE_COUNT`, oldest
    /// first, until the table returns to `store_limit`.
    ///
    /// **Saturation path**: when the normal path deletes nothing AND the table exceeds
    /// `2 × store_limit`, bypass hot-row protection and delete oldest rows regardless of
    /// `use_count`. Emits a `warn!` with the eviction count so operators can tune
    /// `store_limit` upward or lower the hot threshold.
    ///
    /// Returns the number of rows deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if any database operation fails.
    #[tracing::instrument(name = "memory.reasoning.evict_lru", skip(self), fields(store_limit))]
    pub async fn evict_lru(&self, store_limit: usize) -> Result<usize, MemoryError> {
        let count = self.count().await?;
        if count <= store_limit {
            return Ok(0);
        }

        let over_by = count - store_limit;
        let deleted_cold = self.delete_oldest_cold(over_by).await?;
        if deleted_cold > 0 {
            // Also delete from Qdrant best-effort (ids not tracked here — full resync on recovery).
            tracing::debug!(
                deleted = deleted_cold,
                count,
                "reasoning: evicted cold strategies"
            );
            return Ok(deleted_cold);
        }

        // All rows over limit are hot. Check hard ceiling.
        let hard_ceiling = store_limit.saturating_mul(2);
        if count <= hard_ceiling {
            tracing::debug!(
                count,
                store_limit,
                "reasoning: hot saturation — growth allowed under 2x ceiling"
            );
            return Ok(0);
        }

        // Hard ceiling breached: force-evict oldest rows unconditionally.
        let forced = count - store_limit;
        let deleted_forced = self.delete_oldest_unconditional(forced).await?;
        tracing::warn!(
            deleted = deleted_forced,
            count,
            hard_ceiling,
            "reasoning: hard-ceiling eviction — evicted hot strategies; consider raising store_limit"
        );

        Ok(deleted_forced)
    }

    /// Return the total number of rows in `reasoning_strategies`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn count(&self) -> Result<usize, MemoryError> {
        let row: (i64,) = zeph_db::query_as("SELECT COUNT(*) FROM reasoning_strategies")
            .fetch_one(&self.pool)
            .await?;
        Ok(usize::try_from(row.0.max(0)).unwrap_or(0))
    }

    // ── private helpers ───────────────────────────────────────────────────────

    /// Fetch strategy rows by their ids in a single `WHERE id IN (...)` query.
    pub(crate) async fn fetch_by_ids(
        &self,
        ids: &[String],
    ) -> Result<Vec<ReasoningStrategy>, MemoryError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut strategies = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(MAX_IDS_PER_QUERY) {
            let ph = placeholder_list(1, chunk.len());
            // Note: placeholder_list generates DB-specific ?N/$N syntax — do NOT rewite.
            let sql = format!(
                "SELECT id, summary, outcome, task_hint, created_at, last_used_at, use_count, embedded_at \
                 FROM reasoning_strategies WHERE id IN ({ph})"
            );
            let mut q = zeph_db::query_as::<
                _,
                (String, String, String, String, i64, i64, i64, Option<i64>),
            >(&sql);
            for id in chunk {
                q = q.bind(id.as_str());
            }
            let rows = q.fetch_all(&self.pool).await?;
            for (
                id,
                summary,
                outcome_str,
                task_hint,
                created_at,
                last_used_at,
                use_count,
                embedded_at,
            ) in rows
            {
                let outcome = Outcome::from_str(&outcome_str).unwrap_or(Outcome::Failure);
                strategies.push(ReasoningStrategy {
                    id,
                    summary,
                    outcome,
                    task_hint,
                    created_at,
                    last_used_at,
                    use_count,
                    embedded_at,
                });
            }
        }

        Ok(strategies)
    }

    /// Delete up to `n` cold rows (`use_count <= HOT_STRATEGY_USE_COUNT`), oldest first.
    ///
    /// Returns the number of deleted rows.
    async fn delete_oldest_cold(&self, n: usize) -> Result<usize, MemoryError> {
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        // Use plain `?` + rewrite_placeholders so postgres gets `$1`.
        let raw = format!(
            "DELETE FROM reasoning_strategies \
             WHERE id IN ( \
               SELECT id FROM reasoning_strategies \
               WHERE use_count <= {HOT_STRATEGY_USE_COUNT} \
               ORDER BY last_used_at ASC LIMIT ? \
             )"
        );
        let sql = zeph_db::rewrite_placeholders(&raw);
        let result = zeph_db::query(&sql).bind(limit).execute(&self.pool).await?;
        Ok(usize::try_from(result.rows_affected()).unwrap_or(0))
    }

    /// Delete up to `n` rows unconditionally (oldest by `last_used_at`).
    ///
    /// Used only for the hard-ceiling saturation path.
    async fn delete_oldest_unconditional(&self, n: usize) -> Result<usize, MemoryError> {
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        let raw = "DELETE FROM reasoning_strategies \
                   WHERE id IN ( \
                     SELECT id FROM reasoning_strategies \
                     ORDER BY last_used_at ASC LIMIT ? \
                   )";
        let sql = zeph_db::rewrite_placeholders(raw);
        let result = zeph_db::query(&sql).bind(limit).execute(&self.pool).await?;
        Ok(usize::try_from(result.rows_affected()).unwrap_or(0))
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

/// Run the self-judge step against a turn's message tail.
///
/// Sends the last `messages` slice to the LLM with the self-judge system prompt and
/// attempts to parse the JSON response into a [`SelfJudgeOutcome`].
///
/// Returns `None` on parse failure, timeout, or LLM error — never propagates errors.
/// Callers should log the `None` case at most at `debug` level.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use zeph_llm::any::AnyProvider;
/// use zeph_memory::reasoning::run_self_judge;
///
/// async fn demo(provider: AnyProvider, messages: &[zeph_llm::provider::Message]) {
///     let outcome = run_self_judge(&provider, messages, Duration::from_secs(10)).await;
///     if let Some(o) = outcome {
///         println!("success={}, hint={}", o.success, o.task_hint);
///     }
/// }
/// ```
#[tracing::instrument(name = "memory.reasoning.self_judge", skip(provider, messages), fields(n = messages.len()))]
pub async fn run_self_judge(
    provider: &AnyProvider,
    messages: &[Message],
    extraction_timeout: Duration,
) -> Option<SelfJudgeOutcome> {
    if messages.is_empty() {
        return None;
    }

    let user_prompt = build_transcript_prompt(messages);

    let llm_messages = [
        Message::from_legacy(Role::System, SELF_JUDGE_SYSTEM),
        Message::from_legacy(Role::User, user_prompt),
    ];

    let response = match timeout(extraction_timeout, provider.chat(&llm_messages)).await {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "reasoning: self-judge LLM call failed");
            return None;
        }
        Err(_) => {
            tracing::warn!("reasoning: self-judge timed out");
            return None;
        }
    };

    parse_self_judge_response(&response)
}

/// Run the distillation step.
///
/// Sends the reasoning chain and outcome label to the LLM and trims the response to
/// at most 3 sentences and 512 characters.
///
/// Returns `None` on LLM error, timeout, or empty response.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use zeph_llm::any::AnyProvider;
/// use zeph_memory::reasoning::{Outcome, distill_strategy};
///
/// async fn demo(provider: AnyProvider) {
///     let summary = distill_strategy(&provider, Outcome::Success, "tried X, worked", Duration::from_secs(10)).await;
///     println!("{:?}", summary);
/// }
/// ```
#[tracing::instrument(name = "memory.reasoning.distill", skip(provider, reasoning_chain))]
pub async fn distill_strategy(
    provider: &AnyProvider,
    outcome: Outcome,
    reasoning_chain: &str,
    distill_timeout: Duration,
) -> Option<String> {
    if reasoning_chain.is_empty() {
        return None;
    }

    let user_prompt = format!(
        "Outcome: {}\n\nReasoning chain:\n{reasoning_chain}",
        outcome.as_str()
    );

    let llm_messages = [
        Message::from_legacy(Role::System, DISTILL_SYSTEM),
        Message::from_legacy(Role::User, user_prompt),
    ];

    let response = match timeout(distill_timeout, provider.chat(&llm_messages)).await {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "reasoning: distillation LLM call failed");
            return None;
        }
        Err(_) => {
            tracing::warn!("reasoning: distillation timed out");
            return None;
        }
    };

    let trimmed = trim_to_three_sentences(&response);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Configuration for the [`process_turn`] extraction pipeline.
///
/// Groups timeout and limit parameters that rarely change between turns.
#[derive(Debug, Clone, Copy)]
pub struct ProcessTurnConfig {
    /// Maximum rows to retain in the `reasoning_strategies` table.
    pub store_limit: usize,
    /// Timeout for the self-judge LLM call.
    pub extraction_timeout: Duration,
    /// Timeout for the distillation LLM call.
    pub distill_timeout: Duration,
}

/// Run the full extraction pipeline for a single turn.
///
/// Calls [`run_self_judge`], then [`distill_strategy`], then inserts the result.
/// `evict_lru` is called when the table exceeds `store_limit`. All errors are
/// logged at `warn` level and the function returns `Ok(())` so callers never
/// propagate pipeline failures.
///
/// # Errors
///
/// Returns an error if the embedding call fails, but not if self-judge or distillation fails.
#[tracing::instrument(name = "memory.reasoning.process_turn", skip_all)]
pub async fn process_turn(
    memory: &ReasoningMemory,
    extract_provider: &AnyProvider,
    distill_provider: &AnyProvider,
    embed_provider: &AnyProvider,
    messages: &[Message],
    cfg: ProcessTurnConfig,
) -> Result<(), MemoryError> {
    let ProcessTurnConfig {
        store_limit,
        extraction_timeout,
        distill_timeout,
    } = cfg;
    let Some(outcome) = run_self_judge(extract_provider, messages, extraction_timeout).await else {
        return Ok(());
    };

    let outcome_enum = if outcome.success {
        Outcome::Success
    } else {
        Outcome::Failure
    };

    let Some(summary) = distill_strategy(
        distill_provider,
        outcome_enum,
        &outcome.reasoning_chain,
        distill_timeout,
    )
    .await
    else {
        return Ok(());
    };

    // Embed task_hint + summary for Qdrant retrieval (S2 from architect plan).
    let embed_input = format!("{}\n{}", outcome.task_hint, summary);
    let embedding = match embed_provider.embed(&embed_input).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "reasoning: embedding failed — strategy not stored");
            return Ok(());
        }
    };

    let id = uuid::Uuid::new_v4().to_string();
    let strategy = ReasoningStrategy {
        id,
        summary,
        outcome: outcome_enum,
        task_hint: outcome.task_hint,
        created_at: 0, // filled by SQL EPOCH_NOW
        last_used_at: 0,
        use_count: 0,
        embedded_at: None,
    };

    // P2-2: check count before insert to skip the evict_lru SELECT+DELETE when not needed.
    // If count is already at or above store_limit, evict after insert. Approximate: two
    // concurrent inserts can both read the same count and both decide to evict — the
    // evict_lru implementation is idempotent so over-eviction by ≤1 row is acceptable.
    let count_before = memory.count().await.unwrap_or(0);

    if let Err(e) = memory.insert(&strategy, embedding).await {
        tracing::warn!(error = %e, "reasoning: insert failed");
        return Ok(());
    }

    if count_before >= store_limit
        && let Err(e) = memory.evict_lru(store_limit).await
    {
        tracing::warn!(error = %e, "reasoning: evict_lru failed");
    }

    Ok(())
}

// ── private helpers ───────────────────────────────────────────────────────────

/// Maximum characters taken from a single message's content in the transcript prompt.
///
/// Prevents unbounded prompt growth when long tool outputs or code blocks are present
/// in the turn history (S-Med2 fix).
const MAX_TRANSCRIPT_MESSAGE_CHARS: usize = 2000;

/// Build a turn transcript prompt from the message slice.
///
/// Each message's content is truncated to [`MAX_TRANSCRIPT_MESSAGE_CHARS`] to bound
/// the prompt length regardless of tool-output size. Mirrors the
/// `build_extraction_prompt` format in `trajectory.rs` for consistency.
fn build_transcript_prompt(messages: &[Message]) -> String {
    let mut prompt = String::from("Agent turn messages:\n");
    for (i, msg) in messages.iter().enumerate() {
        use std::fmt::Write as _;
        let role = format!("{:?}", msg.role);
        // Truncate at a char boundary to avoid invalid UTF-8 slices.
        let content: std::borrow::Cow<str> =
            if msg.content.chars().count() > MAX_TRANSCRIPT_MESSAGE_CHARS {
                msg.content
                    .char_indices()
                    .nth(MAX_TRANSCRIPT_MESSAGE_CHARS)
                    .map_or(msg.content.as_str().into(), |(byte_idx, _)| {
                        msg.content[..byte_idx].into()
                    })
            } else {
                msg.content.as_str().into()
            };
        let _ = writeln!(prompt, "[{}] {}: {}", i + 1, role, content);
    }
    prompt.push_str("\nEvaluate this turn and return JSON.");
    prompt
}

/// Parse the LLM response from the self-judge step into a [`SelfJudgeOutcome`].
///
/// Strips markdown code fences, then tries direct parse; on failure, locates the
/// outermost `{…}` brackets and tries again. Returns `None` on persistent parse failure.
fn parse_self_judge_response(response: &str) -> Option<SelfJudgeOutcome> {
    // Strip markdown fences (```json … ```)
    let stripped = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    if let Ok(v) = serde_json::from_str::<SelfJudgeOutcome>(stripped) {
        return Some(v);
    }

    // Try to extract the first `{…}` span.
    if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}'))
        && end > start
        && let Ok(v) = serde_json::from_str::<SelfJudgeOutcome>(&stripped[start..=end])
    {
        return Some(v);
    }

    tracing::warn!(
        "reasoning: failed to parse self-judge response (len={}): {:.200}",
        response.len(),
        response
    );
    None
}

/// Trim text to at most 3 sentences and 512 characters.
///
/// Sentence boundaries are detected by `.`, `!`, `?` followed by whitespace or end-of-string.
/// The hard 512-char cap truncates at the nearest char boundary below the limit.
fn trim_to_three_sentences(text: &str) -> String {
    const MAX_CHARS: usize = 512;
    const MAX_SENTENCES: usize = 3;

    let text = text.trim();
    let mut sentence_ends: Vec<usize> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();

    for (i, &ch) in chars.iter().enumerate() {
        if matches!(ch, '.' | '!' | '?') {
            let next_is_boundary = i + 1 >= len || chars[i + 1].is_whitespace();
            if next_is_boundary {
                sentence_ends.push(i + 1); // exclusive byte position (chars)
                if sentence_ends.len() >= MAX_SENTENCES {
                    break;
                }
            }
        }
    }

    let char_limit = if let Some(&end) = sentence_ends.last() {
        end.min(MAX_CHARS)
    } else {
        text.chars().count().min(MAX_CHARS)
    };

    let result: String = text.chars().take(char_limit).collect();
    // Hard cap on byte length (chars already limited, but enforce once more).
    match result.char_indices().nth(MAX_CHARS) {
        Some((byte_idx, _)) => result[..byte_idx].to_owned(),
        None => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Outcome ────────────────────────────────────────────────────────────────

    #[test]
    fn outcome_as_str_round_trip() {
        assert_eq!(Outcome::Success.as_str(), "success");
        assert_eq!(Outcome::Failure.as_str(), "failure");
    }

    #[test]
    fn outcome_from_str_success() {
        assert_eq!(Outcome::from_str("success").unwrap(), Outcome::Success);
    }

    #[test]
    fn outcome_from_str_failure() {
        assert_eq!(Outcome::from_str("failure").unwrap(), Outcome::Failure);
    }

    #[test]
    fn outcome_from_str_unknown_defaults_to_failure() {
        // Unknown values silently map to Failure (forward-compatible).
        assert_eq!(Outcome::from_str("partial").unwrap(), Outcome::Failure);
    }

    // ── parse_self_judge_response ─────────────────────────────────────────────

    #[test]
    fn parse_direct_json() {
        let json = r#"{"success":true,"reasoning_chain":"tried X","task_hint":"do Y"}"#;
        let outcome = parse_self_judge_response(json).unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.reasoning_chain, "tried X");
        assert_eq!(outcome.task_hint, "do Y");
    }

    #[test]
    fn parse_json_with_markdown_fences() {
        let response =
            "```json\n{\"success\":false,\"reasoning_chain\":\"r\",\"task_hint\":\"t\"}\n```";
        let outcome = parse_self_judge_response(response).unwrap();
        assert!(!outcome.success);
    }

    #[test]
    fn parse_json_embedded_in_prose() {
        let response = r#"Here is the evaluation: {"success":true,"reasoning_chain":"chain","task_hint":"hint"} — done."#;
        let outcome = parse_self_judge_response(response).unwrap();
        assert!(outcome.success);
    }

    #[test]
    fn parse_invalid_returns_none() {
        let outcome = parse_self_judge_response("not json at all");
        assert!(outcome.is_none());
    }

    // ── trim_to_three_sentences ───────────────────────────────────────────────

    #[test]
    fn trim_three_sentences_short_text() {
        let text = "One. Two. Three.";
        assert_eq!(trim_to_three_sentences(text), "One. Two. Three.");
    }

    #[test]
    fn trim_three_sentences_truncates_at_third() {
        let text = "One. Two. Three. Four. Five.";
        let result = trim_to_three_sentences(text);
        assert!(result.ends_with("Three."), "got: {result}");
        assert!(!result.contains("Four"));
    }

    #[test]
    fn trim_three_sentences_hard_cap() {
        // 600 chars, no sentence boundaries → should be capped at 512 chars
        let long: String = "x".repeat(600);
        let result = trim_to_three_sentences(&long);
        assert!(result.chars().count() <= 512);
    }

    #[test]
    fn trim_three_sentences_empty() {
        assert_eq!(trim_to_three_sentences("   "), "");
    }

    // ── ReasoningMemory (in-memory SQLite) ────────────────────────────────────

    async fn make_test_pool() -> DbPool {
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE reasoning_strategies (
                id           TEXT    PRIMARY KEY NOT NULL,
                summary      TEXT    NOT NULL,
                outcome      TEXT    NOT NULL,
                task_hint    TEXT    NOT NULL,
                created_at   INTEGER NOT NULL DEFAULT (unixepoch('now')),
                last_used_at INTEGER NOT NULL DEFAULT (unixepoch('now')),
                use_count    INTEGER NOT NULL DEFAULT 0,
                embedded_at  INTEGER
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    fn make_strategy(id: &str) -> ReasoningStrategy {
        ReasoningStrategy {
            id: id.to_owned(),
            summary: format!("Summary for {id}"),
            outcome: Outcome::Success,
            task_hint: format!("Task hint for {id}"),
            created_at: 0,
            last_used_at: 0,
            use_count: 0,
            embedded_at: None,
        }
    }

    #[tokio::test]
    async fn insert_and_fetch_by_ids() {
        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool, None);

        let s = make_strategy("abc-123");
        mem.insert(&s, vec![]).await.unwrap();

        let rows = mem.fetch_by_ids(&["abc-123".to_owned()]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "abc-123");
        assert_eq!(rows[0].outcome, Outcome::Success);
    }

    #[tokio::test]
    async fn mark_used_increments_count() {
        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool, None);

        let s = make_strategy("mark-1");
        mem.insert(&s, vec![]).await.unwrap();
        mem.mark_used(&["mark-1".to_owned()]).await.unwrap();
        mem.mark_used(&["mark-1".to_owned()]).await.unwrap();

        let rows = mem.fetch_by_ids(&["mark-1".to_owned()]).await.unwrap();
        assert_eq!(rows[0].use_count, 2);
    }

    #[tokio::test]
    async fn mark_used_empty_is_noop() {
        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool, None);
        // Should not panic or error on empty slice.
        mem.mark_used(&[]).await.unwrap();
    }

    #[tokio::test]
    async fn count_returns_correct_total() {
        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool, None);

        for i in 0..5 {
            mem.insert(&make_strategy(&format!("s{i}")), vec![])
                .await
                .unwrap();
        }

        assert_eq!(mem.count().await.unwrap(), 5);
    }

    #[tokio::test]
    async fn evict_lru_cold_rows() {
        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool, None);

        // Insert 5 cold rows (use_count = 0 by default).
        for i in 0..5 {
            mem.insert(&make_strategy(&format!("cold-{i}")), vec![])
                .await
                .unwrap();
        }

        // Store limit is 3 → should delete 2 oldest.
        let deleted = mem.evict_lru(3).await.unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(mem.count().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn evict_lru_respects_hot_rows_under_ceiling() {
        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool.clone(), None);

        // Insert 5 hot rows by manually setting use_count > HOT_STRATEGY_USE_COUNT.
        for i in 0..5 {
            let id = format!("hot-{i}");
            mem.insert(&make_strategy(&id), vec![]).await.unwrap();
            // Mark used 11 times to make them hot.
            let ids: Vec<String> = (0..11).map(|_| id.clone()).collect();
            for chunk_ids in ids.chunks(1) {
                mem.mark_used(chunk_ids).await.unwrap();
            }
        }

        // store_limit=3, count=5, all hot, 5 < 2*3=6 → under ceiling → no deletion.
        let deleted = mem.evict_lru(3).await.unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(mem.count().await.unwrap(), 5);
    }

    #[tokio::test]
    async fn evict_lru_hard_ceiling_forces_deletion() {
        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool.clone(), None);

        // Insert 7 hot rows. store_limit=3, ceiling=6. 7 > 6 → forced eviction.
        for i in 0..7 {
            let id = format!("hot2-{i}");
            mem.insert(&make_strategy(&id), vec![]).await.unwrap();
            // Make hot.
            for _ in 0..=HOT_STRATEGY_USE_COUNT {
                mem.mark_used(&[id.clone()]).await.unwrap();
            }
        }

        let deleted = mem.evict_lru(3).await.unwrap();
        assert!(deleted > 0, "expected forced deletion");
        let remaining = mem.count().await.unwrap();
        assert_eq!(remaining, 3, "should be trimmed to store_limit");
    }

    #[tokio::test]
    async fn evict_lru_no_op_when_under_limit() {
        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool, None);

        for i in 0..3 {
            mem.insert(&make_strategy(&format!("s{i}")), vec![])
                .await
                .unwrap();
        }

        // store_limit=10 → count(3) ≤ 10 → no deletion.
        let deleted = mem.evict_lru(10).await.unwrap();
        assert_eq!(deleted, 0);
    }

    // ── mark_used chunked path ────────────────────────────────────────────────

    #[tokio::test]
    async fn mark_used_chunked_over_490_ids() {
        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool, None);

        // Insert 500 strategies — exceeds MAX_IDS_PER_QUERY (490) forcing two SQL batches.
        for i in 0..500usize {
            mem.insert(&make_strategy(&format!("chunked-{i}")), vec![])
                .await
                .unwrap();
        }

        let ids: Vec<String> = (0..500usize).map(|i| format!("chunked-{i}")).collect();
        mem.mark_used(&ids).await.unwrap();

        // Spot-check: first and 491st should both have use_count == 1.
        let first = mem.fetch_by_ids(&[ids[0].clone()]).await.unwrap();
        let over_chunk = mem.fetch_by_ids(&[ids[490].clone()]).await.unwrap();
        assert_eq!(first[0].use_count, 1, "first id should have use_count = 1");
        assert_eq!(
            over_chunk[0].use_count, 1,
            "id past the chunk boundary should have use_count = 1"
        );
    }

    // ── run_self_judge malformed response ─────────────────────────────────────

    #[tokio::test]
    async fn run_self_judge_malformed_json_returns_none() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        // with_responses populates the one-shot queue; chat() returns this prose string.
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec![
            "This is not JSON at all.".to_string(),
        ]));
        let msgs = vec![Message::from_legacy(Role::User, "hello")];
        let result = run_self_judge(&provider, &msgs, std::time::Duration::from_secs(5)).await;
        assert!(result.is_none(), "malformed LLM response must return None");
    }

    // ── distill_strategy truncation ───────────────────────────────────────────

    #[tokio::test]
    async fn distill_strategy_truncates_to_three_sentences() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let long_response = "One. Two. Three. Four. Five.";
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec![
            long_response.to_string(),
        ]));
        let result = distill_strategy(
            &provider,
            Outcome::Success,
            "chain here",
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(result.ends_with("Three."), "got: {result}");
        assert!(
            !result.contains("Four"),
            "should not contain 4th sentence: {result}"
        );
    }

    // ── process_turn smoke test ───────────────────────────────────────────────

    #[tokio::test]
    async fn process_turn_with_empty_messages_is_noop() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        let pool = make_test_pool().await;
        let mem = ReasoningMemory::new(pool, None);
        // MockProvider returns "{}" which parse_self_judge_response will return None for
        // (missing required fields) → Ok(()) with zero inserts.
        let provider = AnyProvider::Mock(MockProvider::default());
        let cfg = ProcessTurnConfig {
            store_limit: 100,
            extraction_timeout: std::time::Duration::from_secs(1),
            distill_timeout: std::time::Duration::from_secs(1),
        };
        let result = process_turn(&mem, &provider, &provider, &provider, &[], cfg).await;
        assert!(
            result.is_ok(),
            "process_turn with empty messages must succeed"
        );
        assert_eq!(
            mem.count().await.unwrap(),
            0,
            "no strategies should be stored"
        );
    }
}
