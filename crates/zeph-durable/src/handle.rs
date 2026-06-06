// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `&self` durable execution context.
//!
//! [`DurableContext`] is the front door to durable execution: a consumer wraps each unit of work in
//! [`step`](DurableContext::step) (or a [`parallel`](DurableContext::parallel) batch) and the
//! context journals it, replays it on resume, and enforces the exactly-once contract. The entire
//! surface is `&self` — step ids come from an [`AtomicU32`], so concurrent steps under a single
//! shared context are sound without a mutable borrow (system-invariants §10).
//!
//! # Deterministic step ids (INV-2)
//!
//! A step id is assigned the moment a step is *started*, in program order, via `fetch_add`. A
//! [`ParallelScope`] assigns each child's id eagerly when the child future is constructed (before
//! any of them is polled), so a parallel batch's ids are fixed by argument order and are independent
//! of completion order. The same program therefore re-derives the same ids on replay.
//!
//! # Replay and the divergence guard (INV-3)
//!
//! On resume the program re-runs and each step consults the `ReplayCursor`:
//!
//! - a committed result replays without invoking the closure (INV-10);
//! - an intent-only entry means the *ambiguous window* — the step's
//!   [`OnAmbiguous`] policy decides (and a mandatory audit record is emitted, FR-DE-10);
//! - nothing journaled means run fresh.
//!
//! Before replaying a result the context compares the journaled step's [`IdempotencyKey`] — the
//! step's structural fingerprint — against the key derived from the *current* descriptor. A mismatch
//! is a [`DurableError::ReplayDivergence`]: the journal is marked `aborted` and replay is disabled
//! so the execution restarts fresh, never returning a result for a structurally different step.
//!
//! # Exactly-once and the ambiguous window (FR-DE-04, INV-13)
//!
//! A guarded step commits its `EffectIntent` (acknowledged) *before* the closure runs and its
//! `StepResult` (acknowledged) *after*. If a replay divergence forces a fresh run, a guarded effect
//! that already committed a result is recognized by its idempotency key (a point lookup) and its
//! journaled value is returned rather than re-firing the effect.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::SystemTime;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::Instrument as _;

use crate::backend::local::now_unix_millis;
use crate::backend::{DurableBackendEnum, ExecutionBackend as _};
use crate::config::DurableConfig;
use crate::effect::{EffectClass, OnAmbiguous};
use crate::error::DurableError;
use crate::ids::{ExecutionId, ExecutionKind, IdempotencyKey, StepId};
use crate::journal::{EntryKind, ExecutionStatus, Journal as _, JournalEntry};
use crate::replay::{DEFAULT_SEGMENT_STEPS, ReplayCursor, StepReplay};
use crate::step::{
    DurableStep, PAYLOAD_VERSION, StepDescriptor, StepError, StepHandle, deserialize_result,
    serialize_result,
};
use crate::writer::JournalWriterHandle;

/// The `&self` durable execution context handed to a consumer's program.
///
/// Construct it with [`DurableContext::new`] from a shared backend (the read path) and a
/// [`JournalWriterHandle`] (the write path) — both bound to the same `durable.db`. A fresh
/// execution opens with `is_resume = false`; a resumed one with `is_resume = true`, which activates
/// the `ReplayCursor`.
#[derive(Debug)]
pub struct DurableContext {
    execution_id: ExecutionId,
    kind: ExecutionKind,
    next_step: AtomicU32,
    diverged: AtomicBool,
    is_resume: bool,
    cursor: ReplayCursor,
    backend: Arc<DurableBackendEnum>,
    writer: JournalWriterHandle,
    max_steps_per_execution: u32,
    max_payload_bytes: u64,
}

impl DurableContext {
    /// Build a context for an execution.
    ///
    /// `backend` is the shared read path (the `ReplayCursor` reads segments through it) and must
    /// be the same `durable.db` instance the `writer` commits to. `is_resume` activates replay:
    /// pass `true` when reopening an execution that already has journal entries (as
    /// [`LocalBackend::open_execution`](crate::LocalBackend::open_execution) reports), `false` for a
    /// brand-new execution.
    #[must_use]
    pub fn new(
        execution_id: ExecutionId,
        kind: ExecutionKind,
        is_resume: bool,
        backend: Arc<DurableBackendEnum>,
        writer: JournalWriterHandle,
        config: &DurableConfig,
    ) -> Self {
        let cursor = ReplayCursor::new(backend.clone(), execution_id, DEFAULT_SEGMENT_STEPS);
        Self {
            execution_id,
            kind,
            next_step: AtomicU32::new(0),
            diverged: AtomicBool::new(false),
            is_resume,
            cursor,
            backend,
            writer,
            max_steps_per_execution: config.max_steps_per_execution,
            max_payload_bytes: config.max_payload_bytes,
        }
    }

