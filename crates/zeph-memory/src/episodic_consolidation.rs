// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Episodic-to-semantic consolidation daemon (issue #3799).
//!
//! A background loop sweeps mature `episodic_events` rows, batches them into a single
//! LLM call to extract durable factual statements, deduplicates via Jaccard similarity,
//! and promotes accepted facts to `consolidated_facts` (`SQLite`) and `zeph_key_facts`
//! (Qdrant, when available).
//!
//! # Daemon pattern
//!
//! [`start_episodic_consolidation_loop`] follows the same pattern as
//! [`crate::tiers::start_tier_promotion_loop`]: the loop exits immediately when
//! `config.enabled = false` and fails open on every error (logs warning, skips sweep).
//!
//! # Invariants
//!
//! - Episodic events are **never deleted** — `consolidated_at` is set to mark processed rows.
//! - LLM timeout or Qdrant unavailability skips the sweep; it does NOT crash the agent.
//! - Re-running a sweep is idempotent: `WHERE consolidated_at IS NULL` prevents re-processing.
//! - Each fact promotion is a single `SQLite` transaction for consistency.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use zeph_db::{DbPool, sql};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::semantic::KEY_FACTS_COLLECTION;
use crate::store::SqliteStore;

/// Row fetched from `episodic_events`: `(id, session_id, event_type, summary, message_content, created_at)`.
type CandidateRow = (i64, String, String, String, String, i64);

/// Configuration for the episodic consolidation daemon.
///
/// Passed from `zeph-config::EpisodicConsolidationConfig` to avoid a direct
/// dependency from `zeph-memory` on `zeph-config`.
#[derive(Debug, Clone)]
pub struct EpisodicConsolidationConfig {
    /// Enable the episodic consolidation daemon.
    pub enabled: bool,
    /// Provider name for fact extraction LLM calls (resolved by the caller).
    pub consolidation_provider: String,
    /// How often the sweep runs, in seconds. Default: `1800`.
    pub interval_secs: u64,
    /// Maximum episodic events processed per sweep. Default: `30`.
    pub batch_size: usize,
    /// Minimum age in seconds before an event is eligible. Default: `300`.
    pub min_age_secs: u64,
    /// Jaccard token-set similarity threshold for dedup. Default: `0.6`.
    pub dedup_jaccard_threshold: f32,
}

/// Result of one episodic consolidation sweep.
#[derive(Debug, Default)]
pub struct EpisodicConsolidationResult {
    /// Number of episodic events processed in this sweep.
    pub events_processed: usize,
    /// Number of new facts promoted to semantic tier.
    pub facts_promoted: usize,
    /// Number of candidate facts dropped as near-duplicates.
    pub duplicates_skipped: usize,
    /// Number of events skipped due to negative cognitive weight.
    pub negative_weight_skipped: usize,
}

/// A candidate episodic event fetched for consolidation.
struct ConsolidationCandidate {
    /// Row ID from `episodic_events.id` (NOT `ExperienceId` — these are different tables).
    event_id: i64,
    #[allow(dead_code)]
    session_id: String,
    event_type: String,
    summary: String,
    message_content: String,
    cognitive_weight: f64,
}

/// An extracted fact from the LLM response.
struct ExtractedFact {
    fact: String,
    source_event_ids: Vec<i64>,
}

/// A non-duplicate fact ready for Qdrant embedding and `SQLite` promotion.
struct PendingFact {
    fact: ExtractedFact,
    cog_weight_f32: f32,
    valid_source_ids: Vec<i64>,
}

/// Start the background episodic consolidation loop.
///
/// The loop ticks every `config.interval_secs` seconds, skipping the first tick to avoid
/// running at startup. Each tick calls [`run_episodic_consolidation_sweep`]; errors are
/// logged as warnings and do not stop the loop.
///
/// Returns immediately if `config.enabled = false`.
pub async fn start_episodic_consolidation_loop(
    store: Arc<SqliteStore>,
    provider: AnyProvider,
    config: EpisodicConsolidationConfig,
    qdrant: Option<Arc<EmbeddingStore>>,
    cancel: CancellationToken,
) {
    if !config.enabled {
        tracing::debug!("episodic consolidation disabled (episodic_consolidation.enabled = false)");
        return;
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(config.interval_secs));
    // Skip the first immediate tick so we don't run at startup.
    ticker.tick().await;

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::debug!("episodic consolidation loop shutting down");
                return;
            }
            _ = ticker.tick() => {}
        }

        match run_episodic_consolidation_sweep(
            store.pool().clone(),
            &provider,
            &config,
            qdrant.as_deref(),
        )
        .await
        {
            Ok(r) => {
                tracing::info!(
                    events = r.events_processed,
                    promoted = r.facts_promoted,
                    dupes = r.duplicates_skipped,
                    skipped_neg = r.negative_weight_skipped,
                    "episodic consolidation sweep complete"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "episodic consolidation sweep failed — skipping");
            }
        }
    }
}

