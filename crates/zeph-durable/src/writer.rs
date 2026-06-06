// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The background journal-writer actor.
//!
//! Every durable append routes through a single [`JournalWriter`] task so the calling path never
//! blocks on a database write and writes are serialized into a monotonic [`JournalSeq`]. Callers
//! hold a cheap, cloneable [`JournalWriterHandle`] and choose one of two durability classes:
//!
//! - **Buffered** ([`append_buffered`](JournalWriterHandle::append_buffered)) — fire-and-forget for
//!   `Idempotent`/`AtLeastOnce` effects. Entries accumulate and group-commit on a flush interval,
//!   amortizing the WAL fsync. On a full channel the entry is dropped with a `WARN`: a lost buffered
//!   entry simply re-runs on resume, which is safe by class definition (the durability-on-return
//!   guarantee, spec C-N1).
//! - **Acked** ([`append_acked`](JournalWriterHandle::append_acked)) — for `ExactlyOnceGuarded`
//!   intents and results. The writer flushes all causally-preceding buffered entries first (INV-4),
//!   commits the entry, and only then returns its [`JournalSeq`] over a oneshot. The call is bounded
//!   by `journal_ack_timeout_ms`: a stalled or unreachable writer yields
//!   [`DurableError::JournalUnavailable`] rather than blocking the agent loop (INV-12, FR-DE-11).
//!
//! # Supervision and restart
//!
//! [`JournalWriter::run`] is the actor future; the daemon spawns it under a `TaskSupervisor`
//! (spec-039). On every (re)start the writer reads `MAX(seq)` to anchor itself at the last
//! committed entry (FR-DE-12); because `seq` is database-assigned, resumed appends continue without
//! gap or duplication. The writer is bound to the local backend's `durable.db` — Restate journals
//! through its own SDK and does not use this actor.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::backend::local::LocalBackend;
use crate::config::DurableConfig;
use crate::error::DurableError;
use crate::ids::JournalSeq;
use crate::journal::{Journal, JournalEntry};

/// Bounded capacity of the writer's command channel (1024, per the spec's channel-capacity rule).
const CHANNEL_CAPACITY: usize = 1024;

/// Maximum entries buffered before an early group-commit, bounding the actor's memory between ticks.
const MAX_BATCH: usize = 256;

/// A command sent to the [`JournalWriter`] task.
///
/// Buffered appends are fire-and-forget; acked appends and flushes carry a oneshot the calling task
/// awaits. This is the actor's internal protocol — callers use [`JournalWriterHandle`].
pub(crate) enum JournalMsg {
    /// Append a `Idempotent`/`AtLeastOnce` entry; group-committed, droppable under backpressure.
    AppendBuffered(JournalEntry),
    /// Append an `ExactlyOnceGuarded` entry; flushed-before-committed and acknowledged by seq.
    AppendAcked(
        JournalEntry,
        oneshot::Sender<Result<JournalSeq, DurableError>>,
    ),
    /// Drain all buffered entries and acknowledge — a turn-boundary barrier.
    Flush(oneshot::Sender<()>),
}

/// The background actor that owns the write path to a [`LocalBackend`]'s `durable.db`.
///
/// Construct it with [`JournalWriter::new`] (which also returns the handle), then drive it with
/// [`JournalWriter::run`] on a supervised task.
#[derive(Debug)]
pub struct JournalWriter {
    backend: Arc<LocalBackend>,
    rx: mpsc::Receiver<JournalMsg>,
    flush_interval: Duration,
    max_batch: usize,
}

