// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Supervised background task manager for the agent loop.
//!
//! [`BackgroundSupervisor`] wraps a [`tokio::task::JoinSet`] with per-class concurrency limits
//! and drop-on-overflow semantics. It is owned by the agent (not `Arc`-shared) and accessed
//! only via `&mut self`, avoiding any `Mutex` overhead on the hot path.
//!
//! # Task classes
//!
//! | Class | Limit | Drop policy | Examples |
//! |---|---|---|---|
//! | `Enrichment` | 4 | Drop | summarization, graph/persona/trajectory extraction |
//! | `Telemetry` | 8 | Drop | audit log writes, graph count sync |
//! | `ForegroundAdjacent` | 4 | Drop | magic docs update |
//!
//! # Critic-driven design decisions
//!
//! - **S1**: `unsummarized_count` is NOT shared via `AtomicUsize`. Background summarization
//!   signals completion via a [`SummarizationSignal`] stored inside the supervisor; the foreground
//!   reads it on `reap()` and resets the counter synchronously.
//! - **S2**: background tasks must NOT call `send_status`. Use `tracing` events only.
//! - **M1**: `class_inflight` is decremented inside the spawned future wrapper (via a drop guard),
//!   so concurrency slots free immediately on task completion, not only at `reap()`.
//! - **M2**: `OverflowPolicy` has only `Drop`; coalesce is not implemented.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::task::JoinSet;

/// Identifies the class of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskClass {
    /// Enrichment tasks spawned from `persist_message`: summarization, graph/persona/trajectory
    /// extraction. Lossy — dropping under load is acceptable.
    Enrichment,
    /// Telemetry/metrics updates: audit log writes, graph count sync. Small and fast.
    Telemetry,
}

impl TaskClass {
    fn index(self) -> usize {
        match self {
            TaskClass::Enrichment => 0,
            TaskClass::Telemetry => 1,
        }
    }

    fn max_concurrency(self) -> usize {
        match self {
            TaskClass::Enrichment => 4,
            TaskClass::Telemetry => 8,
        }
    }

    fn name(self) -> &'static str {
        match self {
            TaskClass::Enrichment => "enrichment",
            TaskClass::Telemetry => "telemetry",
        }
    }
}

// MVP: only Drop overflow policy is supported.
const NUM_CLASSES: usize = 2;

/// Signal that background summarization completed successfully.
/// Stored in `BackgroundSupervisor` and consumed by `reap()` to reset `unsummarized_count`
/// on the foreground without any shared mutable state between tasks.
#[derive(Debug, Default)]
pub(crate) struct SummarizationSignal {
    /// `true` when at least one background summarization task completed successfully this cycle.
    pub(crate) did_summarize: bool,
}

/// Per-class counters updated by the supervisor.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ClassMetrics {
    pub(crate) spawned: u64,
    pub(crate) dropped: u64,
    pub(crate) completed: u64,
}

/// Snapshot of supervisor metrics for logging and TUI display.
#[derive(Debug, Default, Clone)]
pub(crate) struct SupervisorMetrics {
    pub(crate) classes: [ClassMetrics; NUM_CLASSES],
    /// Total inflight tasks across all classes at snapshot time.
    pub(crate) inflight: usize,
}

impl SupervisorMetrics {
    pub(crate) fn total_dropped(&self) -> u64 {
        self.classes.iter().map(|c| c.dropped).sum()
    }

    pub(crate) fn total_completed(&self) -> u64 {
        self.classes.iter().map(|c| c.completed).sum()
    }
}

/// Supervised background task manager.
///
/// Owned by the agent, accessed via `&mut self`. Not `Clone` or `Send`.
pub(crate) struct BackgroundSupervisor {
    tasks: JoinSet<TaskResult>,
    /// Per-class inflight counters. Decremented inside spawned tasks via drop-guard (M1).
    class_inflight: [Arc<AtomicUsize>; NUM_CLASSES],
    /// Per-class metrics (spawned / dropped / completed). Updated in `spawn` and `reap`.
    class_metrics: [ClassMetrics; NUM_CLASSES],
}

/// Result produced by a supervised background task.
enum TaskResult {
    /// Normal completion — carries the originating class for correct metrics accounting.
    Done(TaskClass),
    /// Summarization ran successfully. Foreground should reset `unsummarized_count`.
    SummarizationDone,
}