/// Run a single episodic consolidation sweep.
///
/// Steps:
/// 1. Fetch mature, unprocessed candidates.
/// 2. Compute cognitive weight from `experience_nodes`.
/// 3. Call LLM to extract facts (single batch).
/// 4. Jaccard dedup against last 200 existing facts.
/// 5. Promote accepted facts; mark source events as consolidated.
///
/// # Errors
///
/// Returns [`MemoryError`] on database or LLM errors.
#[allow(clippy::too_many_lines)] // complex sweep pipeline; decomposition deferred to future refactor
#[tracing::instrument(skip_all, name = "memory.episodic_consolidation.sweep")]
pub async fn run_episodic_consolidation_sweep(
    pool: DbPool,
    provider: &AnyProvider,
    config: &EpisodicConsolidationConfig,
    qdrant: Option<&EmbeddingStore>,
) -> Result<EpisodicConsolidationResult, MemoryError> {
    let mut result = EpisodicConsolidationResult::default();

    // Step 1: fetch candidates.
    let raw_candidates = fetch_candidates(&pool, config).await?;

    if raw_candidates.is_empty() {
        return Ok(result);
    }

    // Step 2: compute cognitive weight for each candidate.
    let mut candidates: Vec<ConsolidationCandidate> = Vec::with_capacity(raw_candidates.len());
    for (event_id, session_id, event_type, summary, message_content, created_at) in raw_candidates {
        let weight = compute_cognitive_weight(&pool, &session_id, created_at).await?;
        if weight < -0.5 {
            result.negative_weight_skipped += 1;
            // Still mark as consolidated so we don't retry endlessly.
            mark_consolidated(&pool, event_id).await?;
            continue;
        }
        candidates.push(ConsolidationCandidate {
            event_id,
            session_id,
            event_type,
            summary,
            message_content,
            cognitive_weight: weight,
        });
    }

    if candidates.is_empty() {
        return Ok(result);
    }

    result.events_processed = candidates.len();

    // Step 3: extract facts via LLM.
    let extracted = match extract_facts_via_llm(provider, &candidates).await {
        Ok(facts) => facts,
        Err(e) => {
            tracing::warn!(error = %e, "episodic consolidation: LLM extraction failed, skipping sweep");
            return Err(e);
        }
    };

    // Empty array from LLM is valid — mark all events consolidated (nothing to extract).
    if extracted.is_empty() {
        tracing::debug!(
            "episodic consolidation: LLM returned no facts, marking events consolidated"
        );
        for c in &candidates {
            mark_consolidated(&pool, c.event_id).await?;
        }
        return Ok(result);
    }

    // Step 4: Jaccard dedup against last 200 existing facts.
    let existing_facts = fetch_existing_facts(&pool, 200).await?;

    // Step 5: promote accepted facts.
    {
        let mut all_source_event_ids: HashSet<i64> = HashSet::new();

        // Separate unique facts from duplicates upfront so we can batch-embed in one call.
        let mut pending: Vec<PendingFact> = Vec::new();

        for fact in extracted {
            let is_dup = {
                let _span = tracing::info_span!("memory.episodic_consolidation.dedup").entered();
                is_jaccard_duplicate(&fact.fact, &existing_facts, config.dedup_jaccard_threshold)
            };

            if is_dup {
                result.duplicates_skipped += 1;
                for id in fact
                    .source_event_ids
                    .iter()
                    .copied()
                    .filter(|id| candidates.iter().any(|c| c.event_id == *id))
                {
                    all_source_event_ids.insert(id);
                }
                continue;
            }

            let cog_weight = candidates
                .iter()
                .filter(|c| fact.source_event_ids.contains(&c.event_id))
                .map(|c| c.cognitive_weight)
                .sum::<f64>();
            let cog_weight = if cog_weight.is_finite() {
                cog_weight
            } else {
                0.0_f64
            };

            #[allow(clippy::cast_possible_truncation)]
            let cog_weight_f32 = cog_weight as f32;

            let valid_source_ids: Vec<i64> = fact
                .source_event_ids
                .iter()
                .copied()
                .filter(|id| candidates.iter().any(|c| c.event_id == *id))
                .collect();

            pending.push(PendingFact {
                fact,
                cog_weight_f32,
                valid_source_ids,
            });
        }

        // Batch-embed all non-duplicate facts in one call when Qdrant is available.
        let embeddings: Vec<Option<Vec<f32>>> = if qdrant.is_some()
            && provider.supports_embeddings()
            && !pending.is_empty()
        {
            let texts: Vec<&str> = pending.iter().map(|p| p.fact.fact.as_str()).collect();
            let span = tracing::info_span!("memory.episodic.embed_batch", count = texts.len());
            let vecs = provider.embed_batch(&texts).instrument(span).await;
            match vecs {
                Ok(vecs) => {
                    if vecs.len() == texts.len() {
                        vecs.into_iter().map(Some).collect()
                    } else {
                        tracing::warn!(
                            expected = texts.len(),
                            got = vecs.len(),
                            "episodic consolidation: embed_batch length mismatch, Qdrant upsert skipped"
                        );
                        pending.iter().map(|_| None).collect()
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "episodic consolidation: embed_batch failed, facts stored in SQLite only"
                    );
                    pending.iter().map(|_| None).collect()
                }
            }
        } else {
            pending.iter().map(|_| None).collect()
        };

        for (p, embedding) in pending.into_iter().zip(embeddings) {
            promote_fact(
                &pool,
                &p.fact.fact,
                p.cog_weight_f32,
                &p.valid_source_ids,
                qdrant,
                embedding,
            )
            .await?;

            result.facts_promoted += 1;
            for id in &p.fact.source_event_ids {
                all_source_event_ids.insert(*id);
            }
        }

        // Mark any candidates not covered by extracted facts as consolidated too.
        for c in &candidates {
            all_source_event_ids.insert(c.event_id);
        }
        for event_id in all_source_event_ids {
            mark_consolidated(&pool, event_id).await?;
        }
    }

    Ok(result)
}