impl JournalWriter {
    /// Build the writer and its cloneable handle from a backend and the durable configuration.
    ///
    /// The flush interval and ACK timeout are taken from `config`; the channel is bounded at the
    /// spec capacity. Spawn [`JournalWriter::run`] to start processing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// use std::sync::Arc;
    /// use zeph_durable::{DurableConfig, LocalBackend, JournalWriter};
    ///
    /// let backend = Arc::new(LocalBackend::open("durable.db", 1_048_576).await?);
    /// backend.init().await?;
    /// let (writer, handle) = JournalWriter::new(backend, &DurableConfig::default());
    /// let task = tokio::spawn(writer.run());
    /// // ... use `handle` to append; drop all handles to stop the writer ...
    /// # let _ = (task, handle);
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn new(backend: Arc<LocalBackend>, config: &DurableConfig) -> (Self, JournalWriterHandle) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let handle = JournalWriterHandle {
            tx,
            ack_timeout: Duration::from_millis(config.journal_ack_timeout_ms),
        };
        let writer = Self {
            backend,
            rx,
            // Tokio's interval panics on a zero period; clamp to at least 1 ms.
            flush_interval: Duration::from_millis(config.journal_flush_interval_ms.max(1)),
            max_batch: MAX_BATCH,
        };
        (writer, handle)
    }

    /// Run the actor loop until every [`JournalWriterHandle`] is dropped.
    ///
    /// On entry the writer reads `MAX(seq)` to resume from the last committed entry (FR-DE-12). It
    /// then group-commits buffered entries on each flush tick (or when the batch fills), flushes
    /// before every acked commit (INV-4), and emits a `durable.journal.writer.queue_depth` gauge per
    /// commit cycle. When the channel closes it drains any remaining buffered entries and returns,
    /// so the supervisor can restart it cleanly.
    pub async fn run(mut self) {
        let resume = match self.backend.max_seq().await {
            Ok(seq) => seq,
            Err(error) => {
                tracing::error!(%error, "journal writer could not read resume seq; starting at 0");
                None
            }
        };
        tracing::info!(
            resume_seq = resume.map(JournalSeq::value),
            "journal writer started"
        );

        let mut buffer: Vec<JournalEntry> = Vec::new();
        let mut flush = tokio::time::interval(self.flush_interval);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                maybe_msg = self.rx.recv() => match maybe_msg {
                    Some(JournalMsg::AppendBuffered(entry)) => {
                        buffer.push(entry);
                        if buffer.len() >= self.max_batch {
                            self.flush_buffer(&mut buffer).await;
                        }
                    }
                    Some(JournalMsg::AppendAcked(entry, reply)) => {
                        // INV-4: every causally-preceding buffered entry is durable before the
                        // exactly-once entry commits.
                        self.flush_buffer(&mut buffer).await;
                        let result = self.backend.append(entry).await;
                        let _ = reply.send(result);
                    }
                    Some(JournalMsg::Flush(reply)) => {
                        self.flush_buffer(&mut buffer).await;
                        let _ = reply.send(());
                    }
                    None => {
                        self.flush_buffer(&mut buffer).await;
                        break;
                    }
                },
                _ = flush.tick() => {
                    self.flush_buffer(&mut buffer).await;
                }
            }
        }
        tracing::info!("journal writer stopped");
    }

    /// Group-commit and clear the buffer, emitting the queue-depth gauge for the cycle.
    ///
    /// A failed group-commit drops the buffered entries with a `WARN` (they re-run safely on
    /// resume) rather than wedging the actor.
    async fn flush_buffer(&self, buffer: &mut Vec<JournalEntry>) {
        if buffer.is_empty() {
            return;
        }
        let depth = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        metrics::gauge!("durable.journal.writer.queue_depth").set(f64::from(depth));
        if let Err(error) = self.backend.append_batch(buffer).await {
            tracing::warn!(
                %error,
                dropped = buffer.len(),
                "journal group-commit failed; buffered entries dropped (re-run safely on resume)"
            );
        }
        buffer.clear();
    }
}

/// A cheap, cloneable handle to a [`JournalWriter`].
///
/// Cloning shares the same underlying channel; the writer stops once the last handle is dropped.
#[derive(Clone, Debug)]
pub struct JournalWriterHandle {
    tx: mpsc::Sender<JournalMsg>,
    ack_timeout: Duration,
}

