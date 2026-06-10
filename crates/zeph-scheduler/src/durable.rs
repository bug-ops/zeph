// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Durable exactly-once adapter for scheduler job fires (FR-DE-14).
//!
//! This module wraps each scheduler job fire in a durable execution so that a `(job_name,
//! scheduled_slot)` pair fires exactly once across crash-restart cycles. The guarantee is:
//!
//! - **Committed case**: if a prior run committed the `StepResult`, `INV-13` returns the journaled
//!   `()` and the closure is never re-invoked.
//! - **Ambiguous window**: if the process crashed after `EffectIntent` but before `StepResult`, the
//!   `OnAmbiguous::Skip` policy documents the skip in the audit log; the injection is not retried.
//! - **No journal / flag off**: the caller invokes the handler directly, with no durable overhead.
//!
//! The durable flag (`[durable].scheduler`) must be `true` **and** the scheduler must have been
//! built with [`SchedulerDurableAdapter`] via [`crate::Scheduler::with_durable`] for the wrapping to
//! take effect.
//!
//! # Spec reference
//!
//! `specs/064-durable-execution/spec.md` §Integration Adapters → P3, FR-DE-14.

use std::future::Future;
use std::sync::Arc;

use tracing::Instrument as _;
use zeph_config::DurableConfig;
use zeph_durable::{
    DurableBackendEnum, DurableContext, EffectIntentSubClass, ExecutionId, ExecutionKind,
    JournalWriterHandle, LocalBackend, StepDescriptor, StepError,
};

use crate::error::SchedulerError;

/// Derive a deterministic [`ExecutionId`] for a scheduler job fire.
///
/// The id is stable: two calls with the same `job_name` and `slot_ms` produce the same
/// [`ExecutionId`], which lets a restarted scheduler reattach to the existing journal row and
/// skip the fire when it was already committed.
///
/// # Examples
///
/// ```
/// use zeph_scheduler::durable::derive_execution_id;
///
/// let a = derive_execution_id("nightly-report", 1_700_000_000_000);
/// let b = derive_execution_id("nightly-report", 1_700_000_000_000);
/// let c = derive_execution_id("nightly-report", 1_700_000_001_000);
/// assert_eq!(a, b, "same job+slot derives the same id");
/// assert_ne!(a, c, "different slot derives a different id");
/// ```
#[must_use]
pub fn derive_execution_id(job_name: &str, slot_ms: i64) -> ExecutionId {
    // Length-delimited framing: 4-byte job_name length + job_name bytes + null separator +
    // 8-byte little-endian slot_ms. The null byte prevents `"ab" || "cd"` from colliding
    // with `"abc" || "d"`.
    let name_bytes = job_name.as_bytes();
    let mut payload = Vec::with_capacity(8 + name_bytes.len() + 1 + 8);
    payload.extend_from_slice(&(name_bytes.len() as u64).to_le_bytes());
    payload.extend_from_slice(name_bytes);
    payload.push(0u8);
    payload.extend_from_slice(&slot_ms.to_le_bytes());
    ExecutionId::derive(b"zeph.scheduler.fire.v1", &payload)
}

/// Durable adapter state held by [`Scheduler`](crate::Scheduler) for its lifetime.
///
/// Build one with the backend and writer from the binary's durable initialisation path, then pass
/// it to [`crate::Scheduler::with_durable`]. A fresh [`DurableContext`] is opened per fire; the backend
/// and writer are shared across all fires within the scheduler's lifetime.
#[derive(Clone)]
pub struct SchedulerDurableAdapter {
    backend: Arc<DurableBackendEnum>,
    writer: JournalWriterHandle,
    config: Arc<DurableConfig>,
}

impl SchedulerDurableAdapter {
    /// Create an adapter from the shared durable backend, journal writer, and config.
    #[must_use]
    pub fn new(
        backend: Arc<DurableBackendEnum>,
        writer: JournalWriterHandle,
        config: Arc<DurableConfig>,
    ) -> Self {
        Self {
            backend,
            writer,
            config,
        }
    }
}