    /// The execution this context drives.
    #[must_use]
    pub fn execution_id(&self) -> ExecutionId {
        self.execution_id
    }

    /// The execution's category.
    #[must_use]
    pub fn kind(&self) -> ExecutionKind {
        self.kind
    }

    /// Run a durable step, returning just its value.
    ///
    /// On a fresh execution the operation `op` runs and its result is journaled; on replay the
    /// journaled result is returned and `op` is never invoked (INV-10). The closure receives a
    /// [`StepHandle`] carrying the step's [`IdempotencyKey`] for boundary deduplication.
    ///
    /// Use [`step_recorded`](DurableContext::step_recorded) when the live/replayed distinction or the
    /// step id matters.
    ///
    /// # Errors
    ///
    /// - [`DurableError::StepFailed`] if `op` returns an error on a fresh run.
    /// - [`DurableError::ReplayDivergence`] if the journaled step at this position has a different
    ///   structural fingerprint (INV-3).
    /// - [`DurableError::AmbiguousEffect`] for an [`OnAmbiguous::Fail`] guarded step caught in the
    ///   ambiguous window.
    /// - [`DurableError::Serialize`] / [`DurableError::Decode`] on a payload codec failure, or a
    ///   storage error from the journal.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run(ctx: &zeph_durable::DurableContext) -> Result<(), zeph_durable::DurableError> {
    /// use zeph_durable::StepDescriptor;
    ///
    /// let lines: usize = ctx
    ///     .step(StepDescriptor::idempotent("count", b"tool:count".to_vec()), |_handle| async {
    ///         Ok(42)
    ///     })
    ///     .await?;
    /// assert_eq!(lines, 42);
    /// # Ok(()) }
    /// ```
    pub async fn step<T, F, Fut>(&self, desc: StepDescriptor, op: F) -> Result<T, DurableError>
    where
        T: Serialize + DeserializeOwned + Send,
        F: FnOnce(StepHandle) -> Fut + Send,
        Fut: Future<Output = Result<T, StepError>> + Send,
    {
        let step_id = self.assign_step_id();
        self.run_step_at(step_id, desc, op)
            .await
            .map(DurableStep::into_value)
    }

    /// Run a durable step, returning the full [`DurableStep`] record (id, key, and outcome).
    ///
    /// # Errors
    ///
    /// Identical to [`step`](DurableContext::step).
    pub async fn step_recorded<T, F, Fut>(
        &self,
        desc: StepDescriptor,
        op: F,
    ) -> Result<DurableStep<T>, DurableError>
    where
        T: Serialize + DeserializeOwned + Send,
        F: FnOnce(StepHandle) -> Fut + Send,
        Fut: Future<Output = Result<T, StepError>> + Send,
    {
        let step_id = self.assign_step_id();
        self.run_step_at(step_id, desc, op).await
    }

