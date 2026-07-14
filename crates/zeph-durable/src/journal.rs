// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The append-only journal abstraction and its data model.
//!
//! A [`Journal`] records the control flow of an execution as an ordered sequence of
//! [`JournalEntry`] values. Each entry is one [`EntryKind`] — a closed enum that makes illegal
//! states unrepresentable: control entries (effect intents, promise creation, timer arming) carry
//! no ciphertext payload field at all, so a "control entry with payload" cannot be constructed.
//!
//! This module defines the *types* only. The concrete journal backends, the writer actor, and the
//! replay cursor land in follow-up issues.

use std::future::Future;

use bytes::Bytes;

use crate::cipher::EntryKindTag;
use crate::config::RetentionPolicy;
use crate::effect::EffectClass;
use crate::error::DurableError;
use crate::ids::{
    ExecutionId, ExecutionKind, IdempotencyKey, JournalSeq, PromiseId, StepId, TimerId,
};

/// Terminal and in-flight status of a durable execution.
///
/// Maps one-to-one to the `status` column `CHECK` constraint
/// (`'running' | 'completed' | 'failed' | 'aborted'`).
///
/// # Examples
///
/// ```
/// use zeph_durable::ExecutionStatus;
///
/// assert_eq!(ExecutionStatus::Completed.as_str(), "completed");
/// assert!(ExecutionStatus::Running.is_running());
/// assert!(!ExecutionStatus::Failed.is_running());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// The execution is in flight.
    Running,
    /// The execution finished successfully.
    Completed,
    /// The execution ended in an error.
    Failed,
    /// The execution was discarded (e.g. after a replay divergence or step-cap abort).
    Aborted,
}

impl ExecutionStatus {
    /// Return the canonical string used in the `status` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }

    /// Reconstruct a status from its canonical `status`-column string.
    ///
    /// Returns `None` for an unrecognized tag. The `durable_executions.status` column carries a
    /// `CHECK` constraint over exactly these four values, so a `None` indicates schema corruption
    /// or drift rather than a routine miss; callers should fail closed.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }

    /// Whether the execution is still in flight (not yet in a terminal state).
    #[must_use]
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// The kind of a single journal entry.
///
/// A closed enum: an exhaustive `match` over its variants is required, which guarantees every
/// replay-relevant entry shape is handled. Only the variants that genuinely carry data
/// (`StepResult`, `PromiseResolved`, `Checkpoint`) own a `payload`/`snapshot` field; the control
/// entries hold identifiers and an optional row-level HMAC instead, so an illegal "control entry
/// with payload" is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// The committed result of a completed step. The `payload` is AEAD-sealed
    /// (`nonce || ciphertext || tag`).
    StepResult {
        /// Deduplication key for the step's effect.
        idempotency_key: IdempotencyKey,
        /// Sealed result bytes.
        payload: Bytes,
        /// How the step's side effect behaves under replay.
        effect: EffectClass,
        /// Wire-format version discriminator for the sealed payload.
        payload_version: u8,
    },
    /// An intent to run an exactly-once-guarded effect, journaled before the effect fires.
    EffectIntent {
        /// Deduplication key for the guarded effect.
        idempotency_key: IdempotencyKey,
        /// How the step's side effect behaves under replay.
        effect: EffectClass,
        /// Row-level HMAC for shared-DB / Restate deployments; `None` for single-user `SQLite`.
        hmac: Option<[u8; 32]>,
    },
    /// Creation of an external-completion promise.
    PromiseCreated {
        /// The new promise's identifier.
        promise_id: PromiseId,
        /// BLAKE3 hash of the 32-byte resolver token (the token itself is never journaled).
        resolver_token_hash: [u8; 32],
        /// Row-level HMAC for shared-DB / Restate deployments; `None` for single-user `SQLite`.
        hmac: Option<[u8; 32]>,
    },
    /// Resolution of a previously-created promise with its sealed result.
    PromiseResolved {
        /// The resolved promise's identifier.
        promise_id: PromiseId,
        /// Sealed resolution bytes.
        payload: Bytes,
    },
    /// A durable timer was armed to fire at a persisted instant.
    TimerArmed {
        /// The armed timer's identifier.
        timer_id: TimerId,
        /// Wake instant, as Unix epoch milliseconds.
        due_at_ms: i64,
        /// Row-level HMAC for shared-DB / Restate deployments; `None` for single-user `SQLite`.
        hmac: Option<[u8; 32]>,
    },
    /// A previously-armed timer fired.
    TimerFired {
        /// The fired timer's identifier.
        timer_id: TimerId,
    },
    /// A checkpoint fold that compacts the idempotent prefix up to a step.
    Checkpoint {
        /// All steps strictly below this id are folded into the snapshot.
        up_to_step: u32,
        /// Sealed snapshot bytes.
        snapshot: Bytes,
    },
}

