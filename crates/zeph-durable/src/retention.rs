// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Journal retention, the background prune sweep, and the checkpoint-fold codec.
//!
//! Two mechanisms bound journal growth, and neither runs on the step-dispatch hot path (spec NEVER):
//!
//! - **Background prune** — [`DurableRetentionService`] is a tokio task that wakes every
//!   `prune_interval_secs` and calls [`Journal::prune`](crate::Journal::prune), which deletes
//!   *terminal* executions older than their TTL in `prune_batch_size` batches, yielding between
//!   batches so a large sweep never holds the write lock.
//! - **In-execution checkpoint fold** — a long *in-flight* execution that crosses the soft step cap
//!   (90% of `max_steps_per_execution`) folds its committed-idempotent prefix into a single
//!   [`Checkpoint`](crate::EntryKind::Checkpoint) entry. The fold packs each folded step's replay
//!   value into the checkpoint snapshot and deletes the individual rows, so a resume still replays
//!   those steps from the snapshot rather than re-running them. The hard cap (100%) aborts the
//!   execution with [`DurableError::StepCapExceeded`].

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tracing::Instrument as _;

use crate::backend::DurableBackendEnum;
use crate::config::RetentionPolicy;
use crate::error::DurableError;
use crate::journal::Journal as _;

/// Wire-format version for the checkpoint snapshot encoding.
const CHECKPOINT_FORMAT_V1: u8 = 1;

/// One step's replay value, folded into a checkpoint snapshot.
///
/// The fold preserves exactly what a resume needs to replay the step without re-running its
/// operation: the [`IdempotencyKey`](crate::IdempotencyKey) bytes (so the replay-divergence guard
/// still matches, INV-3), the payload wire-format version, and the *plaintext* result bytes (the
/// snapshot as a whole is AEAD-sealed by the backend, so individual step payloads need no further
/// sealing inside it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FoldedStep {
    /// Position of the folded step within its execution.
    pub(crate) step_id: u32,
    /// The step's 32-byte idempotency key, for the divergence guard on replay.
    pub(crate) idem_key: [u8; 32],
    /// Wire-format version of the result payload.
    pub(crate) payload_version: u8,
    /// Plaintext result bytes.
    pub(crate) payload: Bytes,
}

/// A decoded checkpoint snapshot: the folded prefix of an execution, in step order.
pub(crate) type CheckpointSnapshot = Vec<FoldedStep>;

/// Per-step fixed framing overhead in the encoded snapshot (step + version + `idem_key` + len).
const FOLDED_STEP_OVERHEAD: usize = 4 + 1 + 32 + 4;

/// Encoded size one [`FoldedStep`] contributes (framing + payload).
pub(crate) fn folded_step_encoded_len(payload_len: usize) -> usize {
    FOLDED_STEP_OVERHEAD.saturating_add(payload_len)
}

/// Serialize a checkpoint snapshot into a compact, self-describing byte buffer.
///
/// Layout: `version(1) || count(u32 le) || [ step(u32 le) version(1) idem_key(32) len(u32 le)
/// payload(len) ]*`. Fixed-width framing keeps the encoding injective and the per-step size
/// predictable, so the backend can cut the fold at the payload ceiling without trial encoding.
pub(crate) fn encode_checkpoint(steps: &[FoldedStep]) -> Vec<u8> {
    let total: usize = steps
        .iter()
        .map(|s| folded_step_encoded_len(s.payload.len()))
        .sum();
    let mut out = Vec::with_capacity(5 + total);
    out.push(CHECKPOINT_FORMAT_V1);
    out.extend_from_slice(&u32::try_from(steps.len()).unwrap_or(u32::MAX).to_le_bytes());
    for step in steps {
        out.extend_from_slice(&step.step_id.to_le_bytes());
        out.push(step.payload_version);
        out.extend_from_slice(&step.idem_key);
        out.extend_from_slice(
            &u32::try_from(step.payload.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        out.extend_from_slice(&step.payload);
    }
    out
}

/// Decode a checkpoint snapshot, failing closed on truncation or an unknown format version.
///
/// # Errors
///
/// Returns [`DurableError::Decode`] if the buffer is truncated, declares more steps than it
/// contains, or carries an unrecognized format version.
pub(crate) fn decode_checkpoint(bytes: &[u8]) -> Result<CheckpointSnapshot, DurableError> {
    let mut cursor = Reader::new(bytes);
    let version = cursor.u8()?;
    if version != CHECKPOINT_FORMAT_V1 {
        return Err(DurableError::Decode {
            context: "checkpoint snapshot has an unknown format version",
        });
    }
    let count = cursor.u32()? as usize;
    let mut steps = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let step_id = cursor.u32()?;
        let payload_version = cursor.u8()?;
        let idem_key = cursor.array32()?;
        let len = cursor.u32()? as usize;
        let payload = Bytes::copy_from_slice(cursor.take(len)?);
        steps.push(FoldedStep {
            step_id,
            idem_key,
            payload_version,
            payload,
        });
    }
    Ok(steps)
}

/// A bounds-checked forward reader over the snapshot buffer; every read fails closed on underrun.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DurableError> {
        let end = self.pos.checked_add(len).ok_or(DurableError::Decode {
            context: "checkpoint snapshot length overflow",
        })?;
        let slice = self.bytes.get(self.pos..end).ok_or(DurableError::Decode {
            context: "checkpoint snapshot is truncated",
        })?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, DurableError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, DurableError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn array32(&mut self) -> Result<[u8; 32], DurableError> {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.take(32)?);
        Ok(out)
    }
}

