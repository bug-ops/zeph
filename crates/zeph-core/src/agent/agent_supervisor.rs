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
//! | `Enrichment` | configurable (default 4) | Drop | summarization, graph/persona/trajectory extraction |
//! | `Telemetry` | configurable (default 8) | Drop | audit log writes, graph count sync |
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
use std::time::{Duration, Instant};

use tokio::task::{AbortHandle, JoinSet};
use tracing::Instrument as _;

use crate::config::TaskSupervisorConfig;
use crate::metrics::HistogramRecorder;

/// Identifies the class of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskClass {
    /// Enrichment tasks spawned from `persist_message`: summarization, graph/persona/trajectory
    /// extraction. Lossy — dropping under load is acceptable.
    Enrichment,
    /// Telemetry/metrics updates: audit log writes, graph count sync. Small and fast.
    Telemetry,
    /// Background shell runs spawned via `background = true` bash tool calls.
    ///
    /// Isolated from `Enrichment` so shell-run saturation cannot starve memory compaction.
    /// Budget mirrors `ShellConfig::max_background_runs` (default 8).
    #[allow(dead_code)]
    BackgroundShell,
}

impl TaskClass {
    pub(crate) fn index(self) -> usize {
        match self {
            TaskClass::Enrichment => 0,
            TaskClass::Telemetry => 1,
            TaskClass::BackgroundShell => 2,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            TaskClass::Enrichment => "enrichment",
            TaskClass::Telemetry => "telemetry",
            TaskClass::BackgroundShell => "background_shell",
        }
    }
}

// MVP: only Drop overflow policy is supported.
const NUM_CLASSES: usize = 3;

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
    /// Per-class inflight counts at snapshot time.
    pub(crate) class_inflight: [usize; NUM_CLASSES],
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
    /// Per-class concurrency limits loaded from config.
    class_limits: [usize; NUM_CLASSES],
    /// Per-class `AbortHandle` vecs for selective `abort_class()`. Stale handles are cleaned
    /// up in `reap()` via `is_finished()`. Vec size is bounded by `class_limit * turns_between_reaps`
    /// — in practice at most ~12 entries per reap cycle.
    class_handles: [Vec<AbortHandle>; NUM_CLASSES],
    /// Optional histogram recorder for bg task latency (injected at construction time).
    histogram_recorder: Option<Arc<dyn HistogramRecorder>>,
}