/// Fetch unprocessed, mature episodic events with message content.
///
/// Excludes `SummaryOnly` messages (too degraded) and prefers `compressed_content`
/// when available (`ScrapMem` fidelity levels: `Full`, `Compressed`, `SummaryOnly`).
#[tracing::instrument(skip_all, name = "memory.episodic_consolidation.fetch_candidates")]
async fn fetch_candidates(
    pool: &DbPool,
    config: &EpisodicConsolidationConfig,
) -> Result<Vec<CandidateRow>, MemoryError> {
    let min_age = i64::try_from(config.min_age_secs).unwrap_or(i64::MAX);
    let batch = i64::try_from(config.batch_size).unwrap_or(i64::MAX);

    let rows: Vec<CandidateRow> = zeph_db::query_as(sql!(
        "SELECT e.id, e.session_id, e.event_type, e.summary,
                COALESCE(m.compressed_content, m.content) AS message_content,
                e.created_at
         FROM episodic_events e
         JOIN messages m ON m.id = e.message_id
         WHERE e.consolidated_at IS NULL
           AND e.created_at < unixepoch() - ?1
           AND m.content_fidelity != 'SummaryOnly'
         ORDER BY e.created_at ASC
         LIMIT ?2"
    ))
    .bind(min_age)
    .bind(batch)
    .fetch_all(pool)
    .await
    .map_err(MemoryError::from)?;

    Ok(rows)
}

/// Compute a cognitive weight signal for an episodic event.
///
/// Joins `experience_nodes` within a ±30 s window around the event's `created_at`.
/// Returns 0.0 when no experience data is available.
#[tracing::instrument(skip(pool), name = "memory.episodic_consolidation.cognitive_weight")]
async fn compute_cognitive_weight(
    pool: &DbPool,
    session_id: &str,
    created_at: i64,
) -> Result<f64, MemoryError> {
    let weight: f64 = zeph_db::query_scalar(sql!(
        "SELECT COALESCE(SUM(CASE
             WHEN outcome = 'success' THEN 1.0
             WHEN outcome = 'error'   THEN -0.5
             ELSE 0.0
         END), 0.0)
         FROM experience_nodes
         WHERE session_id = ?1
           AND created_at BETWEEN ?2 - 30 AND ?2 + 30"
    ))
    .bind(session_id)
    .bind(created_at)
    .fetch_one(pool)
    .await
    .map_err(MemoryError::from)?;

    Ok(weight)
}