/// Compute the soft and hard step-cap thresholds for a `max_steps_per_execution` budget.
///
/// The soft threshold (90%) triggers a checkpoint fold; the hard threshold (the cap itself) aborts.
/// A `max` of zero disables both (returns `(u32::MAX, u32::MAX)`), so an unconfigured cap never folds
/// or aborts.
#[must_use]
pub(crate) fn step_cap_thresholds(max: u32) -> (u32, u32) {
    if max == 0 {
        return (u32::MAX, u32::MAX);
    }
    // `max * 9 / 10 <= max`, so the result always fits back into u32; the widening guards the
    // intermediate product against overflow.
    let soft = u32::try_from(u64::from(max) * 9 / 10).unwrap_or(max);
    (soft, max)
}

/// Background task that prunes terminal executions on a fixed interval.
///
/// Spawn [`DurableRetentionService::run`] on a supervised task (alongside the
/// [`JournalWriter`](crate::JournalWriter)). It owns no write path of its own — it calls
/// [`Journal::prune`](crate::Journal::prune) on the shared backend, which performs the batched delete
/// off the hot path.
#[derive(Debug)]
pub struct DurableRetentionService {
    backend: Arc<DurableBackendEnum>,
    policy: RetentionPolicy,
    interval: Duration,
}

impl DurableRetentionService {
    /// Build the service from the shared backend and the configured retention policy.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
    /// use std::sync::Arc;
    /// use zeph_durable::{DurableBackendEnum, DurableRetentionService, LocalBackend, RetentionPolicy};
    ///
    /// let backend = Arc::new(DurableBackendEnum::Local(Arc::new(
    ///     LocalBackend::open("durable.db", 1_048_576).await?,
    /// )));
    /// let service = DurableRetentionService::new(backend, RetentionPolicy::default());
    /// let task = tokio::spawn(service.run());
    /// # let _ = task;
    /// # Ok(()) }
    /// ```
    #[must_use]
    pub fn new(backend: Arc<DurableBackendEnum>, policy: RetentionPolicy) -> Self {
        let interval = Duration::from_secs(policy.prune_interval_secs.max(1));
        Self {
            backend,
            policy,
            interval,
        }
    }

    /// Run the prune loop until the task is aborted.
    ///
    /// Each tick prunes terminal executions older than their TTL; a prune failure is logged and the
    /// loop continues (a transient database error must not kill retention).
    #[tracing::instrument(name = "durable.retention.run", skip_all)]
    pub async fn run(self) {
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first immediate tick fires at startup; skip pruning on it so a just-launched daemon
        // does not sweep before the first real interval elapses.
        tick.tick().await;
        loop {
            tick.tick().await;
            async {
                match self.backend.prune(&self.policy).await {
                    Ok(deleted) => {
                        tracing::debug!(deleted, "durable retention prune sweep completed");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "durable retention prune sweep failed; will retry");
                    }
                }
            }
            .instrument(tracing::info_span!("durable.retention.run.iter"))
            .await;
        }
    }
}

/// Run one batched prune pass over the journal, deleting terminal executions past their TTL.
///
/// This is the shared body behind [`Journal::prune`](crate::Journal::prune) for the local backend.
/// It is a free function (rather than a method) so the backend can keep its prune implementation thin
/// while the batching/yielding policy lives next to the rest of retention. `delete_batch` performs
/// one bounded `DELETE` transaction and returns the rows it removed; the loop yields between batches
/// so a large sweep never monopolizes the runtime.
pub(crate) async fn prune_in_batches<F, Fut>(
    policy: &RetentionPolicy,
    now_ms: i64,
    delete_batch: F,
) -> Result<u64, DurableError>
where
    F: Fn(PruneCutoffs, u64) -> Fut,
    Fut: Future<Output = Result<u64, DurableError>>,
{
    let cutoffs = PruneCutoffs::from_policy(policy, now_ms);
    let batch = policy.prune_batch_size.max(1);
    let mut total = 0u64;
    let span = tracing::info_span!(
        "durable.journal.prune",
        deleted_count = tracing::field::Empty
    );
    async {
        loop {
            let deleted = delete_batch(cutoffs, batch).await?;
            total = total.saturating_add(deleted);
            if deleted < batch {
                break;
            }
            // Release the write lock and let other tasks run before the next batch.
            tokio::task::yield_now().await;
        }
        tracing::Span::current().record("deleted_count", total);
        Ok(total)
    }
    .instrument(span)
    .await
}

