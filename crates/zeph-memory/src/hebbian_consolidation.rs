// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! HL-F3/F4 background consolidation for Hebbian-weighted graph entities (#3345).
//!
//! Periodically identifies high-traffic entity clusters (`degree × avg_weight` above a
//! configurable threshold), passes their neighbourhood summaries to a mid-tier LLM, and
//! stores the resulting strategy summaries as [`GraphRule`] rows anchored to the entity.
//!
//! # Architecture
//!
//! The consolidation loop (`spawn_consolidation_loop`) is started by the top-level runner
//! as a supervised `RunOnce` task. It fires a status spinner update at the start of each
//! sweep so the TUI always reflects background activity.
//!
//! # Transaction safety
//!
//! [`insert_graph_rule_and_mark`] wraps both the `INSERT INTO graph_rules` and the
//! `UPDATE graph_entities SET consolidated_at` in a single `SQLite` transaction so partial
//! state is never written.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zeph_common::config::memory::HebbianConsolidationConfig;
use zeph_db::sql;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, Role};

use crate::error::MemoryError;
use crate::store::SqliteStore;

// ── Internal helpers ───────────────────────────────────────────────────────────

/// Drop guard that clears the TUI spinner by sending an empty string when dropped.
///
/// Ensures the spinner is cleared on every exit path — success, error, or cancellation.
struct ClearStatusOnDrop(Option<tokio::sync::mpsc::UnboundedSender<String>>);