/// RAII guard that decrements a class inflight counter when dropped.
///
/// Placed inside every spawned future so the concurrency slot is freed as soon as the
/// task future resolves, not when `reap()` next polls the `JoinSet` (M1).
struct InflightGuard(Arc<AtomicUsize>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // Saturating: avoids underflow if somehow called more than once.
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
    }
}

impl BackgroundSupervisor {
    pub(crate) fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            class_inflight: std::array::from_fn(|_| Arc::new(AtomicUsize::new(0))),
            class_metrics: [ClassMetrics::default(); NUM_CLASSES],
        }
    }

    /// Spawn a background task under `class`.
    ///
    /// Returns `true` when the task was accepted, `false` when the class concurrency limit
    /// was reached and the task was dropped (drop-on-overflow, MVP policy).
    pub(crate) fn spawn(
        &mut self,
        class: TaskClass,
        name: &'static str,
        fut: impl Future<Output = ()> + Send + 'static,
    ) -> bool {
        let idx = class.index();
        let current = self.class_inflight[idx].load(Ordering::Relaxed);
        if current >= class.max_concurrency() {
            tracing::debug!(
                class = class.name(),
                task = name,
                limit = class.max_concurrency(),
                "background task dropped: concurrency limit reached"
            );
            self.class_metrics[idx].dropped += 1;
            return false;
        }

        self.class_inflight[idx].fetch_add(1, Ordering::Relaxed);
        let guard = InflightGuard(Arc::clone(&self.class_inflight[idx]));
        self.class_metrics[idx].spawned += 1;

        self.tasks.spawn(async move {
            let _guard = guard; // dropped when future resolves
            fut.await;
            TaskResult::Done(class)
        });

        tracing::debug!(class = class.name(), task = name, "background task spawned");
        true
    }

    /// Variant of [`spawn`] for summarization tasks that need to signal completion.
    ///
    /// When the inner future completes successfully, `reap()` will return
    /// `SummarizationSignal { did_summarize: true }` so the foreground can reset
    /// `unsummarized_count` synchronously without shared mutable state (S1 fix).
    pub(crate) fn spawn_summarization(
        &mut self,
        name: &'static str,
        fut: impl Future<Output = bool> + Send + 'static,
    ) -> bool {
        let class = TaskClass::Enrichment;
        let idx = class.index();
        let current = self.class_inflight[idx].load(Ordering::Relaxed);
        if current >= class.max_concurrency() {
            tracing::debug!(
                class = class.name(),
                task = name,
                limit = class.max_concurrency(),
                "summarization task dropped: concurrency limit reached"
            );
            self.class_metrics[idx].dropped += 1;
            return false;
        }

        self.class_inflight[idx].fetch_add(1, Ordering::Relaxed);
        let guard = InflightGuard(Arc::clone(&self.class_inflight[idx]));
        self.class_metrics[idx].spawned += 1;

        self.tasks.spawn(async move {
            let _guard = guard;
            let did_summarize = fut.await;
            if did_summarize {
                TaskResult::SummarizationDone
            } else {
                TaskResult::Done(TaskClass::Enrichment)
            }
        });

        tracing::debug!(
            class = class.name(),
            task = name,
            "summarization task spawned"
        );
        true
    }

    /// Poll all completed tasks without blocking.
    ///
    /// Returns a [`SummarizationSignal`] if any background summarization task completed
    /// during this reap cycle. The caller must reset `unsummarized_count` synchronously
    /// when `signal.did_summarize` is true.
    pub(crate) fn reap(&mut self) -> SummarizationSignal {
        let mut signal = SummarizationSignal::default();

        // Drain any already-completed tasks.
        while let Some(result) = self.tasks.try_join_next() {
            match result {
                Ok(TaskResult::Done(class)) => {
                    self.class_metrics[class.index()].completed += 1;
                }
                Ok(TaskResult::SummarizationDone) => {
                    self.class_metrics[TaskClass::Enrichment.index()].completed += 1;
                    signal.did_summarize = true;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "background task panicked");
                }
            }
        }

        signal
    }

    /// Abort all inflight tasks immediately.
    ///
    /// Called during agent shutdown. Logging only — enrichment is lossy by design.
    pub(crate) fn abort_all(&mut self) {
        let remaining = self.tasks.len();
        if remaining > 0 {
            tracing::debug!(remaining, "aborting background tasks on shutdown");
        }
        self.tasks.abort_all();
    }

    /// Current total inflight tasks across all classes.
    pub(crate) fn inflight(&self) -> usize {
        self.class_inflight
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .sum()
    }

    /// Wait for all inflight tasks to complete (test helper only).
    ///
    /// Production code uses [`reap`] (non-blocking) or [`abort_all`] (shutdown).
    /// Wait for all inflight tasks to complete and return the aggregated signal (test helper only).
    ///
    /// Production code uses [`reap`] (non-blocking) or [`abort_all`] (shutdown).
    #[cfg(test)]
    pub(crate) async fn join_all_for_test(&mut self) -> SummarizationSignal {
        let mut signal = SummarizationSignal::default();
        while let Some(result) = self.tasks.join_next().await {
            match result {
                Ok(TaskResult::SummarizationDone) => {
                    self.class_metrics[TaskClass::Enrichment.index()].completed += 1;
                    signal.did_summarize = true;
                }
                Ok(TaskResult::Done(class)) => {
                    self.class_metrics[class.index()].completed += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "background task panicked in test");
                }
            }
        }
        signal
    }

    /// Snapshot of current metrics.
    pub(crate) fn metrics_snapshot(&self) -> SupervisorMetrics {
        SupervisorMetrics {
            classes: self.class_metrics,
            inflight: self.inflight(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn spawn_and_reap_basic() {
        let mut sv = BackgroundSupervisor::new();
        let (tx, rx) = oneshot::channel::<()>();

        let accepted = sv.spawn(TaskClass::Enrichment, "test-task", async move {
            let _ = rx.await;
        });
        assert!(accepted);
        assert_eq!(sv.inflight(), 1);

        tx.send(()).unwrap();
        // Give the task time to complete.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let signal = sv.reap();
        assert!(!signal.did_summarize);
        assert_eq!(sv.inflight(), 0);
    }

    #[tokio::test]
    async fn drop_on_overflow() {
        let mut sv = BackgroundSupervisor::new();
        let limit = TaskClass::Enrichment.max_concurrency();

        // Fill the class up to the limit.
        let mut txs = Vec::new();
        for _ in 0..limit {
            let (tx, rx) = oneshot::channel::<()>();
            txs.push(tx);
            let accepted = sv.spawn(TaskClass::Enrichment, "blocking", async move {
                let _ = rx.await;
            });
            assert!(accepted);
        }

        // One more should be dropped.
        let dropped = sv.spawn(TaskClass::Enrichment, "should-drop", async {});
        assert!(!dropped);
        assert_eq!(sv.class_metrics[TaskClass::Enrichment.index()].dropped, 1);

        // Release all.
        for tx in txs {
            tx.send(()).ok();
        }
    }

    #[tokio::test]
    async fn summarization_signal_propagated() {
        let mut sv = BackgroundSupervisor::new();

        let accepted = sv.spawn_summarization("test-summarize", async { true });
        assert!(accepted);

        // Let the task run.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let signal = sv.reap();
        assert!(signal.did_summarize);
    }

    #[tokio::test]
    async fn abort_all_does_not_panic() {
        let mut sv = BackgroundSupervisor::new();
        sv.spawn(TaskClass::Telemetry, "long-running", async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        sv.abort_all();
        // abort_all signals cancellation but doesn't await — drop the supervisor to verify
        // no panic occurs when JoinSet is dropped with cancelled tasks.
        drop(sv);
    }

    #[tokio::test]
    async fn inflight_decremented_on_completion_not_reap() {
        let mut sv = BackgroundSupervisor::new();
        let (tx, rx) = oneshot::channel::<()>();

        sv.spawn(TaskClass::Enrichment, "t", async move {
            let _ = rx.await;
        });
        assert_eq!(sv.inflight(), 1);

        tx.send(()).unwrap();
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Inflight should be 0 even before reap() is called (M1 fix).
        assert_eq!(sv.inflight(), 0);

        sv.reap();
        assert_eq!(sv.inflight(), 0);
    }
}