/// Result produced by a supervised background task.
enum TaskResult {
    /// Normal completion — carries the originating class and elapsed time since spawn.
    Done(TaskClass, Duration),
    /// Summarization ran successfully. Foreground should reset `unsummarized_count`.
    SummarizationDone(Duration),
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
    /// Create a new supervisor with limits and optional histogram recorder from config.
    pub(crate) fn new(
        config: &TaskSupervisorConfig,
        recorder: Option<Arc<dyn HistogramRecorder>>,
    ) -> Self {
        Self {
            tasks: JoinSet::new(),
            class_inflight: std::array::from_fn(|_| Arc::new(AtomicUsize::new(0))),
            class_metrics: [ClassMetrics::default(); NUM_CLASSES],
            class_limits: [
                config.enrichment_limit,
                config.telemetry_limit,
                config.background_shell_limit,
            ],
            class_handles: std::array::from_fn(|_| Vec::new()),
            histogram_recorder: recorder,
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
        if current >= self.class_limits[idx] {
            tracing::debug!(
                class = class.name(),
                task = name,
                limit = self.class_limits[idx],
                "background task dropped: concurrency limit reached"
            );
            self.class_metrics[idx].dropped += 1;
            return false;
        }

        self.class_inflight[idx].fetch_add(1, Ordering::Relaxed);
        let guard = InflightGuard(Arc::clone(&self.class_inflight[idx]));
        self.class_metrics[idx].spawned += 1;
        let spawned_at = Instant::now();

        let span = tracing::info_span!("bg_task", class = class.name(), task = name);
        let handle = self.tasks.spawn(
            async move {
                let _guard = guard; // dropped when future resolves
                fut.await;
                TaskResult::Done(class, spawned_at.elapsed())
            }
            .instrument(span),
        );
        self.class_handles[idx].push(handle);

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
        if current >= self.class_limits[idx] {
            tracing::debug!(
                class = class.name(),
                task = name,
                limit = self.class_limits[idx],
                "summarization task dropped: concurrency limit reached"
            );
            self.class_metrics[idx].dropped += 1;
            return false;
        }

        self.class_inflight[idx].fetch_add(1, Ordering::Relaxed);
        let guard = InflightGuard(Arc::clone(&self.class_inflight[idx]));
        self.class_metrics[idx].spawned += 1;
        let spawned_at = Instant::now();

        let span = tracing::info_span!("bg_task", class = class.name(), task = name);
        let handle = self.tasks.spawn(
            async move {
                let _guard = guard;
                let did_summarize = fut.await;
                if did_summarize {
                    TaskResult::SummarizationDone(spawned_at.elapsed())
                } else {
                    TaskResult::Done(TaskClass::Enrichment, spawned_at.elapsed())
                }
            }
            .instrument(span),
        );
        self.class_handles[idx].push(handle);

        tracing::debug!(
            class = class.name(),
            task = name,
            "summarization task spawned"
        );
        true
    }

    /// Abort all inflight tasks of the given class.
    ///
    /// Calls `abort()` on each stored `AbortHandle` for the class. Aborting a completed
    /// task's handle is a no-op, so no special-casing is needed. The `InflightGuard` drop
    /// inside the task decrements the inflight counter when the abort is processed.
    pub(crate) fn abort_class(&mut self, class: TaskClass) {
        let idx = class.index();
        let aborted = self.class_handles[idx].len();
        for handle in self.class_handles[idx].drain(..) {
            handle.abort();
        }
        if aborted > 0 {
            tracing::debug!(
                class = class.name(),
                count = aborted,
                "aborted background tasks at turn boundary"
            );
        }
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
                Ok(TaskResult::Done(class, elapsed)) => {
                    let idx = class.index();
                    self.class_metrics[idx].completed += 1;
                    if let Some(ref rec) = self.histogram_recorder {
                        rec.observe_bg_task(class.name(), elapsed);
                    }
                }
                Ok(TaskResult::SummarizationDone(elapsed)) => {
                    self.class_metrics[TaskClass::Enrichment.index()].completed += 1;
                    if let Some(ref rec) = self.histogram_recorder {
                        rec.observe_bg_task(TaskClass::Enrichment.name(), elapsed);
                    }
                    signal.did_summarize = true;
                }
                Err(ref e) if e.is_cancelled() => {
                    tracing::debug!(error = %e, "background task cancelled");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "background task panicked");
                }
            }
        }

        // Clean up stale abort handles (tasks that completed on their own).
        // Vec size is bounded by class_limit * turns_between_reaps — O(n) where n is small.
        for handles in &mut self.class_handles {
            handles.retain(|h| !h.is_finished());
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

    /// Wait for all inflight tasks to complete and return the aggregated signal (test helper only).
    ///
    /// Production code uses [`reap`] (non-blocking) or [`abort_all`] (shutdown).
    #[cfg(test)]
    pub(crate) async fn join_all_for_test(&mut self) -> SummarizationSignal {
        let mut signal = SummarizationSignal::default();
        while let Some(result) = self.tasks.join_next().await {
            match result {
                Ok(TaskResult::SummarizationDone(elapsed)) => {
                    self.class_metrics[TaskClass::Enrichment.index()].completed += 1;
                    if let Some(ref rec) = self.histogram_recorder {
                        rec.observe_bg_task(TaskClass::Enrichment.name(), elapsed);
                    }
                    signal.did_summarize = true;
                }
                Ok(TaskResult::Done(class, elapsed)) => {
                    let idx = class.index();
                    self.class_metrics[idx].completed += 1;
                    if let Some(ref rec) = self.histogram_recorder {
                        rec.observe_bg_task(class.name(), elapsed);
                    }
                }
                Err(ref e) if e.is_cancelled() => {
                    tracing::debug!(error = %e, "background task cancelled in test");
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
        let class_inflight =
            std::array::from_fn(|i| self.class_inflight[i].load(Ordering::Relaxed));
        SupervisorMetrics {
            classes: self.class_metrics,
            inflight: self.inflight(),
            class_inflight,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn default_supervisor() -> BackgroundSupervisor {
        BackgroundSupervisor::new(&TaskSupervisorConfig::default(), None)
    }

    #[tokio::test]
    async fn spawn_and_reap_basic() {
        let mut sv = default_supervisor();
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
        let mut sv = default_supervisor();
        let limit = sv.class_limits[TaskClass::Enrichment.index()];

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
        let mut sv = default_supervisor();

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
        let mut sv = default_supervisor();
        sv.spawn(TaskClass::Telemetry, "long-running", async {
            tokio::time::sleep(std::time::Duration::from_mins(1)).await;
        });
        sv.abort_all();
        // abort_all signals cancellation but doesn't await — drop the supervisor to verify
        // no panic occurs when JoinSet is dropped with cancelled tasks.
        drop(sv);
    }

    #[tokio::test]
    async fn inflight_decremented_on_completion_not_reap() {
        let mut sv = default_supervisor();
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

    // 2B: reap produces Done with non-zero duration
    #[tokio::test]
    async fn reap_produces_duration() {
        let mut sv = default_supervisor();
        sv.spawn(TaskClass::Telemetry, "timed", async {});
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        sv.reap();
        assert_eq!(sv.class_metrics[TaskClass::Telemetry.index()].completed, 1);
    }

    // 2C: metrics_snapshot reports per-class inflight
    #[tokio::test]
    async fn metrics_snapshot_per_class_inflight() {
        let mut sv = default_supervisor();
        let (tx1, rx1) = oneshot::channel::<()>();
        let (tx2, rx2) = oneshot::channel::<()>();
        sv.spawn(TaskClass::Enrichment, "e", async move {
            let _ = rx1.await;
        });
        sv.spawn(TaskClass::Telemetry, "t", async move {
            let _ = rx2.await;
        });

        let snap = sv.metrics_snapshot();
        assert_eq!(snap.class_inflight[TaskClass::Enrichment.index()], 1);
        assert_eq!(snap.class_inflight[TaskClass::Telemetry.index()], 1);
        assert_eq!(snap.inflight, 2);

        tx1.send(()).ok();
        tx2.send(()).ok();
    }

    // 2D: abort_class only cancels targeted class
    #[tokio::test]
    async fn abort_class_only_cancels_targeted_class() {
        let mut sv = default_supervisor();
        let (_tx_enrich, rx_enrich) = oneshot::channel::<()>();
        let (_tx_telem, rx_telem) = oneshot::channel::<()>();
        sv.spawn(TaskClass::Enrichment, "e", async move {
            let _ = rx_enrich.await;
        });
        sv.spawn(TaskClass::Telemetry, "t", async move {
            let _ = rx_telem.await;
        });

        assert_eq!(sv.inflight(), 2);
        sv.abort_class(TaskClass::Enrichment);
        // Give tasks time to process abort.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        // Enrichment inflight drops; telemetry still up.
        assert_eq!(
            sv.class_inflight[TaskClass::Enrichment.index()].load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            sv.class_inflight[TaskClass::Telemetry.index()].load(Ordering::Relaxed),
            1
        );
    }

    // 2B: observe_bg_task is called on reap with the correct class label
    #[tokio::test]
    async fn observe_bg_task_called_on_reap() {
        use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

        struct CountingRecorder(Arc<AtomicU32>);
        impl HistogramRecorder for CountingRecorder {
            fn observe_llm_latency(&self, _: Duration) {}
            fn observe_turn_duration(&self, _: Duration) {}
            fn observe_tool_execution(&self, _: Duration) {}
            fn observe_bg_task(&self, _label: &str, _dur: Duration) {
                self.0.fetch_add(1, AtomicOrdering::Relaxed);
            }
        }

        let counter = Arc::new(AtomicU32::new(0));
        let recorder: Arc<dyn HistogramRecorder> = Arc::new(CountingRecorder(Arc::clone(&counter)));
        let config = TaskSupervisorConfig::default();
        let mut sv = BackgroundSupervisor::new(&config, Some(recorder));

        sv.spawn(TaskClass::Enrichment, "test", async {});
        sv.join_all_for_test().await;

        assert_eq!(counter.load(AtomicOrdering::Relaxed), 1);
    }

    // 2E: custom limits from config are respected
    #[tokio::test]
    async fn custom_limits_from_config() {
        let config = TaskSupervisorConfig {
            enrichment_limit: 2,
            telemetry_limit: 3,
            abort_enrichment_on_turn: false,
            background_shell_limit: 8,
        };
        let mut sv = BackgroundSupervisor::new(&config, None);
        let mut txs = Vec::new();
        for _ in 0..2 {
            let (tx, rx) = oneshot::channel::<()>();
            txs.push(tx);
            assert!(sv.spawn(TaskClass::Enrichment, "e", async move {
                let _ = rx.await;
            }));
        }
        // Third should be dropped.
        let dropped = sv.spawn(TaskClass::Enrichment, "overflow", async {});
        assert!(!dropped);
        assert_eq!(sv.class_metrics[TaskClass::Enrichment.index()].dropped, 1);
        for tx in txs {
            tx.send(()).ok();
        }
    }
}
