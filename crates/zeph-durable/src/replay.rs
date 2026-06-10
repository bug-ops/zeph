// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The replay cursor that walks a resumed execution's journal.
//!
//! On resume, the program re-runs from the start and re-derives the same [`StepId`] sequence
//! (INV-2). For each step the [`DurableContext`](crate::DurableContext) asks the [`ReplayCursor`]
//! what the journal already knows about that position:
//!
//! - [`StepReplay::Result`] — a committed `StepResult` exists; the step's value is replayed and the
//!   operation closure is skipped.
//! - [`StepReplay::IntentOnly`] — only an `EffectIntent` exists (the *ambiguous window*); the step's
//!   [`OnAmbiguous`](crate::OnAmbiguous) policy decides what to do.
//! - [`StepReplay::Fresh`] — nothing is journaled for this position; the step runs for the first
//!   time. The first `Fresh` step is the resume point.
//!
//! # Bounded memory
//!
//! The cursor reads the journal in step-range *segments* (default
//! [`DEFAULT_SEGMENT_STEPS`]) via [`Journal::read_execution_range`], prefetching one segment ahead
//! as replay advances rather than loading the whole journal — `O(segment)` resident memory for a
//! resume (NFR-DE-02). Each step is journaled at most twice (an intent and a result), so a segment
//! reads `2 × segment_steps` rows; the last (possibly truncated) step group of a full batch is
//! deferred to the next read so a step is never observed half-loaded. A looked-up step is removed
//! from the resident window, so consumed entries do not accumulate.
//!
//! The cursor is consulted only on resume; a fresh execution never touches it.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::Instrument as _;

use crate::backend::DurableBackendEnum;
use crate::error::DurableError;
use crate::ids::{ExecutionId, StepId};
use crate::journal::{EntryKind, Journal as _, JournalEntry};

/// Default number of steps the cursor prefetches per segment read.
pub(crate) const DEFAULT_SEGMENT_STEPS: u32 = 100;

/// What the journal knows about a [`StepId`] on resume.
#[derive(Debug)]
pub(crate) enum StepReplay {
    /// A committed `StepResult` for this step (payload already opened by the backend).
    Result(JournalEntry),
    /// Only an `EffectIntent` for this step — the ambiguous window.
    IntentOnly(JournalEntry),
    /// Nothing journaled for this step: run it fresh.
    Fresh,
}

/// The journaled entries observed for a single step position.
#[derive(Debug, Default)]
struct LoadedStep {
    result: Option<JournalEntry>,
    intent: Option<JournalEntry>,
}

/// Interior, lock-guarded loading state of a [`ReplayCursor`].
#[derive(Debug)]
struct CursorState {
    /// Resident window of loaded steps, keyed by `StepId` value; entries are removed on lookup.
    loaded: BTreeMap<u32, LoadedStep>,
    /// The next step position a segment read should start from.
    next_step_to_load: u32,
    /// Whether the journal has been read to its end (a short final segment was seen).
    exhausted: bool,
    /// Whether the folded-step checkpoint preload has run (once, before the first segment read).
    checkpoints_preloaded: bool,
}

/// A forward-walking, segment-buffered view of a resumed execution's journal.
///
/// Built once when the execution is opened for resume and consulted per step. Cloning is not
/// supported — the cursor is owned by its [`DurableContext`](crate::DurableContext).
#[derive(Debug)]
pub(crate) struct ReplayCursor {
    backend: Arc<DurableBackendEnum>,
    execution_id: ExecutionId,
    segment_steps: u32,
    // A tokio async mutex protecting the resident window and loader bookkeeping.  The lock is
    // never held across I/O: each I/O call snapshots the decision fields, drops the guard,
    // performs the async read, re-acquires the guard, and merges the result.  Concurrent callers
    // are handled by the idempotency guard in `load_segment_from` and a double-check in the
    // checkpoint preload — no deadlock is possible because no other lock is taken while it is held.
    state: Mutex<CursorState>,
}

impl ReplayCursor {
    /// Build a cursor over `execution_id`, prefetching `segment_steps` steps per read.
    ///
    /// Construction performs no I/O; the first segment is read lazily on the first
    /// [`lookup`](ReplayCursor::lookup).
    pub(crate) fn new(
        backend: Arc<DurableBackendEnum>,
        execution_id: ExecutionId,
        segment_steps: u32,
    ) -> Self {
        let _span = tracing::info_span!(
            "durable.replay.cursor.build",
            execution_id = %execution_id.as_uuid(),
        )
        .entered();
        Self {
            backend,
            execution_id,
            segment_steps: segment_steps.max(1),
            state: Mutex::new(CursorState {
                loaded: BTreeMap::new(),
                next_step_to_load: 0,
                exhausted: false,
                checkpoints_preloaded: false,
            }),
        }
    }

    /// Row budget for one segment read (each step is journaled at most twice).
    fn segment_rows(&self) -> usize {
        usize::try_from(self.segment_steps)
            .unwrap_or(usize::MAX / 2)
            .saturating_mul(2)
    }