impl JournalWriterHandle {
    /// Enqueue a buffered, fire-and-forget append.
    ///
    /// Returns immediately. On a full channel the entry is dropped with a `WARN` (acceptable for
    /// `Idempotent`/`AtLeastOnce` effects, which re-run safely on resume); on a stopped writer it is
    /// likewise dropped. Use [`append_acked`](Self::append_acked) when durability-on-return matters.
    pub fn append_buffered(&self, entry: JournalEntry) {
        match self.tx.try_send(JournalMsg::AppendBuffered(entry)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    "journal writer channel full; dropping buffered entry (re-runs safely on resume)"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("journal writer stopped; dropping buffered entry");
            }
        }
    }

    /// Append an exactly-once entry and await its committed [`JournalSeq`].
    ///
    /// The writer flushes all causally-preceding buffered entries before committing this one
    /// (INV-4). The whole round-trip is bounded by `journal_ack_timeout_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::JournalUnavailable`] if the writer does not acknowledge within the
    /// timeout or is unreachable, and propagates a backend [`DurableError`] if the commit itself
    /// fails. The caller never blocks indefinitely (INV-12).
    pub async fn append_acked(&self, entry: JournalEntry) -> Result<JournalSeq, DurableError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let send_and_wait = async {
            self.tx
                .send(JournalMsg::AppendAcked(entry, reply_tx))
                .await
                .map_err(|_| DurableError::JournalUnavailable)?;
            match reply_rx.await {
                Ok(result) => result,
                Err(_) => Err(DurableError::JournalUnavailable),
            }
        };
        tokio::time::timeout(self.ack_timeout, send_and_wait)
            .await
            .unwrap_or(Err(DurableError::JournalUnavailable))
    }

    /// Drain all buffered entries to the database and await confirmation — a turn-boundary barrier.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::JournalUnavailable`] if the writer does not confirm within the
    /// timeout or is unreachable.
    pub async fn flush(&self) -> Result<(), DurableError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let send_and_wait = async {
            self.tx
                .send(JournalMsg::Flush(reply_tx))
                .await
                .map_err(|_| DurableError::JournalUnavailable)?;
            reply_rx.await.map_err(|_| DurableError::JournalUnavailable)
        };
        tokio::time::timeout(self.ack_timeout, send_and_wait)
            .await
            .unwrap_or(Err(DurableError::JournalUnavailable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::EffectClass;
    use crate::ids::{ExecutionId, ExecutionKind, IdempotencyKey, StepId};
    use crate::journal::EntryKind;
    use bytes::Bytes;

    fn step_result(exec: ExecutionId, step: u32, payload: &[u8]) -> JournalEntry {
        let step_id = StepId::new(step);
        JournalEntry {
            seq: None,
            execution_id: exec,
            kind: ExecutionKind::AgentTurn,
            step_id,
            entry: EntryKind::StepResult {
                idempotency_key: IdempotencyKey::derive(exec, step_id, b"tool:read"),
                payload: Bytes::copy_from_slice(payload),
                effect: EffectClass::Idempotent,
                payload_version: 1,
            },
            created_at_ms: 100,
        }
    }

    #[tokio::test]
    async fn append_acked_times_out_when_writer_is_stalled() {
        // A live channel whose receiver is never polled: the oneshot is never answered, so the
        // bounded ACK wait must elapse and surface JournalUnavailable rather than block forever.
        let (tx, _rx) = mpsc::channel(4);
        let handle = JournalWriterHandle {
            tx,
            ack_timeout: Duration::from_millis(50),
        };
        let result = handle
            .append_acked(step_result(ExecutionId::new(), 0, b"x"))
            .await;
        assert!(matches!(result, Err(DurableError::JournalUnavailable)));
    }

    #[tokio::test]
    async fn append_acked_errors_when_writer_is_gone() {
        // Dropping the receiver closes the channel; the send fails fast (no need to wait the timeout).
        let (tx, rx) = mpsc::channel(4);
        drop(rx);
        let handle = JournalWriterHandle {
            tx,
            ack_timeout: Duration::from_secs(30),
        };
        let result = handle
            .append_acked(step_result(ExecutionId::new(), 0, b"x"))
            .await;
        assert!(matches!(result, Err(DurableError::JournalUnavailable)));
    }

    #[tokio::test]
    async fn append_buffered_drops_on_full_channel_without_blocking() {
        let (tx, mut rx) = mpsc::channel(2);
        let handle = JournalWriterHandle {
            tx,
            ack_timeout: Duration::from_millis(50),
        };
        let exec = ExecutionId::new();
        // Three buffered sends into a capacity-2 channel: the third is dropped with a WARN, never
        // blocks, and never errors (acceptable for re-runnable buffered entries).
        handle.append_buffered(step_result(exec, 0, b"a"));
        handle.append_buffered(step_result(exec, 1, b"b"));
        handle.append_buffered(step_result(exec, 2, b"c"));

        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 2, "the over-capacity buffered entry is dropped");
    }

    #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
    mod with_backend {
        use super::*;
        use crate::DurableConfig;
        use crate::backend::local::LocalBackend;
        use std::sync::Arc;

        async fn mem_backend() -> Arc<LocalBackend> {
            let backend = LocalBackend::open(":memory:", 1_048_576).await.unwrap();
            backend.init().await.unwrap();
            Arc::new(backend)
        }

        fn fast_config() -> DurableConfig {
            DurableConfig {
                journal_flush_interval_ms: 5,
                journal_ack_timeout_ms: 2000,
                ..DurableConfig::default()
            }
        }

        #[tokio::test]
        async fn writer_group_commits_buffered_and_acks_exactly_once() {
            let backend = mem_backend().await;
            let exec = ExecutionId::new();
            backend
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap();

            let (writer, handle) = JournalWriter::new(backend.clone(), &fast_config());
            let task = tokio::spawn(writer.run());

            handle.append_buffered(step_result(exec, 0, b"a"));
            handle.append_buffered(step_result(exec, 1, b"b"));
            // The acked append flushes the two buffered entries first (INV-4), then commits.
            let seq = handle
                .append_acked(step_result(exec, 2, b"c"))
                .await
                .unwrap();
            assert!(seq.value() >= 1);
            handle.flush().await.unwrap();

            let entries = backend.read_execution(exec).await.unwrap();
            assert_eq!(
                entries.len(),
                3,
                "all buffered and acked entries are durable"
            );

            drop(handle);
            task.await.unwrap();
        }

        #[tokio::test]
        async fn writer_resumes_from_max_seq_after_restart() {
            let backend = mem_backend().await;
            let exec = ExecutionId::new();
            backend
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap();
            let config = fast_config();

            // First writer commits three acked entries (seq 1, 2, 3), then stops.
            let (writer1, handle1) = JournalWriter::new(backend.clone(), &config);
            let task1 = tokio::spawn(writer1.run());
            for step in 0..3 {
                handle1
                    .append_acked(step_result(exec, step, b"x"))
                    .await
                    .unwrap();
            }
            drop(handle1);
            task1.await.unwrap();
            assert_eq!(backend.max_seq().await.unwrap(), Some(JournalSeq::new(3)));

            // A restarted writer resumes from MAX(seq); the next commit continues without a gap.
            let (writer2, handle2) = JournalWriter::new(backend.clone(), &config);
            let task2 = tokio::spawn(writer2.run());
            let seq4 = handle2
                .append_acked(step_result(exec, 3, b"y"))
                .await
                .unwrap();
            assert_eq!(
                seq4.value(),
                4,
                "resumed appends continue with neither gap nor duplication"
            );

            drop(handle2);
            task2.await.unwrap();
        }
    }
}