impl EntryKind {
    /// Return the data-free [`EntryKindTag`] discriminator for this entry.
    ///
    /// This is the bridge a backend uses to build the [`PayloadAad`](crate::PayloadAad) for an
    /// entry without exposing the payload to the cipher binding logic.
    #[must_use]
    pub fn tag_enum(&self) -> EntryKindTag {
        match self {
            Self::StepResult { .. } => EntryKindTag::StepResult,
            Self::EffectIntent { .. } => EntryKindTag::EffectIntent,
            Self::PromiseCreated { .. } => EntryKindTag::PromiseCreated,
            Self::PromiseResolved { .. } => EntryKindTag::PromiseResolved,
            Self::TimerArmed { .. } => EntryKindTag::TimerArmed,
            Self::TimerFired { .. } => EntryKindTag::TimerFired,
            Self::Checkpoint { .. } => EntryKindTag::Checkpoint,
        }
    }

    /// Return the canonical string used in the `entry_kind` column.
    ///
    /// The `step_result` tag in particular is the predicate of the unique partial index that
    /// enforces "at most one committed result per step". Delegates to [`EntryKindTag::as_str`] so
    /// the column strings have a single source of truth.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        self.tag_enum().as_str()
    }

    /// Return the entry's [`IdempotencyKey`], for the two step-bearing kinds that carry one.
    ///
    /// The replay-divergence guard (INV-3) compares the journaled key of a `StepResult` /
    /// `EffectIntent` against the key freshly derived from the replayed descriptor; control and
    /// promise/timer entries have no idempotency key and return `None`.
    #[must_use]
    pub fn idempotency_key(&self) -> Option<IdempotencyKey> {
        match self {
            Self::StepResult {
                idempotency_key, ..
            }
            | Self::EffectIntent {
                idempotency_key, ..
            } => Some(*idempotency_key),
            Self::PromiseCreated { .. }
            | Self::PromiseResolved { .. }
            | Self::TimerArmed { .. }
            | Self::TimerFired { .. }
            | Self::Checkpoint { .. } => None,
        }
    }
}

/// One ordered entry in a journal.
///
/// `seq` is `None` before the entry is appended and `Some` once the database assigns its global
/// order. The remaining fields locate the entry within its execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// Global append order; `None` until the backend assigns it on append.
    pub seq: Option<JournalSeq>,
    /// The execution this entry belongs to.
    pub execution_id: ExecutionId,
    /// The category of the owning execution.
    pub kind: ExecutionKind,
    /// The step this entry is associated with.
    pub step_id: StepId,
    /// The entry payload.
    pub entry: EntryKind,
    /// Creation time, as Unix epoch milliseconds.
    pub created_at_ms: i64,
}

/// An append-only, ordered journal of execution control flow.
///
/// Implementations are `Send + Sync` and route all writes through a dedicated connection so that
/// appends are serialized. The returned futures are `Send`, so a journal can be shared across
/// spawned tasks; the trait is consumed via enum dispatch, never as a trait object.
pub trait Journal: Send + Sync {
    /// Append an entry and return its database-assigned global sequence number.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::JournalUnavailable`] if the write cannot be acknowledged in time,
    /// or [`DurableError::PayloadTooLarge`] if a payload exceeds the configured limit.
    fn append(
        &self,
        entry: JournalEntry,
    ) -> impl Future<Output = Result<JournalSeq, DurableError>> + Send;

    /// Read every entry of an execution in append order.
    ///
    /// Intended for short executions; long executions use [`Journal::read_execution_range`] to
    /// bound memory.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Decode`] if a stored entry cannot be decoded, or
    /// [`DurableError::JournalUnavailable`] if the journal cannot be read.
    fn read_execution(
        &self,
        id: ExecutionId,
    ) -> impl Future<Output = Result<Vec<JournalEntry>, DurableError>> + Send;