    /// Return what the journal knows about `step_id`, consuming it from the resident window.
    ///
    /// # Errors
    ///
    /// Returns a [`DurableError`] if a segment read fails or a stored entry cannot be decoded.
    pub(crate) async fn lookup(&self, step_id: StepId) -> Result<StepReplay, DurableError> {
        let step = step_id.value();
        self.ensure_loaded_through(step).await?;
        let entry = self.state.lock().await.loaded.remove(&step);
        Ok(match entry {
            Some(LoadedStep {
                result: Some(result),
                ..
            }) => StepReplay::Result(result),
            Some(LoadedStep {
                result: None,
                intent: Some(intent),
            }) => StepReplay::IntentOnly(intent),
            Some(LoadedStep {
                result: None,
                intent: None,
            })
            | None => StepReplay::Fresh,
        })
    }

    /// Read forward until `step` is covered or the journal is exhausted.
    ///
    /// Each I/O operation (checkpoint preload, segment read) is performed without holding the
    /// state lock: the lock is taken only to read the decision fields and to merge the result.
    async fn ensure_loaded_through(&self, step: u32) -> Result<(), DurableError> {
        // Checkpoint preload: check without lock, then perform I/O, then merge.
        let needs_checkpoint = !self.state.lock().await.checkpoints_preloaded;
        if needs_checkpoint {
            let entries = self
                .backend
                .read_checkpoints(self.execution_id)
                .instrument(tracing::info_span!(
                    "durable.replay.cursor.preload",
                    execution_id = %self.execution_id.as_uuid(),
                ))
                .await?;
            let mut state = self.state.lock().await;
            // Guard against a concurrent caller that already ran the preload.
            if !state.checkpoints_preloaded {
                state.checkpoints_preloaded = true;
                for entry in entries {
                    insert_entry(&mut state, entry);
                }
            }
        }

        loop {
            // Snapshot decision fields without holding the lock across I/O.
            let (exhausted, next_step_to_load) = {
                let state = self.state.lock().await;
                (state.exhausted, state.next_step_to_load)
            };
            if exhausted || next_step_to_load > step {
                break;
            }
            self.load_segment_from(next_step_to_load).await?;
        }
        Ok(())
    }

    /// Read one segment starting at `from` and fold the result into the resident window.
    ///
    /// All async I/O happens before the state lock is re-acquired for the merge.
    async fn load_segment_from(&self, from: u32) -> Result<(), DurableError> {
        let limit = self.segment_rows();
        let rows = async {
            let rows = self
                .backend
                .read_execution_range(self.execution_id, from, limit)
                .await?;
            tracing::Span::current().record("count", rows.len());
            Ok::<_, DurableError>(rows)
        }
        .instrument(tracing::info_span!(
            "durable.replay.cursor.read_segment",
            from_step_id = from,
            count = tracing::field::Empty,
        ))
        .await?;

        let mut state = self.state.lock().await;

        // Another concurrent caller may have already loaded this segment; skip if so.
        if state.next_step_to_load != from {
            return Ok(());
        }

        if rows.len() < limit {
            // A short batch is the journal's tail: there is nothing past it to truncate.
            let mut max_step = from;
            for entry in rows {
                max_step = max_step.max(entry.step_id.value());
                insert_entry(&mut state, entry);
            }
            state.exhausted = true;
            state.next_step_to_load = max_step.saturating_add(1);
            return Ok(());
        }

        // A full batch may have split the final step group across the row limit. Defer that group
        // and re-read it next time so a step is never observed with only part of its entries —
        // unless the whole batch is a single step (impossible with ≥2-row budget per step), in
        // which case insert it to guarantee forward progress.
        let min_step = rows.iter().map(|e| e.step_id.value()).min().unwrap_or(from);
        let max_step = rows.iter().map(|e| e.step_id.value()).max().unwrap_or(from);
        if min_step == max_step {
            for entry in rows {
                insert_entry(&mut state, entry);
            }
            state.next_step_to_load = max_step.saturating_add(1);
        } else {
            for entry in rows {
                if entry.step_id.value() == max_step {
                    continue;
                }
                insert_entry(&mut state, entry);
            }
            state.next_step_to_load = max_step;
        }
        Ok(())
    }
}

/// Fold one journal entry into the resident window, keyed by its step position.
///
/// Only the two step-bearing kinds are tracked; this revision journals no others, so any other kind
/// is ignored defensively.
fn insert_entry(state: &mut CursorState, entry: JournalEntry) {
    let step = entry.step_id.value();
    match entry.entry {
        EntryKind::StepResult { .. } => state.loaded.entry(step).or_default().result = Some(entry),
        EntryKind::EffectIntent { .. } => {
            state.loaded.entry(step).or_default().intent = Some(entry);
        }
        EntryKind::PromiseCreated { .. }
        | EntryKind::PromiseResolved { .. }
        | EntryKind::TimerArmed { .. }
        | EntryKind::TimerFired { .. }
        | EntryKind::Checkpoint { .. } => {}
    }
}

#[cfg(all(test, feature = "sqlite", not(feature = "postgres")))]
mod tests {
    use super::*;
    use crate::backend::local::LocalBackend;
    use crate::effect::EffectClass;
    use crate::ids::{ExecutionKind, IdempotencyKey};
    use bytes::Bytes;