impl Drop for ClearStatusOnDrop {
    fn drop(&mut self) {
        if let Some(ref tx) = self.0 {
            let _ = tx.send(String::new());
        }
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Distilled outcome produced by the LLM for a single entity cluster (HL-F4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HebbianConsolidationOutcome {
    /// Human-readable strategy or pattern extracted from the cluster neighbourhood.
    pub summary: String,
    /// Optional retrieval hint: a short phrase the agent can use to re-surface this rule.
    pub trigger_hint: Option<String>,
    /// LLM confidence in the distilled rule (`0.0`–`1.0`).
    pub confidence: f64,
}

/// A high-traffic graph entity that qualifies for consolidation (HL-F3).
#[derive(Debug, Clone)]
pub struct HebbianConsolidationCandidate {
    /// Entity row identifier (`graph_entities.id`).
    pub entity_id: i64,
    /// Number of active edges incident to this entity.
    pub degree: u64,
    /// Average Hebbian weight across incident edges.
    pub avg_weight: f64,
    /// Combined score: `degree × avg_weight`.
    pub score: f64,
}

/// A distilled rule row stored in `graph_rules` (HL-F4).
#[derive(Debug, Clone)]
pub struct GraphRule {
    /// Auto-increment primary key.
    pub id: i64,
    /// Entity that anchors this rule.
    pub anchor_entity_id: i64,
    /// Distilled summary text.
    pub summary: String,
    /// Optional retrieval hint.
    pub trigger_hint: Option<String>,
    /// LLM-reported confidence.
    pub confidence: f64,
    /// Unix epoch seconds when the rule was created.
    pub created_at: i64,
}

// ── Core functions ─────────────────────────────────────────────────────────────

/// Query for graph entities whose `degree × avg_weight` exceeds `threshold`.
///
/// Only entities whose `consolidated_at` is either NULL or older than `cooldown_before`
/// (a Unix epoch timestamp) are returned. Results are ordered by score descending and
/// capped at `limit`.
///
/// # Errors
///
/// Returns an error if the database query fails.
pub async fn find_candidates(
    pool: &zeph_db::DbPool,
    threshold: f64,
    cooldown_before: i64,
    limit: usize,
) -> Result<Vec<HebbianConsolidationCandidate>, MemoryError> {
    // Degree counts only active (non-expired) edges; expired edges have `valid_to IS NOT NULL`.
    let rows: Vec<(i64, i64, f64, f64)> = zeph_db::query_as(sql!(
        "SELECT e.id,
                COUNT(ed.id)        AS degree,
                AVG(ed.weight)      AS avg_weight,
                COUNT(ed.id) * AVG(ed.weight) AS score
           FROM graph_entities e
           JOIN graph_edges ed
             ON (ed.source_entity_id = e.id OR ed.target_entity_id = e.id)
            AND ed.valid_to IS NULL
          WHERE (e.consolidated_at IS NULL OR e.consolidated_at < ?)
          GROUP BY e.id
         HAVING score > ?
          ORDER BY score DESC
          LIMIT ?"
    ))
    .bind(cooldown_before)
    .bind(threshold)
    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(entity_id, degree, avg_weight, score)| HebbianConsolidationCandidate {
                entity_id,
                // degree is always non-negative (COUNT), safe to interpret as u64.
                degree: u64::try_from(degree).unwrap_or(0),
                avg_weight,
                score,
            },
        )
        .collect())
}

/// Collect the `summary` texts of all active-edge neighbours of `entity_id`.
///
/// Returns at most `max_neighbors` summaries; entities with no summary are skipped.
/// Each database query is bounded by a 10-second timeout to prevent stalling the sweep.
///
/// # Errors
///
/// Returns an error if the database query fails or times out.
pub async fn collect_neighbors(
    pool: &zeph_db::DbPool,
    entity_id: i64,
    max_neighbors: usize,
) -> Result<Vec<String>, MemoryError> {
    // One hop from entity_id via active edges in either direction.
    let query_fut = zeph_db::query_as(sql!(
        "SELECT DISTINCT e.summary
           FROM graph_entities e
           JOIN graph_edges ed
             ON (ed.source_entity_id = ? AND ed.target_entity_id = e.id)
             OR (ed.target_entity_id = ? AND ed.source_entity_id = e.id)
          WHERE ed.valid_to IS NULL
            AND e.summary IS NOT NULL
          LIMIT ?"
    ))
    .bind(entity_id)
    .bind(entity_id)
    .bind(i64::try_from(max_neighbors).unwrap_or(i64::MAX))
    .fetch_all(pool);

    let rows: Vec<(Option<String>,)> = tokio::time::timeout(Duration::from_secs(10), query_fut)
        .await
        .map_err(|_| {
            tracing::warn!(
                entity_id,
                "hebbian_consolidation: collect_neighbors timed out after 10s"
            );
            MemoryError::Timeout("collect_neighbors".into())
        })??;

    Ok(rows.into_iter().filter_map(|(s,)| s).collect())
}

/// Call the LLM to distill a cluster of entity summaries into a [`HebbianConsolidationOutcome`].
///
/// Returns `None` if the LLM call fails, times out, or returns a response that cannot be
/// parsed as the expected JSON schema. Callers should log and skip on `None`.
pub async fn distill_cluster(
    provider: &AnyProvider,
    neighbors: &[String],
    timeout_secs: u64,
) -> Option<HebbianConsolidationOutcome> {
    if neighbors.is_empty() {
        return None;
    }

    let cluster_text = neighbors
        .iter()
        .enumerate()
        .map(|(i, s)| format!("  [{}] {s}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");

    let system = "You are a memory strategy analyst. \
        Given a cluster of related entity summaries from an agent's knowledge graph, \
        produce a single JSON object with this exact schema:\n\
        {\"summary\":\"<distilled strategy or pattern>\",\
        \"trigger_hint\":\"<short retrieval phrase, or null>\",\
        \"confidence\":<0.0-1.0>}\n\
        Return ONLY the JSON object — no markdown, no explanation.";

    let user = format!("Entity cluster:\n{cluster_text}");

    let messages = vec![
        Message::from_legacy(Role::System, system),
        Message::from_legacy(Role::User, &user),
    ];

    let chat_future = provider.chat(&messages);
    let text = match tokio::time::timeout(Duration::from_secs(timeout_secs), chat_future).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "hebbian_consolidation: LLM call failed");
            return None;
        }
        Err(_) => {
            tracing::warn!(timeout_secs, "hebbian_consolidation: LLM call timed out");
            return None;
        }
    };

    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let json_slice = &text[start..=end];

    match serde_json::from_str::<HebbianConsolidationOutcome>(json_slice) {
        Ok(outcome) => Some(outcome),
        Err(e) => {
            tracing::debug!(
                error = %e,
                response = %json_slice,
                "hebbian_consolidation: failed to parse LLM response"
            );
            None
        }
    }
}

