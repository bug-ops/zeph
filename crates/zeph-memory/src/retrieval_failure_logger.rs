// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Async fire-and-forget logger for memory retrieval failure events.
//!
//! [`RetrievalFailureLogger`] owns a bounded mpsc sender. Callers invoke
//! [`RetrievalFailureLogger::log`] on the hot path without blocking. A
//! background task coalesces records into batches and flushes them to `SQLite`.

use std::time::Duration;

use tokio::sync::mpsc;
use tracing::Instrument as _;

use crate::store::SqliteStore;
use crate::store::retrieval_failures::RetrievalFailureRecord;

const QUERY_TEXT_MAX_CHARS: usize = 512;
const ERROR_CONTEXT_MAX_CHARS: usize = 256;
/// How often to check for a cleanup opportunity (every N flushes).
const CLEANUP_FLUSH_INTERVAL: u32 = 500;

/// Async background writer that batches retrieval failure records to `SQLite`.
///
/// Construct with [`RetrievalFailureLogger::new`] and call [`RetrievalFailureLogger::log`]
/// from the recall hot path. Records are sent via a bounded channel; if the channel is
/// full the record is silently dropped (zero hot-path latency, per INV-1).
///
/// Fields are `Option` so that [`shutdown`](Self::shutdown) can take them without a
/// move-out-of-`Drop` conflict, and so the `Drop` impl can abort any task not yet drained.
/// `tx` is declared before `handle` to ensure the channel is closed before the handle is
/// dropped, which allows the background task to exit cleanly when `Drop` fires.
pub struct RetrievalFailureLogger {
    // tx MUST be declared before handle — drop order closes the channel before the handle.
    tx: Option<mpsc::Sender<RetrievalFailureRecord>>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl RetrievalFailureLogger {
    /// Spawn the background writer task and return a logger handle.
    ///
    /// `batch_size` records are flushed at once, or after `flush_interval` elapses,
    /// whichever comes first. Old records are purged every `CLEANUP_FLUSH_INTERVAL`
    /// batch flushes according to `retention_days`.
    #[must_use]
    pub fn new(
        sqlite: SqliteStore,
        channel_capacity: usize,
        batch_size: usize,
        flush_interval: Duration,
        retention_days: u32,
    ) -> Self {
        let (tx, rx) = mpsc::channel(channel_capacity);
        let handle = tokio::spawn(writer_task(
            sqlite,
            rx,
            batch_size,
            flush_interval,
            retention_days,
        ));
        Self {
            tx: Some(tx),
            handle: Some(handle),
        }
    }

    /// Queue a retrieval failure record for async persistence.
    ///
    /// Both `query_text` (512 chars) and `error_context` (256 chars) are truncated
    /// before enqueueing to bound in-channel memory usage (INV-3). If the channel is
    /// full the record is dropped and a debug message is emitted (INV-1).
    pub fn log(&self, mut record: RetrievalFailureRecord) {
        let _span = tracing::debug_span!("memory.retrieval_failure.log").entered();
        if record.query_text.chars().count() > QUERY_TEXT_MAX_CHARS {
            record.query_text = record
                .query_text
                .chars()
                .take(QUERY_TEXT_MAX_CHARS)
                .collect();
        }
        if let Some(ref mut ctx) = record.error_context
            && ctx.chars().count() > ERROR_CONTEXT_MAX_CHARS
        {
            *ctx = ctx.chars().take(ERROR_CONTEXT_MAX_CHARS).collect();
        }
        if let Some(tx) = &self.tx
            && tx.try_send(record).is_err()
        {
            tracing::debug!("retrieval_failure_logger: channel full, dropping record");
        }
    }

    /// Shut down the background writer, draining any queued records.
    ///
    /// Closes the sender and waits for the background task to complete. Drop is
    /// best-effort only; call this method for a clean drain on process exit.
    pub async fn shutdown(mut self) {
        drop(self.tx.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for RetrievalFailureLogger {
    /// Abort the background writer task on drop.
    ///
    /// For a clean drain (flushing queued records) call [`RetrievalFailureLogger::shutdown`]
    /// explicitly before dropping. This impl ensures the task is not silently detached.
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

async fn writer_task(
    sqlite: SqliteStore,
    mut rx: mpsc::Receiver<RetrievalFailureRecord>,
    batch_size: usize,
    flush_interval: Duration,
    retention_days: u32,
) {
    let mut batch: Vec<RetrievalFailureRecord> = Vec::with_capacity(batch_size);
    let mut flush_counter: u32 = 0;

    loop {
        // Collect up to `batch_size` records or until the flush interval elapses.
        let deadline = tokio::time::sleep(flush_interval);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                biased;
                msg = rx.recv() => {
                    if let Some(record) = msg {
                        batch.push(record);
                        if batch.len() >= batch_size {
                            break;
                        }
                    } else {
                        // Sender dropped — drain remaining and exit.
                        flush_batch(&sqlite, &mut batch, &mut flush_counter, retention_days).await;
                        return;
                    }
                }
                () = &mut deadline => break,
            }
        }

        flush_batch(&sqlite, &mut batch, &mut flush_counter, retention_days).await;
    }
}

async fn flush_batch(
    sqlite: &SqliteStore,
    batch: &mut Vec<RetrievalFailureRecord>,
    flush_counter: &mut u32,
    retention_days: u32,
) {
    if batch.is_empty() {
        return;
    }
    let count = batch.len();
    tracing::debug!(count, "retrieval_failure_logger: flushing batch");
    let span = tracing::info_span!("memory.retrieval_failure.flush", count);
    let result = sqlite
        .record_retrieval_failures_batch(batch)
        .instrument(span)
        .await;
    if let Err(e) = result {
        tracing::warn!("retrieval_failure_logger: batch write failed: {e:#}");
    }
    batch.clear();

    *flush_counter = flush_counter.wrapping_add(1);
    if (*flush_counter).is_multiple_of(CLEANUP_FLUSH_INTERVAL)
        && let Err(e) = sqlite.purge_old_retrieval_failures(retention_days).await
    {
        tracing::debug!("retrieval_failure_logger: cleanup failed: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::store::SqliteStore;
    use crate::store::retrieval_failures::{RetrievalFailureRecord, RetrievalFailureType};

    fn no_hit_record() -> RetrievalFailureRecord {
        RetrievalFailureRecord {
            conversation_id: None,
            turn_index: 0,
            failure_type: RetrievalFailureType::NoHit,
            retrieval_strategy: "semantic".into(),
            query_text: "hello world".into(),
            query_len: 11,
            top_score: None,
            confidence_threshold: None,
            result_count: 0,
            latency_ms: 5,
            edge_types: None,
            error_context: None,
        }
    }

    fn low_confidence_record(score: f32, threshold: f32) -> RetrievalFailureRecord {
        RetrievalFailureRecord {
            conversation_id: None,
            turn_index: 0,
            failure_type: RetrievalFailureType::LowConfidence,
            retrieval_strategy: "semantic".into(),
            query_text: "low confidence query".into(),
            query_len: 20,
            top_score: Some(score),
            confidence_threshold: Some(threshold),
            result_count: 3,
            latency_ms: 10,
            edge_types: None,
            error_context: None,
        }
    }

    #[tokio::test]
    async fn no_hit_failure_is_persisted() {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let logger =
            RetrievalFailureLogger::new(sqlite.clone(), 256, 16, Duration::from_millis(10), 90);
        logger.log(no_hit_record());
        logger.shutdown().await;

        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT failure_type FROM memory_retrieval_failures WHERE failure_type = 'no_hit'",
        )
        .fetch_all(sqlite.pool())
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "no_hit record must be persisted");
    }

    #[tokio::test]
    async fn low_confidence_failure_is_persisted() {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let logger =
            RetrievalFailureLogger::new(sqlite.clone(), 256, 16, Duration::from_millis(10), 90);
        logger.log(low_confidence_record(0.3, 0.7));
        logger.shutdown().await;

        let rows: Vec<(String, f32, f32)> = sqlx::query_as(
            "SELECT failure_type, top_score, confidence_threshold \
             FROM memory_retrieval_failures WHERE failure_type = 'low_confidence'",
        )
        .fetch_all(sqlite.pool())
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "low_confidence record must be persisted");
        let (_, top_score, threshold) = &rows[0];
        assert!((*top_score - 0.3_f32).abs() < 1e-5, "top_score must match");
        assert!(
            (*threshold - 0.7_f32).abs() < 1e-5,
            "confidence_threshold must match"
        );
    }

    #[tokio::test]
    async fn log_does_not_block_when_channel_is_full() {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        // capacity = 1 so the second send will be dropped
        let logger = RetrievalFailureLogger::new(sqlite.clone(), 1, 16, Duration::from_mins(1), 90);
        // First log fills the channel (capacity 1).
        logger.log(no_hit_record());
        // Second log must not block — try_send drops the record silently.
        let start = std::time::Instant::now();
        logger.log(no_hit_record());
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "log() must be non-blocking even when channel is full, elapsed={elapsed:?}"
        );
        logger.shutdown().await;
    }

    #[tokio::test]
    async fn query_text_truncated_to_512_chars() {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let logger =
            RetrievalFailureLogger::new(sqlite.clone(), 256, 16, Duration::from_millis(10), 90);
        let long_query = "x".repeat(1000);
        let mut record = no_hit_record();
        record.query_text = long_query;
        record.query_len = 1000;
        logger.log(record);
        logger.shutdown().await;

        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT query_text FROM memory_retrieval_failures")
                .fetch_all(sqlite.pool())
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].0.chars().count(),
            512,
            "query_text must be truncated to 512 chars"
        );
    }

    #[tokio::test]
    async fn logger_disabled_when_option_is_none() {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        // No logger constructed — simulate the disabled path via Option<RetrievalFailureLogger>.
        let logger: Option<RetrievalFailureLogger> = None;
        if let Some(l) = &logger {
            l.log(no_hit_record());
        }
        // Nothing written to the store.
        let rows: Vec<(i64,)> = sqlx::query_as("SELECT COUNT(*) FROM memory_retrieval_failures")
            .fetch_all(sqlite.pool())
            .await
            .unwrap();
        assert_eq!(
            rows[0].0, 0,
            "no records must be written when logger is None"
        );
    }

    #[tokio::test]
    async fn multiple_records_batch_flushed() {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let logger =
            RetrievalFailureLogger::new(sqlite.clone(), 256, 16, Duration::from_millis(10), 90);
        for _ in 0..5 {
            logger.log(no_hit_record());
        }
        logger.log(low_confidence_record(0.2, 0.8));
        logger.shutdown().await;

        let rows: Vec<(i64,)> = sqlx::query_as("SELECT COUNT(*) FROM memory_retrieval_failures")
            .fetch_all(sqlite.pool())
            .await
            .unwrap();
        assert_eq!(rows[0].0, 6, "all 6 records must be persisted in batch");
    }
}