    /// Read up to `limit` entries of an execution starting at `from_step_id`.
    ///
    /// The replay cursor calls this repeatedly to walk a long execution with `O(segment)` memory.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Decode`] if a stored entry cannot be decoded, or
    /// [`DurableError::JournalUnavailable`] if the journal cannot be read.
    fn read_execution_range(
        &self,
        id: ExecutionId,
        from_step_id: u32,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<JournalEntry>, DurableError>> + Send;

    /// Transition an execution to a terminal status.
    ///
    /// Idempotent and safe to race: the transition only applies while the execution is still
    /// `running`, so calling this more than once for the same execution (e.g. a divergence-driven
    /// `Aborted` racing a caller's own `Completed`/`Failed`) is a no-op after the first call commits
    /// — whichever status lands first wins and is never overwritten by a later one.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::JournalUnavailable`] if the transition cannot be committed.
    fn finalize(
        &self,
        id: ExecutionId,
        status: ExecutionStatus,
    ) -> impl Future<Output = Result<(), DurableError>> + Send;

    /// Prune terminal executions according to `policy` and return the number of rows deleted.
    ///
    /// Runs exclusively on a background task — never on the dispatch hot path.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::JournalUnavailable`] if the prune sweep cannot complete.
    fn prune(
        &self,
        policy: &RetentionPolicy,
    ) -> impl Future<Output = Result<u64, DurableError>> + Send;

    /// Crash-orphan reclamation (#6254): flock-verify and hard-abort stale `running` rows.
    ///
    /// A `status='running'` row whose `updated_at` is older than `policy.stale_running_after_secs`
    /// is a sweep candidate; it is only hard-aborted after a non-blocking try-acquire of its
    /// INV-15 `ExecutionLock` succeeds — a live owner (`ExecutionLocked`) short-circuits to skip,
    /// since staleness alone never proves the owner is dead (INV-17). Runs exclusively on a
    /// background task, before [`Journal::prune`] on the same tick — never on the dispatch hot
    /// path.
    ///
    /// Returns the number of executions aborted. Returns `Ok(0)` without scanning when
    /// `policy.stale_running_after_secs == 0` (disabled), and `Ok(0)` with a warn-once log on
    /// backends without a `lock_dir` (`:memory:`, Postgres, non-Unix) — a documented no-op, never
    /// a staleness-only abort.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::JournalUnavailable`] if the sweep cannot complete.
    fn sweep_orphans(
        &self,
        policy: &RetentionPolicy,
    ) -> impl Future<Output = Result<u64, DurableError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ExecutionId;

    fn sample_entry(entry: EntryKind) -> JournalEntry {
        JournalEntry {
            seq: None,
            execution_id: ExecutionId::new(),
            kind: ExecutionKind::AgentTurn,
            step_id: StepId::new(0),
            entry,
            created_at_ms: 0,
        }
    }

    #[test]
    fn entry_kind_match_is_exhaustive() {
        let key = IdempotencyKey::derive(ExecutionId::new(), StepId::new(0), b"op");
        for entry in [
            EntryKind::StepResult {
                idempotency_key: key,
                payload: Bytes::from_static(b"x"),
                effect: EffectClass::Idempotent,
                payload_version: 1,
            },
            EntryKind::EffectIntent {
                idempotency_key: key,
                effect: EffectClass::ExactlyOnceGuarded,
                hmac: None,
            },
            EntryKind::PromiseCreated {
                promise_id: PromiseId::new(),
                resolver_token_hash: [0u8; 32],
                hmac: Some([1u8; 32]),
            },
            EntryKind::PromiseResolved {
                promise_id: PromiseId::new(),
                payload: Bytes::new(),
            },
            EntryKind::TimerArmed {
                timer_id: TimerId::new(),
                due_at_ms: 100,
                hmac: None,
            },
            EntryKind::TimerFired {
                timer_id: TimerId::new(),
            },
            EntryKind::Checkpoint {
                up_to_step: 3,
                snapshot: Bytes::new(),
            },
        ] {
            // Exhaustive match — no wildcard arm — over every variant.
            let tag = match &entry {
                EntryKind::StepResult { .. } => "step_result",
                EntryKind::EffectIntent { .. } => "effect_intent",
                EntryKind::PromiseCreated { .. } => "promise_created",
                EntryKind::PromiseResolved { .. } => "promise_resolved",
                EntryKind::TimerArmed { .. } => "timer_armed",
                EntryKind::TimerFired { .. } => "timer_fired",
                EntryKind::Checkpoint { .. } => "checkpoint",
            };
            assert_eq!(tag, entry.tag());
        }
    }

    #[test]
    fn journal_entry_is_clonable_and_comparable() {
        let entry = sample_entry(EntryKind::TimerFired {
            timer_id: TimerId::new(),
        });
        assert_eq!(entry, entry.clone());
    }

    #[test]
    fn execution_status_round_trips_through_str() {
        for status in [
            ExecutionStatus::Running,
            ExecutionStatus::Completed,
            ExecutionStatus::Failed,
            ExecutionStatus::Aborted,
        ] {
            assert!(!status.as_str().is_empty());
        }
        assert!(ExecutionStatus::Running.is_running());
        assert!(!ExecutionStatus::Aborted.is_running());
    }
}