/// The absolute `finalized_at` cutoffs (Unix ms) below which a terminal execution is prunable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PruneCutoffs {
    /// Completed executions finalized at or before this instant are prunable.
    pub(crate) completed_before_ms: i64,
    /// Failed/aborted executions finalized at or before this instant are prunable.
    pub(crate) failed_before_ms: i64,
}

impl PruneCutoffs {
    pub(crate) fn from_policy(policy: &RetentionPolicy, now_ms: i64) -> Self {
        let completed =
            i64::try_from(policy.ttl_completed_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
        let failed = i64::try_from(policy.ttl_failed_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
        Self {
            completed_before_ms: now_ms.saturating_sub(completed),
            failed_before_ms: now_ms.saturating_sub(failed),
        }
    }
}

/// The largest payload the backend will pack into a single checkpoint snapshot.
///
/// The fold cuts its prefix at this ceiling so a checkpoint entry obeys the same `max_payload_bytes`
/// read/write guard as any other payload (INV-11). Steps that do not fit stay un-folded until a later
/// checkpoint.
#[must_use]
pub(crate) fn checkpoint_budget(max_payload_bytes: u64) -> usize {
    usize::try_from(max_payload_bytes).unwrap_or(usize::MAX)
}

/// Decide how many leading folded steps fit within the checkpoint payload budget.
///
/// Returns the count of steps from the front of `payload_lens` whose cumulative encoded size stays
/// within `budget` (including the 5-byte snapshot header). A single step larger than the whole budget
/// yields `0`, leaving it un-folded rather than producing an over-limit checkpoint.
#[must_use]
pub(crate) fn fold_prefix_len(payload_lens: &[usize], budget: usize) -> usize {
    let mut used = 5usize; // version + count header
    let mut taken = 0usize;
    for &len in payload_lens {
        let next = used.saturating_add(folded_step_encoded_len(len));
        if next > budget {
            break;
        }
        used = next;
        taken += 1;
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folded(step: u32, payload: &[u8]) -> FoldedStep {
        FoldedStep {
            step_id: step,
            idem_key: [u8::try_from(step % 256).unwrap_or(0); 32],
            payload_version: 1,
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn checkpoint_round_trips() {
        let steps = vec![
            folded(0, b"alpha"),
            folded(1, b""),
            folded(2, b"gamma-payload"),
        ];
        let encoded = encode_checkpoint(&steps);
        let decoded = decode_checkpoint(&encoded).unwrap();
        assert_eq!(decoded, steps);
    }

    #[test]
    fn decode_rejects_truncation() {
        let steps = vec![folded(0, b"data")];
        let mut encoded = encode_checkpoint(&steps);
        encoded.truncate(encoded.len() - 2);
        assert!(matches!(
            decode_checkpoint(&encoded),
            Err(DurableError::Decode { .. })
        ));
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let mut encoded = encode_checkpoint(&[folded(0, b"x")]);
        encoded[0] = 99;
        assert!(matches!(
            decode_checkpoint(&encoded),
            Err(DurableError::Decode { .. })
        ));
    }

    #[test]
    fn step_cap_thresholds_are_ninety_percent_and_full() {
        assert_eq!(step_cap_thresholds(10_000), (9_000, 10_000));
        assert_eq!(step_cap_thresholds(10), (9, 10));
        assert_eq!(step_cap_thresholds(0), (u32::MAX, u32::MAX));
    }

    #[test]
    fn fold_prefix_respects_budget() {
        // Each step encodes to 41 + payload; with a 4-byte payload that is 45 bytes + the 5-byte
        // header. A budget of 5 + 45 + 45 = 95 admits exactly two steps.
        let lens = vec![4, 4, 4, 4];
        assert_eq!(fold_prefix_len(&lens, 95), 2);
        // A step larger than the entire budget is left un-folded.
        assert_eq!(fold_prefix_len(&[10_000], 50), 0);
    }

    #[test]
    fn prune_cutoffs_subtract_ttl_from_now() {
        let policy = RetentionPolicy {
            ttl_completed_secs: 10,
            ttl_failed_secs: 20,
            ..RetentionPolicy::default()
        };
        let cutoffs = PruneCutoffs::from_policy(&policy, 100_000);
        assert_eq!(cutoffs.completed_before_ms, 90_000);
        assert_eq!(cutoffs.failed_before_ms, 80_000);
        assert_eq!(checkpoint_budget(1_048_576), 1_048_576);
    }

    #[tokio::test]
    async fn prune_in_batches_loops_until_drained_and_yields() {
        use std::cell::Cell;
        // Three full batches (500) then a short one (120) → four calls, totalling 1620.
        let remaining = Cell::new(1_620u64);
        let policy = RetentionPolicy::default();
        let total = prune_in_batches(&policy, 0, |_cutoffs, batch| {
            let deleted = remaining.get().min(batch);
            remaining.set(remaining.get() - deleted);
            async move { Ok(deleted) }
        })
        .await
        .unwrap();
        assert_eq!(total, 1_620);
        assert_eq!(remaining.get(), 0);
    }
}