    async fn backend_with(steps: &[(u32, bool)]) -> (Arc<DurableBackendEnum>, ExecutionId) {
        let local = LocalBackend::open(":memory:", 1_048_576).await.unwrap();
        local.init().await.unwrap();
        let exec = ExecutionId::new();
        local
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        for &(step, guarded) in steps {
            let step_id = StepId::new(step);
            let idem = IdempotencyKey::derive(exec, step_id, b"op");
            if guarded {
                local
                    .append(JournalEntry {
                        seq: None,
                        execution_id: exec,
                        kind: ExecutionKind::AgentTurn,
                        step_id,
                        entry: EntryKind::EffectIntent {
                            idempotency_key: idem,
                            effect: EffectClass::ExactlyOnceGuarded,
                            hmac: None,
                        },
                        created_at_ms: 0,
                    })
                    .await
                    .unwrap();
            }
            local
                .append(JournalEntry {
                    seq: None,
                    execution_id: exec,
                    kind: ExecutionKind::AgentTurn,
                    step_id,
                    entry: EntryKind::StepResult {
                        idempotency_key: idem,
                        payload: Bytes::from_static(b"v"),
                        effect: if guarded {
                            EffectClass::ExactlyOnceGuarded
                        } else {
                            EffectClass::Idempotent
                        },
                        payload_version: 1,
                    },
                    created_at_ms: 0,
                })
                .await
                .unwrap();
        }
        (Arc::new(DurableBackendEnum::Local(Arc::new(local))), exec)
    }

    async fn intent_only(step: u32) -> (Arc<DurableBackendEnum>, ExecutionId) {
        let local = LocalBackend::open(":memory:", 1_048_576).await.unwrap();
        local.init().await.unwrap();
        let exec = ExecutionId::new();
        local
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let step_id = StepId::new(step);
        local
            .append(JournalEntry {
                seq: None,
                execution_id: exec,
                kind: ExecutionKind::AgentTurn,
                step_id,
                entry: EntryKind::EffectIntent {
                    idempotency_key: IdempotencyKey::derive(exec, step_id, b"op"),
                    effect: EffectClass::ExactlyOnceGuarded,
                    hmac: None,
                },
                created_at_ms: 0,
            })
            .await
            .unwrap();
        (Arc::new(DurableBackendEnum::Local(Arc::new(local))), exec)
    }

    #[tokio::test]
    async fn lookup_classifies_result_intent_and_fresh() {
        let (backend, exec) = backend_with(&[(0, false), (1, true)]).await;
        let cursor = ReplayCursor::new(backend, exec, DEFAULT_SEGMENT_STEPS);

        assert!(matches!(
            cursor.lookup(StepId::new(0)).await.unwrap(),
            StepReplay::Result(_)
        ));
        // Step 1 has both an intent and a result → the result wins.
        assert!(matches!(
            cursor.lookup(StepId::new(1)).await.unwrap(),
            StepReplay::Result(_)
        ));
        // Step 2 was never journaled → fresh.
        assert!(matches!(
            cursor.lookup(StepId::new(2)).await.unwrap(),
            StepReplay::Fresh
        ));
    }

    #[tokio::test]
    async fn lookup_reports_ambiguous_window_intent_only() {
        let (backend, exec) = intent_only(0).await;
        let cursor = ReplayCursor::new(backend, exec, DEFAULT_SEGMENT_STEPS);
        assert!(matches!(
            cursor.lookup(StepId::new(0)).await.unwrap(),
            StepReplay::IntentOnly(_)
        ));
    }

    #[tokio::test]
    async fn segmented_reads_cover_a_long_journal() {
        // 25 idempotent steps with a tiny 2-step segment forces many segment reads, exercising the
        // defer-last-step boundary handling.
        let steps: Vec<(u32, bool)> = (0..25).map(|s| (s, false)).collect();
        let (backend, exec) = backend_with(&steps).await;
        let cursor = ReplayCursor::new(backend, exec, 2);
        for step in 0..25 {
            assert!(
                matches!(
                    cursor.lookup(StepId::new(step)).await.unwrap(),
                    StepReplay::Result(_)
                ),
                "step {step} should replay from the journal"
            );
        }
        assert!(matches!(
            cursor.lookup(StepId::new(25)).await.unwrap(),
            StepReplay::Fresh
        ));
    }

    #[tokio::test]
    async fn out_of_order_lookups_within_a_segment_resolve() {
        let steps: Vec<(u32, bool)> = (0..8).map(|s| (s, false)).collect();
        let (backend, exec) = backend_with(&steps).await;
        let cursor = ReplayCursor::new(backend, exec, DEFAULT_SEGMENT_STEPS);
        // Look up a higher step first, then a lower one — both must still resolve.
        assert!(matches!(
            cursor.lookup(StepId::new(5)).await.unwrap(),
            StepReplay::Result(_)
        ));
        assert!(matches!(
            cursor.lookup(StepId::new(2)).await.unwrap(),
            StepReplay::Result(_)
        ));
    }
}