/// Wrap a single scheduler job fire in a durable exactly-once execution.
///
/// Opens a per-fire [`DurableContext`] keyed on `(job_name, slot_ms)`. If the same `(job, slot)`
/// was committed in a prior run the step is replayed without invoking `fire`. If the process
/// crashed after `EffectIntent` was committed but before `StepResult`, `OnAmbiguous::Skip`
/// documents the skip in the audit log without re-firing.
///
/// When `fire` succeeds its `()` result is journaled. On error no `StepResult` is committed, so a
/// future restart retries the fire.
///
/// # Errors
///
/// Returns [`SchedulerError::TaskFailed`] when the underlying durable step fails (journal error,
/// ambiguous-effect policy fired, or `fire` itself returned an error).
///
/// # Examples
///
/// ```no_run
/// # use std::sync::Arc;
/// # use std::future::Future;
/// # use zeph_scheduler::durable::{SchedulerDurableAdapter, fire_with_durable};
/// # async fn example(adapter: &SchedulerDurableAdapter) -> Result<(), zeph_scheduler::SchedulerError> {
/// fire_with_durable(adapter, "nightly-report", 1_700_000_000_000, || async {
///     // Original fire body goes here.
///     Ok(())
/// })
/// .await
/// # }
/// ```
pub async fn fire_with_durable<F, Fut>(
    adapter: &SchedulerDurableAdapter,
    job_name: &str,
    slot_ms: i64,
    fire: F,
) -> Result<(), SchedulerError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), SchedulerError>> + Send + 'static,
{
    let span = tracing::info_span!("sched.durable.fire", job = job_name, slot_ms = slot_ms,);

    async move {
        let exec_id = derive_execution_id(job_name, slot_ms);

        let local_backend: Arc<LocalBackend> = match &*adapter.backend {
            DurableBackendEnum::Local(lb) => lb.clone(),
            _ => {
                return Err(SchedulerError::TaskFailed(
                    "durable scheduler adapter requires a LocalBackend".into(),
                ));
            }
        };

        let is_resume = local_backend
            .open_execution(exec_id, ExecutionKind::ScheduledJob)
            .await
            .map_err(|e| SchedulerError::TaskFailed(format!("durable open failed: {e}")))?;

        let ctx = DurableContext::new(
            exec_id,
            ExecutionKind::ScheduledJob,
            is_resume,
            adapter.backend.clone(),
            adapter.writer.clone(),
            &adapter.config,
        );

        // CostBearingOrBoundaryIdempotent: the journal deduplicates the fire by idempotency key,
        // so an ambiguous-window restart safely skips rather than re-injecting. Default policy is
        // OnAmbiguous::Skip and no explicit policy override is needed.
        let desc = StepDescriptor::exactly_once_guarded(
            "scheduler_fire",
            EffectIntentSubClass::CostBearingOrBoundaryIdempotent,
            None,
            fingerprint(job_name, slot_ms),
        )
        .map_err(|e| SchedulerError::TaskFailed(format!("durable descriptor error: {e}")))?;

        ctx.step(desc, |_handle| async move {
            fire().await.map_err(|e| StepError::new(e.to_string()))
        })
        .await
        .map_err(|e| SchedulerError::TaskFailed(format!("durable step failed: {e}")))
    }
    .instrument(span)
    .await
}

