// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Persistent store for memory retrieval failure records.
//!
//! Failures are written via [`crate::RetrievalFailureLogger`], which batches records
//! asynchronously and inserts them in the background to avoid blocking the
//! recall hot path.

use super::SqliteStore;
use crate::error::MemoryError;
use zeph_db::sql;

/// Classification of a memory retrieval failure event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrievalFailureType {
    /// No results were returned for the query.
    NoHit,
    /// Results were returned but the top score was below the confidence threshold.
    LowConfidence,
    /// The recall operation did not complete within the configured timeout.
    Timeout,
    /// The recall backend returned an error.
    Error,
}

impl RetrievalFailureType {
    /// Returns the canonical string representation stored in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoHit => "no_hit",
            Self::LowConfidence => "low_confidence",
            Self::Timeout => "timeout",
            Self::Error => "error",
        }
    }
}

/// A single retrieval failure event to be persisted.
#[derive(Debug, Clone)]
pub struct RetrievalFailureRecord {
    /// Conversation this failure occurred in. `None` when persistence is not yet
    /// initialized (first-turn edge case).
    pub conversation_id: Option<crate::types::ConversationId>,
    /// Turn counter within the conversation. Use `0` when unavailable.
    pub turn_index: i64,
    /// How the recall failed.
    pub failure_type: RetrievalFailureType,
    /// Name of the retrieval strategy that was attempted.
    pub retrieval_strategy: String,
    /// The query text (truncated to 512 chars by [`crate::RetrievalFailureLogger::log`]).
    pub query_text: String,
    /// Byte length of the original query before any truncation.
    ///
    /// Note: `query_text` is truncated to 512 *chars* by [`crate::RetrievalFailureLogger::log`],
    /// so `query_len` may exceed `query_text.len()` for multibyte inputs.
    pub query_len: usize,
    /// Top score returned, if any results were produced.
    pub top_score: Option<f32>,
    /// Configured confidence threshold at failure time.
    pub confidence_threshold: Option<f32>,
    /// Number of results returned (0 for `NoHit`).
    pub result_count: usize,
    /// Wall-clock duration of the recall operation in milliseconds.
    pub latency_ms: u64,
    /// JSON-serialized list of graph edge types used (graph recall only).
    pub edge_types: Option<String>,
    /// Error message or timeout context for `Error`/`Timeout` variants.
    ///
    /// Truncated to 256 chars by [`crate::RetrievalFailureLogger::log`] to bound channel memory.
    pub error_context: Option<String>,
}

impl SqliteStore {
    /// Insert a single retrieval failure record.
    ///
    /// Prefer [`crate::RetrievalFailureLogger`] for hot-path inserts — this method is
    /// intended for tests and one-off writes.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the INSERT fails.
    pub async fn record_retrieval_failure(
        &self,
        r: &RetrievalFailureRecord,
    ) -> Result<(), MemoryError> {
        self.record_retrieval_failures_batch(std::slice::from_ref(r))
            .await
    }

    /// Batch-insert retrieval failure records in a single transaction.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if any INSERT fails.
    pub async fn record_retrieval_failures_batch(
        &self,
        records: &[RetrievalFailureRecord],
    ) -> Result<(), MemoryError> {
        if records.is_empty() {
            return Ok(());
        }
        let mut tx = zeph_db::begin_write(self.pool()).await?;
        for r in records {
            let conversation_id = r.conversation_id.map(|c| c.0);
            let failure_type = r.failure_type.as_str();
            #[allow(clippy::cast_possible_wrap)]
            let query_len = r.query_len as i64;
            #[allow(clippy::cast_possible_wrap)]
            let result_count = r.result_count as i64;
            let latency_ms = r.latency_ms.cast_signed();
            zeph_db::query(sql!(
                "INSERT INTO memory_retrieval_failures
                    (conversation_id, turn_index, failure_type, retrieval_strategy,
                     query_text, query_len, top_score, confidence_threshold,
                     result_count, latency_ms, edge_types, error_context)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ))
            .bind(conversation_id)
            .bind(r.turn_index)
            .bind(failure_type)
            .bind(&r.retrieval_strategy)
            .bind(&r.query_text)
            .bind(query_len)
            .bind(r.top_score)
            .bind(r.confidence_threshold)
            .bind(result_count)
            .bind(latency_ms)
            .bind(&r.edge_types)
            .bind(&r.error_context)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Delete records older than `retention_days` days.
    ///
    /// Called periodically by [`crate::RetrievalFailureLogger`]'s background task.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] if the DELETE fails.
    pub async fn purge_old_retrieval_failures(
        &self,
        retention_days: u32,
    ) -> Result<u64, MemoryError> {
        let cutoff = format!(
            "{}",
            (chrono::Utc::now() - chrono::Duration::days(i64::from(retention_days)))
                .format("%Y-%m-%d %H:%M:%S")
        );
        let rows = zeph_db::query(sql!(
            "DELETE FROM memory_retrieval_failures WHERE created_at < ?"
        ))
        .bind(cutoff)
        .execute(self.pool())
        .await?;
        Ok(rows.rows_affected())
    }
}
