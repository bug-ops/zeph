// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The sealed execution-backend abstraction and its enum-dispatch front door.
//!
//! A backend is the persistence engine behind a durable execution: it journals control flow and,
//! for the local backend, owns the dedicated `durable.db` pool. The [`ExecutionBackend`] trait is
//! **sealed** (it requires [`crate::sealed::Sealed`]), so only backends declared inside this crate
//! can implement it. External crates never name a concrete backend; they hold a
//! [`DurableBackendEnum`] and dispatch through it.
//!
//! # Why enum dispatch instead of `Box<dyn ExecutionBackend>`
//!
//! The journal append path is hot. A trait object would force a virtual call and a heap allocation
//! per dispatch; [`DurableBackendEnum`] resolves the backend with a single `match` and no
//! allocation (the spec's NEVER list forbids `Box<dyn ExecutionBackend>` on the dispatch path).
//! Because the trait is sealed, adding methods to it later — when the `DurableContext`, promise,
//! and timer entry points land — is a non-breaking change.
//!
//! # Scope
//!
//! This module defines [`BackendCapabilities`], the sealed [`ExecutionBackend`] trait (with its
//! `capabilities` accessor), and the [`DurableBackendEnum`] dispatcher. The execution-open,
//! promise-resolution, and timer-scan methods named in the spec land alongside the
//! `DurableContext` (the trait can gain them without breaking callers).

use std::sync::Arc;

use bytes::Bytes;

use crate::config::RetentionPolicy;
use crate::error::DurableError;
use crate::ids::{ExecutionId, IdempotencyKey, JournalSeq, PromiseId, TimerId};
use crate::journal::{ExecutionStatus, Journal, JournalEntry};
use crate::promise::PromiseRecord;
use crate::waiters::NotifyRegistry;

pub mod local;

pub use local::LocalBackend;

/// A read-only summary of a single durable execution, for operability surfaces.
///
/// Returned by [`LocalBackend::list_executions`]. It carries only the execution-level metadata that
/// the `zeph durable list` CLI and the TUI `DurableView` display — never payload bytes or resolver
/// tokens (INV-5 redaction). The `kind` is the raw column tag (an [`ExecutionKind::Custom`] cannot
/// round-trip to a typed value, so the stored string is exposed verbatim for display).
///
/// [`ExecutionKind::Custom`]: crate::ExecutionKind::Custom
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionSummary {
    /// The execution identity.
    pub execution_id: ExecutionId,
    /// The canonical kind tag as stored (`agent_turn`, `dag_run`, …, or a custom literal).
    pub kind: String,
    /// The current execution status.
    pub status: ExecutionStatus,
    /// Creation time, Unix epoch milliseconds.
    pub created_at_ms: i64,
    /// Last-update time, Unix epoch milliseconds.
    pub updated_at_ms: i64,
    /// Finalization time, Unix epoch milliseconds; `None` while the execution is non-terminal.
    pub finalized_at_ms: Option<i64>,
    /// Number of journal entries recorded for this execution.
    pub step_count: u64,
}

/// A redaction-safe view of one journal entry, for the `zeph durable show`/`inspect` CLI.
///
/// Returned by [`LocalBackend::read_execution_redacted`]. It deliberately excludes the payload bytes
/// and full idempotency key — only the metadata the spec's INV-5 redaction rule permits in default
/// output. To see decrypted payloads a caller must opt in via `--reveal`, which reads through the
/// AEAD cipher with [`Journal::read_execution`](crate::Journal::read_execution) instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedEntry {
    /// Global append sequence.
    pub seq: i64,
    /// The step this entry belongs to.
    pub step_id: crate::ids::StepId,
    /// The raw `entry_kind` column tag (`step_result`, `effect_intent`, …).
    pub entry_kind: String,
    /// The effect-class tag, when the entry carries one.
    pub effect_class: Option<String>,
    /// Hex of the first 8 bytes of the idempotency key, when present (INV-5 prefix only).
    pub idem_key_prefix: Option<String>,
    /// Size in bytes of the stored (AEAD-sealed) payload; `0` for control entries.
    pub payload_len: u64,
    /// Creation time, Unix epoch milliseconds.
    pub created_at_ms: i64,
}

/// The capabilities a backend advertises so callers can adapt their journaling strategy.
///
/// The replay cursor and the durable-step primitive read these flags to decide, for example,
/// whether parallel steps may journal concurrently or must be serialized into reserved-id order.
///
/// # Examples
///
/// ```
/// use zeph_durable::BackendCapabilities;
///
/// // The local backend journals parallel steps concurrently and stays in-process.
/// let caps = BackendCapabilities {
///     parallel_steps: true,
///     cross_process: false,
///     max_payload: 1_048_576,
/// };
/// assert!(caps.parallel_steps);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilities {
    /// Whether the backend may record parallel steps concurrently. `false` (e.g. Restate) requires
    /// the durable wrapper to serialize *recording* into reserved-`StepId` order.
    pub parallel_steps: bool,
    /// Whether the journal lives on a database shared across processes. Drives the INV-8 encryption
    /// gate and row-level HMAC requirement.
    pub cross_process: bool,
    /// The maximum payload size, in bytes, the backend accepts on append.
    pub max_payload: usize,
}

