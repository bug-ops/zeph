// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Async consolidation daemon for five-signal retrieval (issue #4374).
//!
//! The daemon periodically evaluates episodic facts by five-signal score, promotes
//! high-scoring facts to Qdrant, and demotes cold facts to `episodic_only` tier.
//! It runs exclusively on the scheduler thread and never blocks agent turns.

#[cfg(feature = "scheduler")]
use std::pin::Pin;
#[cfg(feature = "scheduler")]
use std::sync::Arc;

#[cfg(feature = "scheduler")]
use tokio::time::Duration;
#[cfg(feature = "scheduler")]
use zeph_config::memory::FiveSignalConsolidationConfig;

#[cfg(feature = "scheduler")]
use crate::error::MemoryError;
#[cfg(feature = "scheduler")]
use crate::five_signal::FiveSignalRuntime;
#[cfg(feature = "scheduler")]
use crate::types::MessageId;

#[cfg(feature = "scheduler")]
use zeph_scheduler::{SchedulerError, TaskHandler};

/// Consolidation daemon handler implementing [`TaskHandler`].
///
/// Registered with `zeph-scheduler` under `TaskKind::Custom("five_signal_consolidation")`.
/// Each run queries the top-K episodic facts, scores them, promotes or demotes as needed,
/// and honours `daemon_max_runtime_ms` as a hard timeout.
///
/// Feature-gated behind `scheduler`.
#[cfg(feature = "scheduler")]
pub struct ConsolidationHandler {
    runtime: Arc<FiveSignalRuntime>,
    config: FiveSignalConsolidationConfig,
}

#[cfg(feature = "scheduler")]
impl ConsolidationHandler {
    /// Create a new handler.
    #[must_use]
    pub fn new(runtime: Arc<FiveSignalRuntime>, config: FiveSignalConsolidationConfig) -> Self {
        Self { runtime, config }
    }
}

#[cfg(feature = "scheduler")]
impl TaskHandler for ConsolidationHandler {
    fn execute(
        &self,
        _config: &serde_json::Value,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), SchedulerError>> + Send + '_>> {
        Box::pin(async move {
            let result = tokio::time::timeout(
                Duration::from_millis(self.config.daemon_max_runtime_ms),
                self.run_once(),
            )
            .await;

            match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "five_signal consolidation daemon run failed");
                    Ok(()) // non-fatal: retry on next scheduled run
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        max_runtime_ms = self.config.daemon_max_runtime_ms,
                        "five_signal consolidation daemon exceeded max runtime; deferring remainder"
                    );
                    Ok(())
                }
            }
        })
    }
}

#[cfg(feature = "scheduler")]
impl ConsolidationHandler {
    async fn run_once(&self) -> Result<(), MemoryError> {
        use sqlx::Row as _;

        tracing::info!("five_signal: consolidation daemon run started");

        let start = std::time::Instant::now();
        let rt = &self.runtime;
        rt.metrics.inc_consolidation_run();

        // Query the most recent top-K episodic facts.
        // NOTE: ORDER BY id DESC retrieves the newest facts, not the highest-scoring ones.
        // This is a known MVP trade-off: scoring the full episodic table would violate
        // NFR-004 runtime bounds. High-value older facts are evaluated in subsequent runs
        // as the session grows.
        let rows = sqlx::query(
            "SELECT id, created_at, qdrant_promoted, memory_tier \
             FROM messages \
             WHERE deleted_at IS NULL \
               AND (memory_tier IS NULL OR memory_tier NOT IN ('episodic_only')) \
             ORDER BY id DESC \
             LIMIT ?1",
        )
        .bind(i64::try_from(self.config.top_k_per_run).unwrap_or(i64::MAX))
        .fetch_all(&rt.pool)
        .await
        .map_err(|e| MemoryError::Db(e.into()))?;

        let mut promote_ids: Vec<i64> = Vec::new();
        let mut demote_ids: Vec<MessageId> = Vec::new();
        let mut processed: usize = 0;

        for row in &rows {
            if processed >= self.config.batch_size {
                break;
            }
            processed += 1;

            let fact_id: i64 = row.get("id");
            let created_at: i64 = row.get("created_at");
            let qdrant_promoted: i64 = row.get("qdrant_promoted");
            let memory_tier: Option<String> = row.try_get("memory_tier").ok().flatten();

            let novelty = rt.novelty_computer.compute(created_at);
            // Frequency and causal signals require per-fact async I/O. The daemon uses
            // neutral values (0.5) to keep runtime bounded (NFR-004). A future
            // enhancement can hydrate signals with a batched pre-fetch.
            let frequency = 0.5_f64;
            let causal =
                crate::five_signal::causal_distance::CausalDistanceComputer::distance_to_score(
                    rt.config.neutral_causal_distance,
                );
            let recency = novelty;

            let w = &rt.weights;
            let score = w.w_recency * recency
                + w.w_relevance * 0.5 // neutral relevance — no embedding at daemon time
                + w.w_frequency * frequency
                + w.w_causal * causal
                + w.w_novelty * novelty;

            let already_promoted =
                qdrant_promoted != 0 || memory_tier.as_deref() == Some("semantic");

            if score >= self.config.promotion_score_threshold && !already_promoted {
                promote_ids.push(fact_id);
            } else if score < self.config.demotion_score_threshold && already_promoted {
                demote_ids.push(MessageId(fact_id));
            }
        }

        // Promote: update SQLite tier (Qdrant upsert requires embedding; deferred to write path).
        let mut promoted: u64 = 0;
        for fact_id in &promote_ids {
            tracing::debug!(fact_id, "five_signal: promoting fact");
            let res = sqlx::query(
                "UPDATE messages SET memory_tier = 'semantic', qdrant_promoted = 1 WHERE id = ?1",
            )
            .bind(fact_id)
            .execute(&rt.pool)
            .await;
            if let Err(e) = res {
                tracing::warn!(fact_id, error = %e, "five_signal: failed to promote fact");
            } else {
                promoted += 1;
            }
        }

        // Demote: remove from Qdrant if available, update SQLite tier.
        let mut demoted: u64 = 0;
        if let Some(qdrant) = &rt.qdrant
            && !demote_ids.is_empty()
            && let Err(e) = qdrant.delete_by_message_ids(&demote_ids).await
        {
            tracing::warn!(error = %e, "five_signal: Qdrant delete failed during demotion");
        }
        for msg_id in &demote_ids {
            tracing::debug!(fact_id = msg_id.0, "five_signal: demoting fact");
            let res = sqlx::query(
                "UPDATE messages SET memory_tier = 'episodic_only', qdrant_promoted = 0 WHERE id = ?1",
            )
            .bind(msg_id.0)
            .execute(&rt.pool)
            .await;
            if let Err(e) = res {
                tracing::warn!(fact_id = msg_id.0, error = %e, "five_signal: failed to demote fact");
            } else {
                demoted += 1;
            }
        }

        rt.metrics.add_promoted(promoted);
        rt.metrics.add_demoted(demoted);

        tracing::info!(
            promoted,
            demoted,
            processed,
            run_duration_ms = start.elapsed().as_millis(),
            "five_signal consolidation daemon run complete"
        );

        Ok(())
    }
}