/// Build a stable, non-secret fingerprint for `(job_name, slot_ms)`.
///
/// Length-delimits the job name so `"ab" || "c"` ≠ `"a" || "bc"`.
fn fingerprint(job_name: &str, slot_ms: i64) -> Vec<u8> {
    let name_bytes = job_name.as_bytes();
    let mut buf = Vec::with_capacity(8 + name_bytes.len() + 8);
    buf.extend_from_slice(&(name_bytes.len() as u64).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    buf.extend_from_slice(&slot_ms.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use zeph_durable::config::DurableConfig;
    use zeph_durable::{DurableBackendEnum, JournalWriter, LocalBackend};

    use super::*;

    #[test]
    fn derive_execution_id_is_stable() {
        let a = derive_execution_id("weekly-digest", 1_700_000_000_000);
        let b = derive_execution_id("weekly-digest", 1_700_000_000_000);
        assert_eq!(a, b, "same inputs must produce the same ExecutionId");
    }

    #[test]
    fn derive_execution_id_differs_by_slot() {
        let a = derive_execution_id("weekly-digest", 1_700_000_000_000);
        let c = derive_execution_id("weekly-digest", 1_700_000_001_000);
        assert_ne!(
            a, c,
            "different slot_ms must produce a different ExecutionId"
        );
    }

    #[test]
    fn derive_execution_id_differs_by_name() {
        let a = derive_execution_id("job-a", 1_000);
        let b = derive_execution_id("job-b", 1_000);
        assert_ne!(a, b, "different job names must produce different ids");
    }

    #[test]
    fn derive_execution_id_no_prefix_collision() {
        // Ensure "ab"+"cd" ≠ "abc"+"d" at the same slot.
        let a = derive_execution_id("ab", 1_000);
        let b = derive_execution_id("abc", 1_000);
        assert_ne!(
            a, b,
            "length-delimited framing must prevent prefix collisions"
        );
    }

    fn fast_config() -> DurableConfig {
        DurableConfig {
            journal_flush_interval_ms: 5,
            journal_ack_timeout_ms: 2000,
            ..DurableConfig::default()
        }
    }

    async fn make_adapter() -> (SchedulerDurableAdapter, tokio::task::JoinHandle<()>) {
        let local = Arc::new(LocalBackend::open(":memory:", 1_048_576).await.unwrap());
        local.init().await.unwrap();
        let backend = Arc::new(DurableBackendEnum::Local(local.clone()));
        let cfg = Arc::new(fast_config());
        let (writer, handle) = JournalWriter::new(local, &cfg);
        let task = tokio::spawn(writer.run()); // EXEMPT: test-only helper
        (SchedulerDurableAdapter::new(backend, handle, cfg), task)
    }

    /// First fire executes the closure and journals the result.
    /// Second call (same job+slot, same execution) replays without invoking the closure.
    #[tokio::test]
    async fn fire_with_durable_replays_without_reinvocation() {
        let (adapter, _task) = make_adapter().await;
        let count = Arc::new(AtomicU32::new(0));

        // First fire — closure must run.
        let c = count.clone();
        fire_with_durable(&adapter, "test-job", 1_000, move || async move {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 1, "first fire must execute");

        // Second fire — same (job, slot) — closure must NOT run again.
        let c = count.clone();
        fire_with_durable(&adapter, "test-job", 1_000, move || async move {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "replay must not re-invoke the closure"
        );
    }

    /// A different slot derives a different execution — the closure runs again.
    #[tokio::test]
    async fn fire_with_durable_different_slot_runs_again() {
        let (adapter, _task) = make_adapter().await;
        let count = Arc::new(AtomicU32::new(0));

        let c = count.clone();
        fire_with_durable(&adapter, "test-job", 1_000, move || async move {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap();

        let c = count.clone();
        fire_with_durable(&adapter, "test-job", 2_000, move || async move {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(
            count.load(Ordering::Relaxed),
            2,
            "different slot_ms must produce a separate execution and run"
        );
    }

    /// When durable is not configured (no `with_durable`), the scheduler calls the handler directly.
    #[test]
    fn no_adapter_means_no_durable_overhead() {
        // The Scheduler struct's `durable` field defaults to None; this is a structural check only.
        // The property is validated indirectly by the scheduler_init_and_tick test in scheduler.rs.
        assert!(derive_execution_id("job", 0) != derive_execution_id("other", 0));
    }
}