/// A durable-execution persistence backend.
///
/// `ExecutionBackend` is the closed set of journal engines Zeph ships. It is sealed via
/// [`crate::sealed::Sealed`]: external crates cannot implement it and must dispatch through
/// [`DurableBackendEnum`]. Every backend is also a [`Journal`], so the append/read/finalize/prune
/// surface is available uniformly.
///
/// # Contract for implementors
///
/// - [`capabilities`](ExecutionBackend::capabilities) MUST return a stable description of the
///   backend; callers cache it and adapt their journaling strategy to it.
/// - The [`Journal`] half MUST serialize writes through a single connection so appends receive a
///   monotonic [`JournalSeq`].
///
/// Additional entry points (execution open, promise resolution, timer scan) are added as the
/// higher layers land; because the trait is sealed, those additions do not break callers.
pub trait ExecutionBackend: Journal + Send + Sync + crate::sealed::Sealed {
    /// Return this backend's stable capability description.
    fn capabilities(&self) -> BackendCapabilities;

    /// Look up a committed `StepResult` anywhere in an execution by its [`IdempotencyKey`].
    ///
    /// This is the point-lookup behind INV-13: after a [`DurableError::ReplayDivergence`] the
    /// execution restarts fresh, but a guarded effect that already committed its result must not
    /// re-fire. Before invoking a guarded operation the durable step consults this lookup; a `Some`
    /// result means the effect already succeeded and its journaled value is returned instead. The
    /// key uniquely locates the row via the `idx_durable_journal_idem_key` index, so the lookup is
    /// `O(log n)`.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::Decode`] if the located row cannot be reconstructed, or
    /// [`DurableError::Storage`] if the query fails.
    fn lookup_committed_result(
        &self,
        id: ExecutionId,
        idem_key: IdempotencyKey,
    ) -> impl std::future::Future<Output = Result<Option<JournalEntry>, DurableError>> + Send;
}

/// Closed enum dispatch over the compiled-in backends.
///
/// Construct it from a concrete backend and hand it across the crate boundary behind an `Arc`;
/// callers invoke the [`Journal`] and [`ExecutionBackend`] methods on the enum and the dispatch
/// resolves to the active variant with a single `match`. The enum is `#[non_exhaustive]`: the
/// feature-gated `Restate` variant joins it with the `restate` feature without breaking in-crate
/// matches.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zeph_durable::{BackendCapabilities, DurableBackendEnum};
///
/// fn max_payload(backend: &DurableBackendEnum) -> usize {
///     use zeph_durable::ExecutionBackend as _;
///     backend.capabilities().max_payload
/// }
/// # let _ = max_payload;
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum DurableBackendEnum {
    /// The always-compiled local backend journaling to a dedicated `durable.db`.
    ///
    /// Held behind an [`Arc`] so the same backing instance can be shared with the
    /// [`JournalWriter`](crate::JournalWriter) (which owns the write path) while this enum serves
    /// the read path consumed by the [`ReplayCursor`](crate::DurableContext) — both observe one
    /// `durable.db` pool.
    Local(Arc<LocalBackend>),
}

impl crate::sealed::Sealed for DurableBackendEnum {}

impl Journal for DurableBackendEnum {
    async fn append(&self, entry: JournalEntry) -> Result<JournalSeq, DurableError> {
        match self {
            Self::Local(backend) => backend.append(entry).await,
        }
    }

    async fn read_execution(&self, id: ExecutionId) -> Result<Vec<JournalEntry>, DurableError> {
        match self {
            Self::Local(backend) => backend.read_execution(id).await,
        }
    }

    async fn read_execution_range(
        &self,
        id: ExecutionId,
        from_step_id: u32,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, DurableError> {
        match self {
            Self::Local(backend) => backend.read_execution_range(id, from_step_id, limit).await,
        }
    }

    async fn finalize(&self, id: ExecutionId, status: ExecutionStatus) -> Result<(), DurableError> {
        match self {
            Self::Local(backend) => backend.finalize(id, status).await,
        }
    }

    async fn prune(&self, policy: &RetentionPolicy) -> Result<u64, DurableError> {
        match self {
            Self::Local(backend) => backend.prune(policy).await,
        }
    }
}

impl ExecutionBackend for DurableBackendEnum {
    fn capabilities(&self) -> BackendCapabilities {
        match self {
            Self::Local(backend) => backend.capabilities(),
        }
    }