/// Call the LLM to extract durable facts from a batch of episodic events.
///
/// Returns an empty vec when the LLM signals no extractable facts.
/// Returns `Err` on timeout or malformed JSON.
#[tracing::instrument(skip_all, name = "memory.episodic_consolidation.extract_facts")]
async fn extract_facts_via_llm(
    provider: &AnyProvider,
    candidates: &[ConsolidationCandidate],
) -> Result<Vec<ExtractedFact>, MemoryError> {
    let system_prompt = "You are a memory consolidation assistant. Given episodic events from an \
        agent session, extract reusable factual statements. Each fact should be a single sentence \
        that would be useful to recall in future conversations. Return a JSON array of objects: \
        [{\"fact\": \"...\", \"source_event_ids\": [1, 2, ...]}]. \
        Only extract facts that represent durable knowledge, not transient actions. \
        Skip events that are purely procedural with no lasting insight.";

    let user_content = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let excerpt: String = c.message_content.chars().take(256).collect();
            let summary_excerpt: String = c.summary.chars().take(256).collect();
            format!(
                "{}. [id={}] type={}, summary={}, message={}",
                i + 1,
                c.event_id,
                c.event_type,
                summary_excerpt,
                excerpt
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let messages = vec![
        Message {
            role: Role::System,
            content: system_prompt.to_owned(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
        Message {
            role: Role::User,
            content: user_content,
            parts: vec![],
            metadata: MessageMetadata::default(),
        },
    ];

    let text = tokio::time::timeout(Duration::from_secs(30), provider.chat(&messages))
        .await
        .map_err(|_| {
            MemoryError::Other("episodic consolidation: LLM call timed out after 30s".to_owned())
        })?
        .map_err(|e| MemoryError::Other(format!("episodic consolidation: LLM error: {e}")))?;

    let text = text.trim().to_owned();
    if text.is_empty() {
        return Ok(Vec::new());
    }

    // Strip optional ```json … ``` fencing.
    let json_str = if let Some(inner) = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
    {
        inner.trim_end_matches("```").trim()
    } else {
        text.as_str()
    };

    let parsed: serde_json::Value = serde_json::from_str(json_str).map_err(|e| {
        MemoryError::Other(format!(
            "episodic consolidation: malformed LLM JSON: {e} — raw: {json_str}"
        ))
    })?;

    let arr = parsed.as_array().ok_or_else(|| {
        MemoryError::Other("episodic consolidation: LLM returned non-array JSON".to_owned())
    })?;

    let mut facts = Vec::with_capacity(arr.len());
    for item in arr {
        let fact = item
            .get("fact")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if fact.is_empty() {
            continue;
        }
        let source_ids: Vec<i64> = item
            .get("source_event_ids")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(serde_json::Value::as_i64).collect())
            .unwrap_or_default();
        facts.push(ExtractedFact {
            fact,
            source_event_ids: source_ids,
        });
    }

    Ok(facts)
}

/// Fetch the last `limit` consolidated facts for Jaccard dedup.
#[tracing::instrument(skip(pool), name = "memory.episodic.fetch_existing_facts")]
async fn fetch_existing_facts(pool: &DbPool, limit: i64) -> Result<Vec<String>, MemoryError> {
    let rows: Vec<(String,)> = zeph_db::query_as(sql!(
        "SELECT fact_text FROM consolidated_facts ORDER BY created_at DESC LIMIT ?1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(MemoryError::from)?;

    Ok(rows.into_iter().map(|(s,)| s).collect())
}

/// Compute Jaccard similarity between two token sets.
fn jaccard(a: &HashSet<&str>, b: &HashSet<&str>) -> f32 {
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    (intersection as f32 / union as f32)
}

/// Return `true` if `fact` is a near-duplicate of any existing fact.
fn is_jaccard_duplicate(fact: &str, existing: &[String], threshold: f32) -> bool {
    let tokens: HashSet<&str> = fact.split_ascii_whitespace().collect();
    for existing_fact in existing {
        let existing_tokens: HashSet<&str> = existing_fact.split_ascii_whitespace().collect();
        if jaccard(&tokens, &existing_tokens) >= threshold {
            return true;
        }
    }
    false
}

/// Promote a single accepted fact to `SQLite` and optionally Qdrant.
///
/// All `SQLite` writes for one fact happen in one transaction. The `embedding`
/// parameter carries a pre-computed vector (from `embed_batch` in the caller).
/// When `None`, Qdrant upsert is skipped (no embedding support, or batch failed).
#[tracing::instrument(skip_all, name = "memory.episodic.promote_fact")]
async fn promote_fact(
    pool: &DbPool,
    fact_text: &str,
    cognitive_weight: f32,
    source_event_ids: &[i64],
    qdrant: Option<&EmbeddingStore>,
    embedding: Option<Vec<f32>>,
) -> Result<(), MemoryError> {
    // Persist fact and provenance links in a single transaction.
    let fact_id: i64 = {
        let mut tx = pool.begin().await.map_err(MemoryError::from)?;

        let fid: i64 = sqlx::query_scalar(sql!(
            "INSERT INTO consolidated_facts (fact_text, source, cognitive_weight)
             VALUES (?1, 'episodic_consolidation', ?2)
             RETURNING id"
        ))
        .bind(fact_text)
        .bind(cognitive_weight)
        .fetch_one(&mut *tx)
        .await
        .map_err(MemoryError::from)?;

        for &event_id in source_event_ids {
            sqlx::query(sql!(
                "INSERT OR IGNORE INTO consolidated_fact_sources (fact_id, event_id)
                 VALUES (?1, ?2)"
            ))
            .bind(fid)
            .bind(event_id)
            .execute(&mut *tx)
            .await
            .map_err(MemoryError::from)?;
        }

        tx.commit().await.map_err(MemoryError::from)?;
        fid
    };

    // Upsert into Qdrant `zeph_key_facts` when a pre-computed embedding is available.
    if let (Some(qdrant), Some(vector)) = (qdrant, embedding) {
        let vector_size = u64::try_from(vector.len()).unwrap_or(896);
        if let Err(e) = qdrant
            .ensure_named_collection(KEY_FACTS_COLLECTION, vector_size)
            .await
        {
            tracing::warn!(error = %e, "episodic consolidation: failed to ensure key_facts collection");
        } else {
            let payload = serde_json::json!({
                "fact_text": fact_text,
                "source": "episodic_consolidation",
                "cognitive_weight": cognitive_weight,
                "consolidated_fact_id": fact_id,
            });
            if let Err(e) = qdrant
                .store_to_collection(KEY_FACTS_COLLECTION, payload, vector)
                .await
            {
                tracing::warn!(
                    error = %e,
                    "episodic consolidation: Qdrant upsert failed (SQLite fact was stored)"
                );
            }
        }
    }

    Ok(())
}

/// Mark an episodic event as consolidated.
#[tracing::instrument(skip(pool), name = "memory.episodic.mark_consolidated")]
async fn mark_consolidated(pool: &DbPool, event_id: i64) -> Result<(), MemoryError> {
    zeph_db::query(sql!(
        "UPDATE episodic_events SET consolidated_at = unixepoch() WHERE id = ?1"
    ))
    .bind(event_id)
    .execute(pool)
    .await
    .map_err(MemoryError::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
    use zeph_db::sql;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    async fn setup_db() -> (SqliteStore, DbPool) {
        let store = SqliteStore::new(":memory:").await.unwrap();
        let pool = store.pool().clone();
        (store, pool)
    }

    fn mock_provider_with_response(response: &str) -> AnyProvider {
        let mut p = MockProvider::default();
        p.default_response = response.to_owned();
        AnyProvider::Mock(p)
    }

    /// Insert a message + `episodic_event` row; returns (`message_id`, `event_id`).
    async fn insert_episodic_event(
        pool: &DbPool,
        conv_id: crate::ConversationId,
        content: &str,
        summary: &str,
        age_secs: i64,
    ) -> (i64, i64) {
        let msg_id: i64 = sqlx::query_scalar(sql!(
            "INSERT INTO messages (conversation_id, role, content)
             VALUES (?1, 'user', ?2)
             RETURNING id"
        ))
        .bind(conv_id.0)
        .bind(content)
        .fetch_one(pool)
        .await
        .unwrap();

        let created_at = chrono::Utc::now().timestamp() - age_secs;
        let event_id: i64 = sqlx::query_scalar(sql!(
            "INSERT INTO episodic_events (session_id, message_id, event_type, summary, created_at)
             VALUES ('test_session', ?1, 'tool_call', ?2, ?3)
             RETURNING id"
        ))
        .bind(msg_id)
        .bind(summary)
        .bind(created_at)
        .fetch_one(pool)
        .await
        .unwrap();

        (msg_id, event_id)
    }

    #[tokio::test]
    async fn sweep_happy_path_promotes_fact() {
        let (store, pool) = setup_db().await;
        let conv_id = store.create_conversation().await.unwrap();

        let (_, ev1) = insert_episodic_event(
            &pool,
            conv_id,
            "Alice uses Rust for systems programming",
            "Alice prefers Rust",
            600,
        )
        .await;

        let llm_response = format!(
            r#"[{{"fact":"Alice uses Rust for systems programming","source_event_ids":[{ev1}]}}]"#
        );
        let provider = mock_provider_with_response(&llm_response);
        let config = EpisodicConsolidationConfig {
            enabled: true,
            consolidation_provider: String::new(),
            interval_secs: 1800,
            batch_size: 30,
            min_age_secs: 300,
            dedup_jaccard_threshold: 0.6,
        };

        let result = run_episodic_consolidation_sweep(pool.clone(), &provider, &config, None)
            .await
            .unwrap();

        assert_eq!(result.events_processed, 1);
        assert_eq!(result.facts_promoted, 1);
        assert_eq!(result.duplicates_skipped, 0);

        let count: i64 = sqlx::query_scalar(sql!("SELECT COUNT(*) FROM consolidated_facts"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "one fact must be persisted to consolidated_facts");

        let consolidated_at: Option<i64> = sqlx::query_scalar(sql!(
            "SELECT consolidated_at FROM episodic_events WHERE id = ?1"
        ))
        .bind(ev1)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            consolidated_at.is_some(),
            "event must be marked consolidated after sweep"
        );
    }

    #[tokio::test]
    async fn sweep_empty_llm_response_marks_events_consolidated() {
        let (store, pool) = setup_db().await;
        let conv_id = store.create_conversation().await.unwrap();

        let (_, ev1) =
            insert_episodic_event(&pool, conv_id, "routine operation", "no insight", 600).await;

        let provider = mock_provider_with_response("[]");
        let config = EpisodicConsolidationConfig {
            enabled: true,
            consolidation_provider: String::new(),
            interval_secs: 1800,
            batch_size: 30,
            min_age_secs: 300,
            dedup_jaccard_threshold: 0.6,
        };

        let result = run_episodic_consolidation_sweep(pool.clone(), &provider, &config, None)
            .await
            .unwrap();

        assert_eq!(result.facts_promoted, 0, "no facts when LLM returns []");

        let consolidated_at: Option<i64> = sqlx::query_scalar(sql!(
            "SELECT consolidated_at FROM episodic_events WHERE id = ?1"
        ))
        .bind(ev1)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            consolidated_at.is_some(),
            "event must be marked consolidated even when LLM returns no facts"
        );
    }

    #[test]
    fn jaccard_identical_sets() {
        let a: HashSet<&str> = ["foo", "bar", "baz"].iter().copied().collect();
        let b = a.clone();
        assert!((jaccard(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn jaccard_disjoint_sets() {
        let a: HashSet<&str> = ["foo", "bar"].iter().copied().collect();
        let b: HashSet<&str> = ["baz", "qux"].iter().copied().collect();
        assert!((jaccard(&a, &b) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn jaccard_partial_overlap() {
        let a: HashSet<&str> = ["a", "b", "c"].iter().copied().collect();
        let b: HashSet<&str> = ["b", "c", "d"].iter().copied().collect();
        // intersection=2, union=4 → 0.5
        assert!((jaccard(&a, &b) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn is_duplicate_above_threshold() {
        let existing = vec!["The sky is blue and clear".to_owned()];
        assert!(is_jaccard_duplicate(
            "The sky is blue and clear",
            &existing,
            0.6
        ));
    }

    #[test]
    fn is_not_duplicate_below_threshold() {
        let existing = vec!["Rust is a systems programming language".to_owned()];
        assert!(!is_jaccard_duplicate(
            "Python is great for data science",
            &existing,
            0.6
        ));
    }

    #[test]
    fn episodic_consolidation_config_default() {
        let cfg = EpisodicConsolidationConfig {
            enabled: false,
            consolidation_provider: String::new(),
            interval_secs: 1800,
            batch_size: 30,
            min_age_secs: 300,
            dedup_jaccard_threshold: 0.6,
        };
        assert!(!cfg.enabled);
        assert_eq!(cfg.interval_secs, 1800);
        assert_eq!(cfg.batch_size, 30);
        assert_eq!(cfg.min_age_secs, 300);
        assert!((cfg.dedup_jaccard_threshold - 0.6).abs() < f32::EPSILON);
    }
}