/// Insert a [`GraphRule`] row and mark `anchor_id` as consolidated in one transaction.
///
/// The `consolidated_at` timestamp is set to the current Unix epoch so the cooldown
/// guard in `find_candidates` can skip this entity until the window elapses.
/// Each database operation is bounded by a 10-second timeout.
///
/// # Errors
///
/// Returns an error if the database transaction fails or times out.
pub async fn insert_graph_rule_and_mark(
    pool: &zeph_db::DbPool,
    anchor_id: i64,
    outcome: &HebbianConsolidationOutcome,
) -> Result<(), MemoryError> {
    let now = chrono::Utc::now().timestamp();

    let begin_fut = pool.begin();
    let mut tx = tokio::time::timeout(Duration::from_secs(10), begin_fut)
        .await
        .map_err(|_| {
            tracing::warn!(
                anchor_id,
                "hebbian_consolidation: begin transaction timed out after 10s"
            );
            MemoryError::Timeout("insert_graph_rule_and_mark: begin".into())
        })??;

    let insert_fut = zeph_db::query(sql!(
        "INSERT INTO graph_rules (anchor_entity_id, summary, trigger_hint, confidence, created_at)
         VALUES (?, ?, ?, ?, ?)"
    ))
    .bind(anchor_id)
    .bind(&outcome.summary)
    .bind(outcome.trigger_hint.as_deref())
    .bind(outcome.confidence)
    .bind(now)
    .execute(&mut *tx);

    tokio::time::timeout(Duration::from_secs(10), insert_fut)
        .await
        .map_err(|_| {
            tracing::warn!(
                anchor_id,
                "hebbian_consolidation: INSERT graph_rules timed out after 10s"
            );
            MemoryError::Timeout("insert_graph_rule_and_mark: insert".into())
        })??;

    let update_fut = zeph_db::query(sql!(
        "UPDATE graph_entities SET consolidated_at = ? WHERE id = ?"
    ))
    .bind(now)
    .bind(anchor_id)
    .execute(&mut *tx);

    tokio::time::timeout(Duration::from_secs(10), update_fut)
        .await
        .map_err(|_| {
            tracing::warn!(
                anchor_id,
                "hebbian_consolidation: UPDATE graph_entities timed out after 10s"
            );
            MemoryError::Timeout("insert_graph_rule_and_mark: update".into())
        })??;

    tx.commit().await?;
    Ok(())
}

/// Execute one full Hebbian consolidation sweep (HL-F3/F4).
///
/// Finds candidates, collects their neighbourhood, distills via LLM, and persists rules.
/// LLM failures are logged and skipped — a single failed distillation does not abort the
/// sweep. Checks the cancellation token between candidates to allow prompt shutdown.
///
/// # Errors
///
/// Returns an error if a mandatory database query fails (candidate lookup).
#[tracing::instrument(skip_all)]
pub async fn run_consolidation_sweep(
    store: &SqliteStore,
    config: &HebbianConsolidationConfig,
    provider: &AnyProvider,
    status_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    cancel: &CancellationToken,
) -> Result<u32, MemoryError> {
    // Guard clears the spinner on every exit path (success, early return on error, cancellation).
    let _clear_status = ClearStatusOnDrop(status_tx.cloned());

    if let Some(tx) = status_tx {
        let _ = tx.send("Consolidating memory clusters\u{2026}".to_owned());
    }

    let now = chrono::Utc::now().timestamp();
    let cooldown_secs = i64::try_from(config.consolidation_cooldown_secs).unwrap_or(i64::MAX);
    let cooldown_before = now.saturating_sub(cooldown_secs);

    let candidates = find_candidates(
        store.pool(),
        config.consolidation_threshold,
        cooldown_before,
        config.max_candidates_per_sweep,
    )
    .await?;

    let mut consolidated = 0u32;

    // Used throughout the loop body for async span instrumentation.
    use tracing::Instrument as _;

    for candidate in &candidates {
        if cancel.is_cancelled() {
            tracing::debug!("hebbian consolidation sweep cancelled mid-sweep");
            break;
        }

        let neighbors = {
            match collect_neighbors(
                store.pool(),
                candidate.entity_id,
                config.consolidation_max_neighbors,
            )
            .instrument(tracing::debug_span!("memory.hebbian.collect_neighbors"))
            .await
            {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(
                        entity_id = candidate.entity_id,
                        error = %e,
                        "hebbian_consolidation: failed to collect neighbours, skipping"
                    );
                    continue;
                }
            }
        };

        if neighbors.is_empty() {
            tracing::debug!(
                entity_id = candidate.entity_id,
                "hebbian_consolidation: no summaries in neighbourhood, skipping"
            );
            continue;
        }

        let outcome = {
            distill_cluster(
                provider,
                &neighbors,
                config.consolidation_prompt_timeout_secs,
            )
            .instrument(tracing::debug_span!("memory.hebbian.distill"))
            .await
        };

        let Some(outcome) = outcome else {
            tracing::debug!(
                entity_id = candidate.entity_id,
                "hebbian_consolidation: LLM returned no outcome, skipping"
            );
            continue;
        };

        let insert_result = {
            insert_graph_rule_and_mark(store.pool(), candidate.entity_id, &outcome)
                .instrument(tracing::debug_span!("memory.hebbian.insert"))
                .await
        };

        match insert_result {
            Ok(()) => {
                consolidated += 1;
                tracing::info!(
                    entity_id = candidate.entity_id,
                    score = candidate.score,
                    confidence = outcome.confidence,
                    "hebbian_consolidation: rule inserted"
                );
            }
            Err(e) => {
                tracing::warn!(
                    entity_id = candidate.entity_id,
                    error = %e,
                    "hebbian_consolidation: failed to insert rule"
                );
            }
        }
    }

    Ok(consolidated)
}