    async fn lookup_committed_result(
        &self,
        id: ExecutionId,
        idem_key: IdempotencyKey,
    ) -> Result<Option<JournalEntry>, DurableError> {
        match self {
            Self::Local(backend) => backend.lookup_committed_result(id, idem_key).await,
        }
    }
}

/// Promise, timer, and retention dispatch.
///
/// These methods back the [`DurablePromise`](crate::DurablePromise) /
/// [`DurableTimerService`](crate::DurableTimerService) / [`DurableRetentionService`] surfaces. They
/// are inherent on the enum rather than on the sealed [`ExecutionBackend`] trait because they are
/// implemented through the local backend's dedicated `durable_promises` / `durable_timers` tables; a
/// future cross-process backend (Restate) would satisfy the same surface through its own SDK
/// primitives, so the closed `match` here gains a new arm at that point — a compile-time prompt
/// rather than a silent gap.
impl DurableBackendEnum {
    /// Insert a freshly-created promise row. See [`LocalBackend::insert_promise`].
    pub(crate) async fn insert_promise(
        &self,
        id: PromiseId,
        execution_id: ExecutionId,
        resolver_token_hash: [u8; 32],
        created_at_ms: i64,
    ) -> Result<(), DurableError> {
        match self {
            Self::Local(backend) => {
                backend
                    .insert_promise(id, execution_id, resolver_token_hash, created_at_ms)
                    .await
            }
        }
    }

    /// Read a promise's persisted state. See [`LocalBackend::promise_state`].
    pub(crate) async fn promise_state(
        &self,
        id: PromiseId,
    ) -> Result<Option<PromiseRecord>, DurableError> {
        match self {
            Self::Local(backend) => backend.promise_state(id).await,
        }
    }

    /// Commit a resolved value to a pending promise. See [`LocalBackend::resolve_promise`].
    pub(crate) async fn resolve_promise(
        &self,
        id: PromiseId,
        execution_id: ExecutionId,
        value_plaintext: &[u8],
        resolved_at_ms: i64,
    ) -> Result<bool, DurableError> {
        match self {
            Self::Local(backend) => {
                backend
                    .resolve_promise(id, execution_id, value_plaintext, resolved_at_ms)
                    .await
            }
        }
    }

    /// Open a promise's sealed resolved payload. See [`LocalBackend::open_promise_payload`].
    pub(crate) fn open_promise_payload(
        &self,
        id: PromiseId,
        execution_id: ExecutionId,
        sealed: &[u8],
    ) -> Result<Bytes, DurableError> {
        match self {
            Self::Local(backend) => backend.open_promise_payload(id, execution_id, sealed),
        }
    }

    /// The in-process promise wakeup registry. See [`LocalBackend::promise_waiters`].
    pub(crate) fn promise_waiters(&self) -> &NotifyRegistry {
        match self {
            Self::Local(backend) => backend.promise_waiters(),
        }
    }

    /// Arm a durable timer. See [`LocalBackend::arm_timer`].
    pub(crate) async fn arm_timer(
        &self,
        id: TimerId,
        execution_id: ExecutionId,
        due_at_ms: i64,
        created_at_ms: i64,
    ) -> Result<(), DurableError> {
        match self {
            Self::Local(backend) => {
                backend
                    .arm_timer(id, execution_id, due_at_ms, created_at_ms)
                    .await
            }
        }
    }

    /// Read a timer's `(due_at_ms, fired)` state. See [`LocalBackend::timer_state`].
    pub(crate) async fn timer_state(
        &self,
        id: TimerId,
    ) -> Result<Option<(i64, bool)>, DurableError> {
        match self {
            Self::Local(backend) => backend.timer_state(id).await,
        }
    }

    /// List unfired timers due at or before `now_ms`. See [`LocalBackend::due_timers`].
    pub(crate) async fn due_timers(&self, now_ms: i64) -> Result<Vec<TimerId>, DurableError> {
        match self {
            Self::Local(backend) => backend.due_timers(now_ms).await,
        }
    }

    /// Mark a timer fired and wake its waiter. See [`LocalBackend::mark_timer_fired`].
    pub(crate) async fn mark_timer_fired(&self, id: TimerId) -> Result<bool, DurableError> {
        match self {
            Self::Local(backend) => backend.mark_timer_fired(id).await,
        }
    }

    /// The in-process timer wakeup registry. See [`LocalBackend::timer_waiters`].
    pub(crate) fn timer_waiters(&self) -> &NotifyRegistry {
        match self {
            Self::Local(backend) => backend.timer_waiters(),
        }
    }

    /// Fold an execution's idempotent prefix into a checkpoint. See [`LocalBackend::checkpoint_fold`].
    pub(crate) async fn checkpoint_fold(
        &self,
        execution_id: ExecutionId,
        up_to_step: u32,
    ) -> Result<u64, DurableError> {
        match self {
            Self::Local(backend) => backend.checkpoint_fold(execution_id, up_to_step).await,
        }
    }

    /// Reconstruct folded step results from every checkpoint. See [`LocalBackend::read_checkpoints`].
    pub(crate) async fn read_checkpoints(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Vec<JournalEntry>, DurableError> {
        match self {
            Self::Local(backend) => backend.read_checkpoints(execution_id).await,
        }
    }
}