    /// Open a [`ParallelScope`] whose children receive contiguous, eagerly-assigned step ids.
    ///
    /// Construct the children synchronously (e.g. with a `Vec` or `.map(...).collect()`), then drive
    /// them with `join_all`/`try_join_all`; their ids are fixed by construction order and are stable
    /// across replay regardless of completion order (INV-2).
    #[must_use]
    pub fn parallel(&self) -> ParallelScope<'_> {
        ParallelScope { ctx: self }
    }

    /// Durable timer entry point — **not yet wired**; durable timers land with the promise/timer
    /// layer (C5).
    ///
    /// # Errors
    ///
    /// Currently always returns [`DurableError::UnsupportedEntryKind`]: the backend does not yet
    /// persist `timer_armed` entries. The signature is stable so consumers can target it ahead of
    /// the timer layer.
    #[allow(
        clippy::unused_async,
        reason = "stub: the C5 timer layer adds the awaiting body; the async signature is the stable surface"
    )]
    pub async fn sleep_until(&self, due: SystemTime) -> Result<(), DurableError> {
        let due_ms = due
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        tracing::debug!(
            execution_id = %self.execution_id.as_uuid(),
            due_ms,
            "durable timers are not yet wired; sleep_until lands with the promise/timer layer"
        );
        Err(DurableError::UnsupportedEntryKind {
            kind: "timer_armed",
        })
    }

    /// Assign the next deterministic step id (INV-2).
    fn assign_step_id(&self) -> StepId {
        StepId::new(self.next_step.fetch_add(1, Ordering::Relaxed))
    }

    /// Whether replay is currently consulted (a resume that has not diverged).
    fn replay_active(&self) -> bool {
        self.is_resume && !self.diverged.load(Ordering::Acquire)
    }

    /// The core step state machine shared by the sequential and parallel entry points.
    async fn run_step_at<T, F, Fut>(
        &self,
        step_id: StepId,
        desc: StepDescriptor,
        op: F,
    ) -> Result<DurableStep<T>, DurableError>
    where
        T: Serialize + DeserializeOwned + Send,
        F: FnOnce(StepHandle) -> Fut + Send,
        Fut: Future<Output = Result<T, StepError>> + Send,
    {
        if step_id.value() >= self.max_steps_per_execution {
            return Err(DurableError::StepCapExceeded {
                cap: self.max_steps_per_execution,
            });
        }
        let effect = desc.effect();
        let idem_key =
            IdempotencyKey::derive(self.execution_id, step_id, &desc.fingerprint_input());

        let span = tracing::info_span!(
            "durable.step.run",
            step_id = step_id.value(),
            effect_class = effect.as_str(),
            replayed = tracing::field::Empty,
        );
        async move {
            // 1) Sequential replay: consult the cursor for this position.
            if self.replay_active() {
                match self.cursor.lookup(step_id).await? {
                    StepReplay::Result(entry) => {
                        self.check_divergence(step_id, idem_key, &entry).await?;
                        let value = replay_value::<T>(step_id, effect, &entry)?;
                        tracing::Span::current().record("replayed", true);
                        return Ok(DurableStep::replayed(step_id, idem_key, value));
                    }
                    StepReplay::IntentOnly(entry) => {
                        self.check_divergence(step_id, idem_key, &entry).await?;
                        return self.resolve_ambiguous(step_id, idem_key, &desc, op).await;
                    }
                    StepReplay::Fresh => {}
                }
            }

            // 2) INV-13: a guarded effect that already committed a result must not re-fire, even on a
            // fresh run that follows a divergence. A point lookup by idempotency key catches it.
            if effect == EffectClass::ExactlyOnceGuarded
                && let Some(entry) = self
                    .backend
                    .lookup_committed_result(self.execution_id, idem_key)
                    .await?
            {
                let value = replay_value::<T>(step_id, effect, &entry)?;
                tracing::Span::current().record("replayed", true);
                return Ok(DurableStep::replayed(step_id, idem_key, value));
            }

            // 3) Fresh execution of the step.
            tracing::Span::current().record("replayed", false);
            if effect == EffectClass::ExactlyOnceGuarded {
                // FR-DE-04: the intent is committed and ACKed before the effect fires.
                let intent = self.intent_entry(step_id, idem_key, effect);
                self.append_acked_degrading(intent, desc.name()).await?;
            }
            let value = self.run_op(op, step_id, idem_key, desc.name()).await?;
            // Serialize before the journal await so no `&T` is held across it (that would force a
            // `T: Sync` bound the consumer's value need not satisfy); the owned value moves into the
            // returned record afterward.
            let payload = serialize_result(&value, desc.name())?;
            self.journal_result(payload, step_id, idem_key, effect, desc.name())
                .await?;
            Ok(DurableStep::live(step_id, idem_key, value))
        }
        .instrument(span)
        .await
    }

    /// Compare a journaled step's fingerprint against the current descriptor's; abort on mismatch.
    async fn check_divergence(
        &self,
        step_id: StepId,
        expected: IdempotencyKey,
        entry: &JournalEntry,
    ) -> Result<(), DurableError> {
        // The idempotency key folds in the descriptor name, effect, and op fingerprint, so equality
        // is the one-BLAKE3-compare fingerprint check the divergence guard requires (INV-3).
        if entry.entry.idempotency_key() == Some(expected) {
            return Ok(());
        }
        self.on_divergence(step_id).await;
        Err(DurableError::ReplayDivergence { step_id })
    }

    /// Mark the execution aborted and disable replay so it restarts fresh (FR-DE-03).
    async fn on_divergence(&self, step_id: StepId) {
        self.diverged.store(true, Ordering::Release);
        tracing::warn!(
            execution_id = %self.execution_id.as_uuid(),
            step_id = step_id.value(),
            "replay divergence detected; marking journal aborted and restarting fresh"
        );
        if let Err(error) = self
            .backend
            .finalize(self.execution_id, ExecutionStatus::Aborted)
            .await
        {
            tracing::warn!(%error, "failed to mark diverged execution aborted");
        }
    }

    /// Apply the [`OnAmbiguous`] policy for a guarded step resumed in the ambiguous window.
    async fn resolve_ambiguous<T, F, Fut>(
        &self,
        step_id: StepId,
        idem_key: IdempotencyKey,
        desc: &StepDescriptor,
        op: F,
    ) -> Result<DurableStep<T>, DurableError>
    where
        T: Serialize + DeserializeOwned + Send,
        F: FnOnce(StepHandle) -> Fut + Send,
        Fut: Future<Output = Result<T, StepError>> + Send,
    {
        let effect = desc.effect();
        let policy = desc.on_ambiguous().unwrap_or(OnAmbiguous::Fail);
        self.emit_ambiguous_audit(step_id, effect, idem_key, policy);
        match policy {
            // Fail: refuse to guess; surface the irreversible-effect uncertainty to the operator.
            OnAmbiguous::Fail => Err(DurableError::AmbiguousEffect { step_id }),
            // Skip re-runs the closure trusting the boundary to deduplicate the re-issued effect by
            // its idempotency key; Rerun re-runs it assuming the effect never fired. At this layer
            // both re-execute (the intent already exists, so it is not re-journaled) and the audit
            // record above distinguishes which policy was applied.
            OnAmbiguous::Skip | OnAmbiguous::Rerun => {
                let value = self.run_op(op, step_id, idem_key, desc.name()).await?;
                let payload = serialize_result(&value, desc.name())?;
                self.journal_result(payload, step_id, idem_key, effect, desc.name())
                    .await?;
                Ok(DurableStep::live(step_id, idem_key, value))
            }
        }
    }

    /// Invoke the operation closure, mapping its failure to [`DurableError::StepFailed`].
    async fn run_op<T, F, Fut>(
        &self,
        op: F,
        step_id: StepId,
        idem_key: IdempotencyKey,
        name: &'static str,
    ) -> Result<T, DurableError>
    where
        F: FnOnce(StepHandle) -> Fut + Send,
        Fut: Future<Output = Result<T, StepError>> + Send,
    {
        let handle = StepHandle::new(step_id, idem_key);
        op(handle)
            .await
            .map_err(|err| DurableError::step_failed(name, err))
    }

    /// Journal a step's already-serialized result with the durability class its effect requires.
    async fn journal_result(
        &self,
        payload: bytes::Bytes,
        step_id: StepId,
        idem_key: IdempotencyKey,
        effect: EffectClass,
        name: &'static str,
    ) -> Result<(), DurableError> {
        // INV-11 write-side guard: reject an oversized payload before it reaches the writer.
        crate::cipher::ensure_payload_within_limit(payload.len(), self.max_payload_bytes)?;
        let entry = JournalEntry {
            seq: None,
            execution_id: self.execution_id,
            kind: self.kind,
            step_id,
            entry: EntryKind::StepResult {
                idempotency_key: idem_key,
                payload,
                effect,
                payload_version: PAYLOAD_VERSION,
            },
            created_at_ms: now_unix_millis(),
        };
        match effect {
            // Exactly-once results are ACKed so durability-on-return holds (FR-DE-04).
            EffectClass::ExactlyOnceGuarded => self.append_acked_degrading(entry, name).await,
            // Buffered results group-commit; a crash before the flush simply re-runs the step.
            EffectClass::Idempotent | EffectClass::AtLeastOnce => {
                self.writer.append_buffered(entry);
                Ok(())
            }
        }
    }

    /// Build an `EffectIntent` entry for a guarded step.
    fn intent_entry(
        &self,
        step_id: StepId,
        idem_key: IdempotencyKey,
        effect: EffectClass,
    ) -> JournalEntry {
        JournalEntry {
            seq: None,
            execution_id: self.execution_id,
            kind: self.kind,
            step_id,
            entry: EntryKind::EffectIntent {
                idempotency_key: idem_key,
                effect,
                // The backend is the HMAC keyholder and stamps the row HMAC itself when configured.
                hmac: None,
            },
            created_at_ms: now_unix_millis(),
        }
    }

    /// ACK an append, degrading to non-durable mode on a writer timeout (INV-12) rather than failing.
    async fn append_acked_degrading(
        &self,
        entry: JournalEntry,
        name: &'static str,
    ) -> Result<(), DurableError> {
        match self.writer.append_acked(entry).await {
            Ok(_) => Ok(()),
            Err(DurableError::JournalUnavailable) => {
                tracing::warn!(
                    step = name,
                    "journal writer unavailable; this step degrades to non-durable mode"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Emit the mandatory structured audit record for an ambiguous-window resolution (FR-DE-10).
    fn emit_ambiguous_audit(
        &self,
        step_id: StepId,
        effect: EffectClass,
        idem_key: IdempotencyKey,
        policy: OnAmbiguous,
    ) {
        tracing::warn!(
            target: "durable.audit",
            execution_id = %self.execution_id.as_uuid(),
            step_id = step_id.value(),
            effect_class = effect.as_str(),
            idem_key = %idem_key_hex8(idem_key),
            on_ambiguous = policy.as_str(),
            "durable step resumed in the ambiguous window; applying on_ambiguous policy"
        );
    }
}

/// A handle for spawning durable steps with eagerly-assigned, contiguous step ids.
///
/// Returned by [`DurableContext::parallel`]. Each [`step`](ParallelScope::step) call assigns its id
/// synchronously, *before* returning the future, so building a batch of children fixes their ids in
/// construction order — completion order is then irrelevant (INV-2).
#[derive(Debug, Clone, Copy)]
pub struct ParallelScope<'a> {
    ctx: &'a DurableContext,
}

impl<'a> ParallelScope<'a> {
    /// Construct a durable step future with its id assigned eagerly.
    ///
    /// The returned future runs (or replays) the step when awaited; its [`StepId`] is already fixed.
    /// Collect the futures synchronously, then await them concurrently.
    ///
    /// # Errors
    ///
    /// The awaited future fails for the same reasons as [`DurableContext::step`].
    pub fn step<T, F, Fut>(
        &self,
        desc: StepDescriptor,
        op: F,
    ) -> impl Future<Output = Result<DurableStep<T>, DurableError>> + Send + 'a
    where
        T: Serialize + DeserializeOwned + Send + 'a,
        F: FnOnce(StepHandle) -> Fut + Send + 'a,
        Fut: Future<Output = Result<T, StepError>> + Send + 'a,
    {
        let step_id = self.ctx.assign_step_id();
        let ctx = self.ctx;
        async move { ctx.run_step_at(step_id, desc, op).await }
    }
}

/// Extract and decode a replayed step's value from its journaled `StepResult`.
fn replay_value<T: DeserializeOwned>(
    step_id: StepId,
    effect: EffectClass,
    entry: &JournalEntry,
) -> Result<T, DurableError> {
    let _span = tracing::info_span!(
        "durable.step.replay",
        step_id = step_id.value(),
        effect_class = effect.as_str(),
    )
    .entered();
    match &entry.entry {
        EntryKind::StepResult { payload, .. } => deserialize_result(payload),
        _ => Err(DurableError::Decode {
            context: "replayed entry is not a step result",
        }),
    }
}

/// Hex-encode the first 8 bytes of an idempotency key for an audit record.
///
/// The key is a BLAKE3 hash, not secret material; the spec redaction rule shows only its first 8
/// bytes in CLI/audit output (INV-5).
fn idem_key_hex8(key: IdempotencyKey) -> String {
    let mut out = String::with_capacity(16);
    for byte in &key.as_bytes()[..8] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(all(test, feature = "sqlite", not(feature = "postgres")))]
mod tests {
    use super::*;
    use crate::backend::local::LocalBackend;
    use crate::config::DurableConfig;
    use crate::effect::EffectIntentSubClass;
    use crate::writer::JournalWriter;
    use std::pin::Pin;
    use std::sync::atomic::AtomicU32;
    use tokio::task::JoinHandle;

    /// A type-erased durable-step future, so a heterogeneous batch of step closures can share one
    /// `Vec` for `join_all` (distinct closures otherwise produce distinct opaque future types).
    type StepFut<'a> =
        Pin<Box<dyn Future<Output = Result<DurableStep<u32>, DurableError>> + Send + 'a>>;

    fn fast_config() -> DurableConfig {
        DurableConfig {
            journal_flush_interval_ms: 5,
            journal_ack_timeout_ms: 2000,
            ..DurableConfig::default()
        }
    }

    /// A running context over a fresh in-memory backend, with the writer task spawned.
    struct Harness {
        ctx: DurableContext,
        backend: Arc<LocalBackend>,
        writer_task: JoinHandle<()>,
        handle: JournalWriterHandle,
    }

    impl Harness {
        async fn open(exec: ExecutionId, is_resume: bool) -> Self {
            let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
            local.init().await.unwrap();
            local
                .open_execution(exec, ExecutionKind::AgentTurn)
                .await
                .unwrap();
            let (writer, handle) = JournalWriter::new(local.clone(), &fast_config());
            let writer_task = tokio::spawn(writer.run());
            let backend = Arc::new(DurableBackendEnum::Local(local.clone()));
            let ctx = DurableContext::new(
                exec,
                ExecutionKind::AgentTurn,
                is_resume,
                backend,
                handle.clone(),
                &fast_config(),
            );
            Self {
                ctx,
                backend: local,
                writer_task,
                handle,
            }
        }

        /// Reopen over the *same* backing journal to drive a resume run.
        fn resume(&self) -> DurableContext {
            let backend = Arc::new(DurableBackendEnum::Local(self.backend.clone()));
            DurableContext::new(
                self.ctx.execution_id,
                ExecutionKind::AgentTurn,
                true,
                backend,
                self.handle.clone(),
                &fast_config(),
            )
        }

        async fn shutdown(self) {
            // Some tests build a second context (a resume / fresh run) that clones the writer
            // handle; that clone keeps the channel open, so the writer never stops on its own.
            // Abort it directly rather than awaiting a graceful drain (data was already flushed
            // before any assertion that needed it).
            self.writer_task.abort();
            let _ = self.writer_task.await;
        }
    }

    #[tokio::test]
    async fn fresh_step_runs_op_and_journals_result() {
        // FR-DE-01: a fresh step records its result in the journal.
        let exec = ExecutionId::new();
        let h = Harness::open(exec, false).await;
        let value: u32 = h
            .ctx
            .step(
                StepDescriptor::idempotent("count", b"op".to_vec()),
                |_| async { Ok(7) },
            )
            .await
            .unwrap();
        assert_eq!(value, 7);
        h.handle.flush().await.unwrap();

        let entries = h.backend.read_execution(exec).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].entry, EntryKind::StepResult { .. }));
        h.shutdown().await;
    }

    #[tokio::test]
    async fn replayed_idempotent_step_skips_op() {
        // INV-10 / FR-DE-02: a replayed idempotent step returns the journaled value without re-running.
        let exec = ExecutionId::new();
        let h = Harness::open(exec, false).await;
        let desc = || StepDescriptor::idempotent("count", b"op".to_vec());
        let first: u32 = h.ctx.step(desc(), |_| async { Ok(11) }).await.unwrap();
        assert_eq!(first, 11);
        h.handle.flush().await.unwrap();

        let resumed = h.resume();
        let ran_again = Arc::new(AtomicU32::new(0));
        let counter = ran_again.clone();
        let replayed: u32 = resumed
            .step(desc(), move |_| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(999)
                }
            })
            .await
            .unwrap();
        assert_eq!(
            replayed, 11,
            "the journaled value is returned, not the new one"
        );
        assert_eq!(
            ran_again.load(Ordering::SeqCst),
            0,
            "the operation closure must not run on replay"
        );
        h.shutdown().await;
    }

    #[tokio::test]
    async fn guarded_step_commits_intent_before_result() {
        // FR-DE-04: an EffectIntent is committed before op, a StepResult after.
        let exec = ExecutionId::new();
        let h = Harness::open(exec, false).await;
        let desc = StepDescriptor::exactly_once_guarded(
            "charge",
            EffectIntentSubClass::CostBearingOrBoundaryIdempotent,
            Some(OnAmbiguous::Skip),
            b"op".to_vec(),
        )
        .unwrap();
        let _: u32 = h.ctx.step(desc, |_| async { Ok(5) }).await.unwrap();
        h.handle.flush().await.unwrap();

        let entries = h.backend.read_execution(exec).await.unwrap();
        let kinds: Vec<_> = entries.iter().map(|e| e.entry.tag()).collect();
        assert_eq!(
            kinds,
            vec!["effect_intent", "step_result"],
            "intent is journaled before the result"
        );
        h.shutdown().await;
    }

    #[tokio::test]
    async fn replay_divergence_on_fingerprint_mismatch() {
        // INV-3 / FR-DE-03: a structurally different step at the same id aborts and restarts fresh.
        let exec = ExecutionId::new();
        let h = Harness::open(exec, false).await;
        let _: u32 = h
            .ctx
            .step(
                StepDescriptor::idempotent("count", b"v1".to_vec()),
                |_| async { Ok(1) },
            )
            .await
            .unwrap();
        h.handle.flush().await.unwrap();

        let resumed = h.resume();
        // Same step position, different op fingerprint → different structural fingerprint.
        let err = resumed
            .step::<u32, _, _>(
                StepDescriptor::idempotent("count", b"v2".to_vec()),
                |_| async { Ok(2) },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DurableError::ReplayDivergence { .. }));

        let (status,): (String,) = zeph_db::query_as(zeph_db::sql!(
            "SELECT status FROM durable_executions WHERE execution_id = ?"
        ))
        .bind(exec.as_uuid().to_string())
        .fetch_one(h.backend.pool())
        .await
        .unwrap();
        assert_eq!(status, "aborted", "the diverged journal is marked aborted");
        h.shutdown().await;
    }

    #[tokio::test]
    async fn ambiguous_window_fail_policy_surfaces_error() {
        // FR-DE-14 path: an intent without a result, policy = Fail, must not re-fire.
        let exec = ExecutionId::new();
        let h = Harness::open(exec, false).await;
        let step_id = StepId::new(0);
        let idem = IdempotencyKey::derive(
            exec,
            step_id,
            &StepDescriptor::exactly_once_guarded(
                "delete",
                EffectIntentSubClass::Destructive,
                Some(OnAmbiguous::Fail),
                b"op".to_vec(),
            )
            .unwrap()
            .fingerprint_input(),
        );
        // Seed only the intent (the crash happened before the result committed).
        h.backend
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

        let resumed = h.resume();
        let ran = Arc::new(AtomicU32::new(0));
        let counter = ran.clone();
        let err = resumed
            .step::<u32, _, _>(
                StepDescriptor::exactly_once_guarded(
                    "delete",
                    EffectIntentSubClass::Destructive,
                    Some(OnAmbiguous::Fail),
                    b"op".to_vec(),
                )
                .unwrap(),
                move |_| {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(1)
                    }
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DurableError::AmbiguousEffect { .. }));
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "a fail-policy ambiguous step must not re-fire the effect"
        );
        h.shutdown().await;
    }

    #[tokio::test]
    async fn inv13_committed_guarded_result_is_not_refired() {
        // INV-13: on a fresh run after divergence, an already-committed guarded result is returned
        // via its idempotency key without re-firing.
        let exec = ExecutionId::new();
        let h = Harness::open(exec, false).await;
        let desc = || {
            StepDescriptor::exactly_once_guarded(
                "transfer",
                EffectIntentSubClass::MoneyMoving,
                Some(OnAmbiguous::Fail),
                b"op".to_vec(),
            )
            .unwrap()
        };
        let first: u32 = h.ctx.step(desc(), |_| async { Ok(500) }).await.unwrap();
        assert_eq!(first, 500);
        h.handle.flush().await.unwrap();

        // A "fresh run after divergence": replay is OFF, but the guarded point lookup must still find
        // the committed result. Build a non-resume context over the same journal.
        let backend = Arc::new(DurableBackendEnum::Local(h.backend.clone()));
        let fresh = DurableContext::new(
            exec,
            ExecutionKind::AgentTurn,
            false,
            backend,
            h.handle.clone(),
            &fast_config(),
        );
        let ran = Arc::new(AtomicU32::new(0));
        let counter = ran.clone();
        let value: u32 = fresh
            .step(desc(), move |_| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(0)
                }
            })
            .await
            .unwrap();
        assert_eq!(value, 500, "the pre-committed guarded result is returned");
        assert_eq!(
            ran.load(Ordering::SeqCst),
            0,
            "the guarded effect must not re-fire"
        );
        h.shutdown().await;
    }

    #[tokio::test]
    async fn parallel_step_ids_are_completion_order_independent() {
        // INV-2: a parallel batch with shuffled completion order yields deterministic step ids.
        let exec = ExecutionId::new();
        let h = Harness::open(exec, false).await;
        let scope = h.ctx.parallel();
        // Construct children in argument order; each gets its id eagerly at the `scope.step` call
        // (before any future is polled). Box them so the differently-typed closures share one Vec.
        let futures: Vec<StepFut> = vec![
            Box::pin(scope.step::<u32, _, _>(
                StepDescriptor::idempotent("a", b"a".to_vec()),
                |handle: StepHandle| async move {
                    // Finishes last despite being constructed first.
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    Ok(handle.step_id().value())
                },
            )),
            Box::pin(scope.step::<u32, _, _>(
                StepDescriptor::idempotent("b", b"b".to_vec()),
                |handle: StepHandle| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    Ok(handle.step_id().value())
                },
            )),
            Box::pin(scope.step::<u32, _, _>(
                StepDescriptor::idempotent("c", b"c".to_vec()),
                |handle: StepHandle| async move { Ok(handle.step_id().value()) },
            )),
        ];
        let results = futures::future::try_join_all(futures).await.unwrap();
        let ids: Vec<u32> = results
            .iter()
            .map(DurableStep::step_id)
            .map(StepId::value)
            .collect();
        // Each child observed the id assigned at construction, regardless of completion order.
        assert_eq!(ids, vec![0, 1, 2]);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_steps_under_shared_ref_are_sound() {
        // System-invariants §10: concurrent step() calls under a single &self assign unique ids and
        // all journal successfully.
        let exec = ExecutionId::new();
        let h = Harness::open(exec, false).await;
        let scope = h.ctx.parallel();
        let futures: Vec<StepFut> = (0..16)
            .map(|i| {
                Box::pin(scope.step::<u32, _, _>(
                    StepDescriptor::idempotent("worker", format!("op:{i}").into_bytes()),
                    move |handle: StepHandle| async move { Ok(handle.step_id().value()) },
                )) as StepFut
            })
            .collect();
        let results = futures::future::try_join_all(futures).await.unwrap();
        let mut ids: Vec<u32> = results
            .iter()
            .map(DurableStep::step_id)
            .map(StepId::value)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 16, "all 16 concurrent steps got unique ids");
        h.handle.flush().await.unwrap();
        assert_eq!(h.backend.read_execution(exec).await.unwrap().len(), 16);
        h.shutdown().await;
    }

    #[tokio::test]
    async fn op_failure_surfaces_as_step_failed_without_journaling() {
        let exec = ExecutionId::new();
        let h = Harness::open(exec, false).await;
        let err = h
            .ctx
            .step::<u32, _, _>(
                StepDescriptor::idempotent("boom", b"op".to_vec()),
                |_| async { Err(StepError::new("op exploded")) },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DurableError::StepFailed { step: "boom", .. }));
        h.handle.flush().await.unwrap();
        assert!(
            h.backend.read_execution(exec).await.unwrap().is_empty(),
            "a failed step journals no result"
        );
        h.shutdown().await;
    }

    #[tokio::test]
    async fn step_cap_is_enforced() {
        let exec = ExecutionId::new();
        let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
        local.init().await.unwrap();
        local
            .open_execution(exec, ExecutionKind::AgentTurn)
            .await
            .unwrap();
        let (writer, handle) = JournalWriter::new(local.clone(), &fast_config());
        let task = tokio::spawn(writer.run());
        let backend = Arc::new(DurableBackendEnum::Local(local.clone()));
        let ctx = DurableContext::new(
            exec,
            ExecutionKind::AgentTurn,
            false,
            backend,
            handle.clone(),
            &DurableConfig {
                max_steps_per_execution: 1,
                ..fast_config()
            },
        );
        // Step id 0 is allowed; step id 1 exceeds the cap of 1.
        ctx.step::<u32, _, _>(
            StepDescriptor::idempotent("ok", b"op".to_vec()),
            |_| async { Ok(0) },
        )
        .await
        .unwrap();
        let err = ctx
            .step::<u32, _, _>(
                StepDescriptor::idempotent("over", b"op".to_vec()),
                |_| async { Ok(0) },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DurableError::StepCapExceeded { cap: 1 }));
        drop(ctx);
        drop(handle);
        task.await.unwrap();
    }
}