/// Start the Hebbian consolidation background loop (HL-F3/F4).
///
/// Exits immediately when `config.consolidation_interval_secs == 0` or when the
/// cancellation token fires. Database and LLM errors during sweeps are logged but
/// do not stop the loop.
pub async fn spawn_consolidation_loop(
    store: Arc<SqliteStore>,
    config: HebbianConsolidationConfig,
    provider: AnyProvider,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    cancel: CancellationToken,
) {
    if config.consolidation_interval_secs == 0 {
        tracing::debug!("hebbian_consolidation: loop disabled (consolidation_interval_secs = 0)");
        return;
    }

    let mut ticker = tokio::time::interval(Duration::from_secs(config.consolidation_interval_secs));
    // Skip the first immediate tick to avoid running at startup.
    ticker.tick().await;

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::debug!("hebbian_consolidation: loop shutting down");
                return;
            }
            _ = ticker.tick() => {}
        }

        let start = std::time::Instant::now();
        tracing::debug!("hebbian_consolidation: starting sweep");

        match run_consolidation_sweep(&store, &config, &provider, status_tx.as_ref(), &cancel).await
        {
            Ok(n) => {
                tracing::info!(
                    consolidated = n,
                    elapsed_ms = start.elapsed().as_millis(),
                    "hebbian_consolidation: sweep complete"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    elapsed_ms = start.elapsed().as_millis(),
                    "hebbian_consolidation: sweep failed, will retry"
                );
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    use super::*;
    use crate::store::SqliteStore;

    async fn make_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    /// Seed a graph entity with `edge_count` outgoing edges at `weight`.
    ///
    /// Each edge goes to a distinct single-use target entity so that target entities
    /// have degree=1 (score ≤ weight) and do not cross the threshold themselves. This
    /// keeps the candidate set predictable.
    async fn seed_entity_with_edges(
        store: &SqliteStore,
        name: &str,
        edge_count: usize,
        weight: f64,
    ) -> i64 {
        let entity_id: i64 = zeph_db::query_scalar(sql!(
            "INSERT INTO graph_entities (name, canonical_name, entity_type)
             VALUES (?, ?, 'concept')
             RETURNING id"
        ))
        .bind(name)
        .bind(name.to_lowercase())
        .fetch_one(store.pool())
        .await
        .unwrap();

        for i in 0..edge_count {
            // Distinct target per edge so each target has degree=1, score=weight < threshold.
            let target_name = format!("{name}_sink_{i}");
            let target_id: i64 = zeph_db::query_scalar(
                "INSERT INTO graph_entities (name, canonical_name, entity_type)
                 VALUES (?, ?, 'concept')
                 RETURNING id",
            )
            .bind(&target_name)
            .bind(&target_name)
            .fetch_one(store.pool())
            .await
            .unwrap();

            zeph_db::query(
                "INSERT INTO graph_edges
                    (source_entity_id, target_entity_id, relation, fact, confidence, weight)
                 VALUES (?, ?, 'related', 'test fact', 1.0, ?)",
            )
            .bind(entity_id)
            .bind(target_id)
            .bind(weight)
            .execute(store.pool())
            .await
            .unwrap();
        }

        entity_id
    }

    #[tokio::test]
    async fn test_find_candidates_empty_db() {
        let store = make_store().await;
        let candidates = find_candidates(store.pool(), 5.0, 0, 10).await.unwrap();
        assert!(candidates.is_empty(), "empty DB must return no candidates");
    }

    #[tokio::test]
    async fn test_find_candidates_below_threshold() {
        let store = make_store().await;
        // degree=1, weight=1.0 → score=1.0, below threshold=5.0
        seed_entity_with_edges(&store, "low", 1, 1.0).await;
        let candidates = find_candidates(store.pool(), 5.0, 0, 10).await.unwrap();
        assert!(
            candidates.is_empty(),
            "entity below threshold must not be returned"
        );
    }

    #[tokio::test]
    async fn test_find_candidates_above_threshold() {
        let store = make_store().await;
        // degree=3, weight=2.0 → score=6.0 > threshold=5.0
        let entity_id = seed_entity_with_edges(&store, "hot", 3, 2.0).await;
        let candidates = find_candidates(store.pool(), 5.0, 0, 10).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].entity_id, entity_id);
        assert!(candidates[0].score > 5.0);
    }

    #[tokio::test]
    async fn test_cooldown_respected() {
        let store = make_store().await;
        let entity_id = seed_entity_with_edges(&store, "hot", 3, 2.0).await;

        // Mark as recently consolidated.
        let now = chrono::Utc::now().timestamp();
        zeph_db::query(sql!(
            "UPDATE graph_entities SET consolidated_at = ? WHERE id = ?"
        ))
        .bind(now)
        .bind(entity_id)
        .execute(store.pool())
        .await
        .unwrap();

        // cooldown_before = now - 86400 (yesterday) → entity consolidated today → should be skipped
        let cooldown_before = now - 86_400;
        let candidates = find_candidates(store.pool(), 5.0, cooldown_before, 10)
            .await
            .unwrap();
        assert!(
            candidates.is_empty(),
            "entity within cooldown window must be skipped"
        );
    }

    #[tokio::test]
    async fn test_distill_cluster_parse_failure() {
        let mock = MockProvider::with_responses(vec!["not valid json at all".to_owned()]);
        let provider = AnyProvider::Mock(mock);
        let neighbors = vec!["Entity A uses Rust".to_owned()];
        let result = distill_cluster(&provider, &neighbors, 30).await;
        assert!(
            result.is_none(),
            "unparseable LLM response must return None"
        );
    }

    #[tokio::test]
    async fn test_insert_graph_rule_marks_consolidated_at() {
        let store = make_store().await;
        let entity_id = seed_entity_with_edges(&store, "anchor", 3, 2.0).await;

        let outcome = HebbianConsolidationOutcome {
            summary: "Agent frequently uses Rust for systems programming".to_owned(),
            trigger_hint: Some("Rust systems".to_owned()),
            confidence: 0.9,
        };

        insert_graph_rule_and_mark(store.pool(), entity_id, &outcome)
            .await
            .unwrap();

        // Rule must be in graph_rules.
        let rule_count: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM graph_rules WHERE anchor_entity_id = ?"
        ))
        .bind(entity_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(rule_count.0, 1, "one rule must be inserted");

        // Entity must have consolidated_at set.
        let ts: (Option<i64>,) = zeph_db::query_as(sql!(
            "SELECT consolidated_at FROM graph_entities WHERE id = ?"
        ))
        .bind(entity_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(
            ts.0.is_some(),
            "consolidated_at must be set after insert_graph_rule_and_mark"
        );
    }

    #[tokio::test]
    async fn test_enabled_false_skips_sweep() {
        let store = Arc::new(make_store().await);
        // Seed a hot entity.
        seed_entity_with_edges(&store, "hot", 3, 2.0).await;

        // Setting interval to 0 disables the loop immediately — equivalent to enabled=false.
        let config = HebbianConsolidationConfig {
            consolidation_interval_secs: 0,
            ..HebbianConsolidationConfig::default()
        };

        let mock = MockProvider::default();
        let provider = AnyProvider::Mock(mock);

        // spawn_consolidation_loop must return immediately when interval=0.
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(spawn_consolidation_loop(
            store.clone(),
            config,
            provider,
            None,
            cancel.clone(),
        ));
        // Give it time to exit on its own.
        tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("loop must exit immediately when interval=0")
            .unwrap();

        // No rules should have been inserted.
        let count: (i64,) = zeph_db::query_as(sql!("SELECT COUNT(*) FROM graph_rules"))
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            count.0, 0,
            "no rules must be inserted when loop is disabled"
        );
    }

    #[tokio::test]
    async fn test_sweep_cancelled_mid_loop() {
        let store = Arc::new(make_store().await);
        // Seed hot entities to ensure the loop has candidates to iterate.
        seed_entity_with_edges(&store, "hot1", 3, 2.0).await;
        seed_entity_with_edges(&store, "hot2", 4, 2.0).await;

        let config = HebbianConsolidationConfig {
            consolidation_threshold: 5.0,
            max_candidates_per_sweep: 10,
            ..HebbianConsolidationConfig::default()
        };

        let cancel = CancellationToken::new();
        // Pre-cancel before sweep starts — sweep must exit the candidate loop immediately.
        cancel.cancel();

        let mock = MockProvider::default();
        let provider = AnyProvider::Mock(mock);
        let result = run_consolidation_sweep(&store, &config, &provider, None, &cancel).await;

        // An already-cancelled sweep must succeed (no rules inserted, no panic).
        assert!(result.is_ok(), "cancelled sweep must not return error");
        assert_eq!(result.unwrap(), 0, "cancelled sweep must insert zero rules");
    }
}
