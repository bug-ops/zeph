// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Supervised lifecycle task manager for long-running named services.
//!
//! [`TaskSupervisor`] manages named, long-lived background tasks (config watcher,
//! scheduler loop, gateway, MCP connections, etc.) with restart policies, health
//! snapshots, and graceful shutdown. Unlike [`crate::agent::agent_supervisor::BackgroundSupervisor`]
//! (which is `&mut self`-only, lossy, and turn-scoped), `TaskSupervisor` is
//! `Clone + Send + Sync` and designed for the full agent session lifetime.
//!
//! # Design rationale
//!
//! - **Shared handle**: `Arc<Inner>` interior allows passing the supervisor to bootstrap
//!   code, TUI status display, and shutdown orchestration without lifetime coupling.
//! - **Event-driven reap**: An internal mpsc channel delivers completion events to a
//!   reap driver task; no polling interval required.
//! - **No `JoinSet`**: Individual `JoinHandle`s per task enable per-name abort, status
//!   tracking, and restart policies — `JoinSet` is better for homogeneous work.
//! - **Mutex held briefly**: `parking_lot::Mutex` guards only bookkeeping operations
//!   (insert/remove from `HashMap`). The lock is **never held across `.await`**.
//!
//! # Examples
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use tokio_util::sync::CancellationToken;
//! use zeph_core::task_supervisor::{RestartPolicy, TaskDescriptor, TaskSupervisor};
//!
//! # #[tokio::main]
//! # async fn main() {
//! let cancel = CancellationToken::new();
//! let supervisor = TaskSupervisor::new(cancel.clone());
//!
//! supervisor.spawn(TaskDescriptor {
//!     name: "my-service",
//!     restart: RestartPolicy::Restart { max: 3, delay: Duration::from_secs(1) },
//!     factory: || async { /* service loop */ },
//! });
//!
//! // Graceful shutdown — waits up to 5 s for all tasks to stop.
//! supervisor.shutdown_all(Duration::from_secs(5)).await;
//! # }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

// ── Public types ─────────────────────────────────────────────────────────────

/// Policy governing what happens when a supervised task completes or panics.
///
/// Used in [`TaskDescriptor`] to configure restart behaviour for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Task runs once; normal completion removes it from the registry.
    RunOnce,
    /// Task is restarted on panic or unexpected exit, up to `max` times.
    ///
    /// A `max` of `0` means the task is monitored but **never** restarted —
    /// it is treated as `RunOnce` for restart purposes but left as `Failed`
    /// in the registry for observability. Use `RunOnce` when you want the
    /// entry removed on completion.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_core::task_supervisor::RestartPolicy;
    ///
    /// let policy = RestartPolicy::Restart { max: 3, delay: Duration::from_secs(2) };
    /// ```
    Restart { max: u32, delay: Duration },
}

/// Configuration passed to [`TaskSupervisor::spawn`] to describe a supervised task.
///
/// `F` must be `Fn` (not `FnOnce`) to support restarts: the factory is called once on
/// initial spawn and once per restart attempt.
pub struct TaskDescriptor<F> {
    /// Unique name for this task (e.g., `"config-watcher"`, `"scheduler-loop"`).
    ///
    /// Names must be `'static` — they are typically compile-time string literals.
    /// Spawning a task with a name that already exists aborts the prior instance.
    pub name: &'static str,
    /// Restart policy applied when the task exits unexpectedly.
    pub restart: RestartPolicy,
    /// Factory called to produce a new future. Must be `Fn` for restart support.
    pub factory: F,
}

/// Opaque handle to a single supervised task.
///
/// Can be used to abort the task by name independently of the supervisor.
#[derive(Debug, Clone)]
pub struct TaskHandle {
    name: &'static str,
    abort: AbortHandle,
}

impl TaskHandle {
    /// Abort the task immediately.
    pub fn abort(&self) {
        tracing::debug!(task.name = self.name, "task aborted via handle");
        self.abort.abort();
    }

    /// Return the task's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// Error returned by [`BlockingHandle::join`].
#[derive(Debug, PartialEq, Eq)]
pub enum BlockingError {
    /// The task panicked before producing a result.
    Panicked,
    /// The supervisor (or the task's abort handle) was dropped before the task completed.
    SupervisorDropped,
}

impl std::fmt::Display for BlockingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Panicked => write!(f, "supervised task panicked"),
            Self::SupervisorDropped => write!(f, "supervisor dropped before task completed"),
        }
    }
}

impl std::error::Error for BlockingError {}

/// Handle returned by [`TaskSupervisor::spawn_blocking`].
///
/// Awaiting [`BlockingHandle::join`] blocks until the task produces a value.
/// Dropping the handle without joining does **not** cancel the task — it
/// continues to run but the result is discarded.
pub struct BlockingHandle<R> {
    rx: oneshot::Receiver<Result<R, BlockingError>>,
    abort: AbortHandle,
}

impl<R> BlockingHandle<R> {
    /// Await the task result.
    ///
    /// # Errors
    ///
    /// - [`BlockingError::Panicked`] — the task closure panicked.
    /// - [`BlockingError::SupervisorDropped`] — the task was aborted or the
    ///   supervisor was dropped before a value was produced.
    pub async fn join(self) -> Result<R, BlockingError> {
        self.rx
            .await
            .unwrap_or(Err(BlockingError::SupervisorDropped))
    }

    /// Abort the underlying task immediately.
    pub fn abort(&self) {
        self.abort.abort();
    }
}

/// Point-in-time state of a supervised task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Task is actively running.
    Running,
    /// Task is waiting for the restart delay before the next attempt.
    Restarting { attempt: u32, max: u32 },
    /// Task completed normally (only present for `RunOnce` tasks briefly before reaping).
    Completed,
    /// Task exhausted all restart attempts and is permanently failed.
    Failed { reason: String },
}

/// Point-in-time snapshot of a supervised task, returned by [`TaskSupervisor::snapshot`].
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    /// Task name.
    pub name: &'static str,
    /// Current status.
    pub status: TaskStatus,
    /// Instant the task was first spawned.
    pub started_at: Instant,
    /// Number of times the task has been restarted.
    pub restart_count: u32,
}

// ── Internal types ───────────────────────────────────────────────────────────

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
type BoxFactory = Box<dyn Fn() -> BoxFuture + Send + Sync>;

struct TaskEntry {
    name: &'static str,
    status: TaskStatus,
    started_at: Instant,
    restart_count: u32,
    restart_policy: RestartPolicy,
    abort_handle: AbortHandle,
    /// `Some` only for `Restart` policy tasks.
    factory: Option<BoxFactory>,
}

struct Completion {
    name: &'static str,
    panicked: bool,
}

struct SupervisorState {
    tasks: HashMap<&'static str, TaskEntry>,
}

struct Inner {
    state: parking_lot::Mutex<SupervisorState>,
    /// Completion events from spawned tasks → reap driver.
    /// Lives in `Inner` (not `SupervisorState`) to avoid double mutex acquisition
    /// — callers clone it once during spawn without re-locking state.
    completion_tx: mpsc::UnboundedSender<Completion>,
    cancel: CancellationToken,
}

// ── Main type ────────────────────────────────────────────────────────────────

/// Shared, cloneable handle to the supervised lifecycle task registry.
///
/// `TaskSupervisor` manages named, long-lived background tasks with restart
/// policies, health snapshots, and graceful shutdown. It is `Clone + Send + Sync`
/// so it can be distributed to bootstrap code, TUI, and shutdown orchestration
/// without any additional synchronisation.
///
/// # Thread safety
///
/// Interior state is guarded by a `parking_lot::Mutex`. The lock is **never**
/// held across `.await` points.
///
/// # Examples
///
/// ```rust,no_run
/// use std::time::Duration;
/// use tokio_util::sync::CancellationToken;
/// use zeph_core::task_supervisor::{RestartPolicy, TaskDescriptor, TaskSupervisor};
///
/// # #[tokio::main]
/// # async fn main() {
/// let cancel = CancellationToken::new();
/// let sup = TaskSupervisor::new(cancel.clone());
///
/// let _handle = sup.spawn(TaskDescriptor {
///     name: "watcher",
///     restart: RestartPolicy::RunOnce,
///     factory: || async { tokio::time::sleep(Duration::from_secs(1)).await },
/// });
///
/// sup.shutdown_all(Duration::from_secs(5)).await;
/// # }
/// ```
#[derive(Clone)]
pub struct TaskSupervisor {
    inner: Arc<Inner>,
}

impl TaskSupervisor {
    /// Create a new supervisor and start its reap driver.
    ///
    /// The `cancel` token is propagated into every spawned task via `tokio::select!`.
    /// When the token is cancelled, all tasks exit cooperatively on their next
    /// cancellation check. Call [`shutdown_all`][Self::shutdown_all] to wait for
    /// them to finish.
    #[must_use]
    pub fn new(cancel: CancellationToken) -> Self {
        // NOTE: unbounded channel is acceptable here because supervised tasks are
        // O(10–20) lifecycle services, not high-throughput work. Backpressure would
        // complicate the spawn path without practical benefit.
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            state: parking_lot::Mutex::new(SupervisorState {
                tasks: HashMap::new(),
            }),
            completion_tx,
            cancel: cancel.clone(),
        });

        Self::start_reap_driver(Arc::clone(&inner), completion_rx, cancel);

        Self { inner }
    }

    /// Spawn a named, supervised task.
    ///
    /// If a task with the same `name` already exists, it is aborted before the
    /// new one is started.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// use tokio_util::sync::CancellationToken;
    /// use zeph_core::task_supervisor::{RestartPolicy, TaskDescriptor, TaskHandle, TaskSupervisor};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let cancel = CancellationToken::new();
    /// let sup = TaskSupervisor::new(cancel.clone());
    ///
    /// let handle: TaskHandle = sup.spawn(TaskDescriptor {
    ///     name: "config-watcher",
    ///     restart: RestartPolicy::Restart { max: 3, delay: Duration::from_secs(1) },
    ///     factory: || async { /* watch loop */ },
    /// });
    /// # }
    /// ```
    pub fn spawn<F, Fut>(&self, desc: TaskDescriptor<F>) -> TaskHandle
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let factory: BoxFactory = Box::new(move || Box::pin((desc.factory)()));
        let (abort_handle, join_handle) =
            Self::do_spawn(desc.name, &factory, self.inner.cancel.clone());

        let entry = TaskEntry {
            name: desc.name,
            status: TaskStatus::Running,
            started_at: Instant::now(),
            restart_count: 0,
            restart_policy: desc.restart,
            abort_handle: abort_handle.clone(),
            factory: match desc.restart {
                RestartPolicy::RunOnce => None,
                RestartPolicy::Restart { .. } => Some(factory),
            },
        };

        {
            let mut state = self.inner.state.lock();
            // Abort any existing task with the same name.
            if let Some(old) = state.tasks.remove(desc.name) {
                old.abort_handle.abort();
            }
            state.tasks.insert(desc.name, entry);
        } // lock released here

        // Drive join_handle → completion channel.
        // completion_tx lives in Inner — no second mutex acquisition needed.
        let completion_tx = self.inner.completion_tx.clone();
        let name = desc.name;
        tokio::spawn(async move {
            let panicked = join_handle.await.is_err();
            let _ = completion_tx.send(Completion { name, panicked });
        });

        TaskHandle {
            name: desc.name,
            abort: abort_handle,
        }
    }

    /// Spawn a task that produces a typed result value.
    ///
    /// No restart policy is supported — the task runs once. Dropping the returned
    /// [`BlockingHandle`] without calling `.join()` does **not** cancel the task;
    /// the result is simply discarded.
    ///
    /// A panic inside the closure is captured and returned as
    /// [`BlockingError::Panicked`] rather than propagating to the caller.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use tokio_util::sync::CancellationToken;
    /// use zeph_core::task_supervisor::{BlockingHandle, TaskSupervisor};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let cancel = CancellationToken::new();
    /// let sup = TaskSupervisor::new(cancel);
    ///
    /// let handle: BlockingHandle<u32> = sup.spawn_blocking("compute", || async { 42_u32 });
    /// let result = handle.join().await.unwrap();
    /// assert_eq!(result, 42);
    /// # }
    /// ```
    pub fn spawn_blocking<F, Fut, R>(&self, name: &'static str, factory: F) -> BlockingHandle<R>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<R, BlockingError>>();
        let cancel = self.inner.cancel.clone();
        let span = tracing::info_span!("supervised_task", task.name = name);
        // Spawn the actual work. Returns Ok(R) on success, Err(JoinError) on panic/abort.
        let join_handle: tokio::task::JoinHandle<Option<R>> = tokio::spawn(
            async move {
                let fut = factory();
                tokio::select! {
                    result = fut => Some(result),
                    () = cancel.cancelled() => None,
                }
            }
            .instrument(span),
        );
        let abort = join_handle.abort_handle();
        // Drive the join handle to completion; map panic → BlockingError::Panicked.
        tokio::spawn(async move {
            match join_handle.await {
                Ok(Some(val)) => {
                    let _ = tx.send(Ok(val));
                }
                Err(e) if e.is_panic() => {
                    let _ = tx.send(Err(BlockingError::Panicked));
                }
                // Ok(None) = cancelled, Err(_) non-panic = aborted:
                // drop tx → rx.await returns SupervisorDropped.
                _ => {}
            }
        });
        BlockingHandle { rx, abort }
    }

    /// Abort a task by name. No-op if no task with that name is registered.
    pub fn abort(&self, name: &'static str) {
        let state = self.inner.state.lock();
        if let Some(entry) = state.tasks.get(name) {
            entry.abort_handle.abort();
            tracing::debug!(task.name = name, "task aborted via supervisor");
        }
    }

    /// Gracefully shut down all supervised tasks.
    ///
    /// Cancels the supervisor's [`CancellationToken`] and waits up to `timeout`
    /// for all tasks to exit. Tasks that do not exit within the timeout are
    /// aborted forcefully.
    ///
    /// # Note
    ///
    /// This cancels the token passed to [`TaskSupervisor::new`]. If you share
    /// that token with other subsystems, they will be cancelled too. Use a child
    /// token (`cancel.child_token()`) when the supervisor should not affect
    /// unrelated components.
    pub async fn shutdown_all(&self, timeout: Duration) {
        self.inner.cancel.cancel();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let active = self.active_count();
            if active == 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    remaining = active,
                    "shutdown timeout — aborting remaining tasks"
                );
                let state = self.inner.state.lock();
                for entry in state.tasks.values() {
                    entry.abort_handle.abort();
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Return a point-in-time snapshot of all registered tasks.
    ///
    /// Suitable for TUI status panels and structured logging. The returned
    /// list is sorted by `started_at` ascending.
    #[must_use]
    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        let state = self.inner.state.lock();
        let mut snaps: Vec<TaskSnapshot> = state
            .tasks
            .values()
            .map(|e| TaskSnapshot {
                name: e.name,
                status: e.status.clone(),
                started_at: e.started_at,
                restart_count: e.restart_count,
            })
            .collect();
        snaps.sort_by_key(|s| s.started_at);
        snaps
    }

    /// Return the number of tasks currently in `Running` or `Restarting` state.
    #[must_use]
    pub fn active_count(&self) -> usize {
        let state = self.inner.state.lock();
        state
            .tasks
            .values()
            .filter(|e| {
                matches!(
                    e.status,
                    TaskStatus::Running | TaskStatus::Restarting { .. }
                )
            })
            .count()
    }

    /// Return a clone of the supervisor's [`CancellationToken`].
    ///
    /// Callers can use this to check whether shutdown has been initiated.
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Spawn the actual tokio task. Returns `(AbortHandle, JoinHandle)`.
    fn do_spawn(
        name: &'static str,
        factory: &BoxFactory,
        cancel: CancellationToken,
    ) -> (AbortHandle, tokio::task::JoinHandle<()>) {
        let fut = factory();
        let span = tracing::info_span!("supervised_task", task.name = name);
        let jh = tokio::spawn(
            async move {
                tokio::select! {
                    () = fut => {},
                    () = cancel.cancelled() => {},
                }
            }
            .instrument(span),
        );
        let abort = jh.abort_handle();
        (abort, jh)
    }

    /// Spawn the reap driver. The driver processes completion events from the mpsc channel.
    fn start_reap_driver(
        inner: Arc<Inner>,
        mut completion_rx: mpsc::UnboundedReceiver<Completion>,
        cancel: CancellationToken,
    ) {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(completion) = completion_rx.recv() => {
                        Self::handle_completion(&inner, completion).await;
                    }
                    () = cancel.cancelled() => break,
                }
            }
        });
    }

    /// Process a single task completion event.
    ///
    /// S1 fix: all mutex operations are performed before any `.await`. The lock
    /// is never held across sleep or spawn.
    async fn handle_completion(inner: &Arc<Inner>, completion: Completion) {
        // --- Phase 1: lock, read policy, unlock ---
        let restart_info = {
            let mut state = inner.state.lock();
            let Some(entry) = state.tasks.get_mut(completion.name) else {
                // Task was removed (aborted externally) — nothing to do.
                return;
            };

            if completion.panicked {
                tracing::warn!(task.name = completion.name, "supervised task panicked");
            } else {
                tracing::info!(task.name = completion.name, "supervised task completed");
            }

            match entry.restart_policy {
                RestartPolicy::RunOnce => {
                    entry.status = TaskStatus::Completed;
                    state.tasks.remove(completion.name);
                    None // no restart
                }
                RestartPolicy::Restart { max, delay } => {
                    if entry.restart_count >= max {
                        // Retries exhausted — mark failed, keep in registry.
                        let reason = if completion.panicked {
                            format!("panicked after {max} restart(s)")
                        } else {
                            format!("exited after {max} restart(s)")
                        };
                        tracing::error!(
                            task.name = completion.name,
                            attempts = max,
                            "task failed permanently"
                        );
                        entry.status = TaskStatus::Failed { reason };
                        None
                    } else {
                        let attempt = entry.restart_count + 1;
                        entry.status = TaskStatus::Restarting { attempt, max };
                        Some((attempt, max, delay))
                    }
                }
            }
            // lock released here (end of block)
        };

        let Some((attempt, max, delay)) = restart_info else {
            return;
        };

        tracing::warn!(
            task.name = completion.name,
            attempt,
            max,
            delay_ms = delay.as_millis(),
            "restarting supervised task"
        );

        // --- Phase 2: sleep (no lock held) ---
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        // --- Phase 3: re-lock, TOCTOU check, spawn ---
        let mut state = inner.state.lock();
        let Some(entry) = state.tasks.get_mut(completion.name) else {
            // Task was aborted or removed during the sleep window — do not restart.
            tracing::debug!(
                task.name = completion.name,
                "task removed during restart delay — skipping restart"
            );
            return;
        };

        // Only restart if the entry is still in Restarting state (not re-spawned externally).
        if !matches!(entry.status, TaskStatus::Restarting { .. }) {
            return;
        }

        let Some(factory) = &entry.factory else {
            return;
        };

        // S2 fix: wrap factory() in catch_unwind so a panic in the factory itself
        // does not crash the reap driver and orphan the registry.
        let Ok(fut) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(factory)) else {
            let reason = format!("factory panicked on restart attempt {attempt}");
            tracing::error!(
                task.name = completion.name,
                attempt,
                "factory panicked during restart"
            );
            entry.status = TaskStatus::Failed { reason };
            return;
        };

        let cancel = inner.cancel.clone();
        let name = entry.name;
        let span = tracing::info_span!("supervised_task", task.name = name);
        let jh = tokio::spawn(
            async move {
                tokio::select! {
                    () = fut => {},
                    () = cancel.cancelled() => {},
                }
            }
            .instrument(span),
        );
        let new_abort = jh.abort_handle();

        entry.restart_count = attempt;
        entry.status = TaskStatus::Running;
        entry.abort_handle = new_abort;
        drop(state); // release before spawning completion reporter

        // completion_tx is in Inner — no re-lock needed.
        let completion_tx = inner.completion_tx.clone();
        tokio::spawn(async move {
            let panicked = jh.await.is_err();
            let _ = completion_tx.send(Completion { name, panicked });
        });
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::*;

    fn make_supervisor() -> (TaskSupervisor, CancellationToken) {
        let cancel = CancellationToken::new();
        let sup = TaskSupervisor::new(cancel.clone());
        (sup, cancel)
    }

    #[tokio::test]
    async fn test_spawn_and_complete() {
        let (sup, _cancel) = make_supervisor();

        let done = Arc::new(tokio::sync::Notify::new());
        let done2 = Arc::clone(&done);

        sup.spawn(TaskDescriptor {
            name: "simple",
            restart: RestartPolicy::RunOnce,
            factory: move || {
                let d = Arc::clone(&done2);
                async move {
                    d.notify_one();
                }
            },
        });

        tokio::time::timeout(Duration::from_secs(2), done.notified())
            .await
            .expect("task should complete");

        // Give reap driver a moment to process the completion.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            sup.active_count(),
            0,
            "RunOnce task should be removed after completion"
        );
    }

    #[tokio::test]
    async fn test_panic_capture() {
        let (sup, _cancel) = make_supervisor();

        sup.spawn(TaskDescriptor {
            name: "panicking",
            restart: RestartPolicy::RunOnce,
            factory: || async { panic!("intentional test panic") },
        });

        // Panic must not propagate to the test thread.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // RunOnce tasks are removed after reap.
        let snaps = sup.snapshot();
        assert!(
            snaps.iter().all(|s| s.name != "panicking"),
            "entry should be reaped"
        );
        assert_eq!(
            sup.active_count(),
            0,
            "active count must be 0 after RunOnce panic"
        );
    }

    #[tokio::test]
    async fn test_restart_policy() {
        let (sup, _cancel) = make_supervisor();

        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = Arc::clone(&counter);

        sup.spawn(TaskDescriptor {
            name: "restartable",
            restart: RestartPolicy::Restart {
                max: 2,
                delay: Duration::from_millis(10),
            },
            factory: move || {
                let c = Arc::clone(&counter2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    panic!("always panic");
                }
            },
        });

        // Wait for initial run + 2 restarts = 3 invocations total.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let runs = counter.load(Ordering::SeqCst);
        assert!(
            runs >= 3,
            "expected at least 3 invocations (initial + 2 restarts), got {runs}"
        );

        let snaps = sup.snapshot();
        let snap = snaps.iter().find(|s| s.name == "restartable");
        assert!(snap.is_some(), "failed task should remain in registry");
        assert!(
            matches!(snap.unwrap().status, TaskStatus::Failed { .. }),
            "task should be Failed after exhausting retries"
        );
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let (sup, _cancel) = make_supervisor();

        for name in ["svc-a", "svc-b", "svc-c"] {
            sup.spawn(TaskDescriptor {
                name,
                restart: RestartPolicy::RunOnce,
                factory: || async {
                    // Cooperative task — exits on cancellation.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                },
            });
        }

        assert_eq!(sup.active_count(), 3);

        // Shutdown should complete well within 2 s even though tasks sleep for 60 s.
        tokio::time::timeout(
            Duration::from_secs(2),
            sup.shutdown_all(Duration::from_secs(1)),
        )
        .await
        .expect("shutdown should complete within timeout");
    }

    #[tokio::test]
    async fn test_registry_snapshot() {
        let (sup, _cancel) = make_supervisor();

        for name in ["alpha", "beta"] {
            sup.spawn(TaskDescriptor {
                name,
                restart: RestartPolicy::RunOnce,
                factory: || async {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                },
            });
        }

        let snaps = sup.snapshot();
        assert_eq!(snaps.len(), 2);
        let names: Vec<_> = snaps.iter().map(|s| s.name).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(snaps.iter().all(|s| s.status == TaskStatus::Running));
    }

    #[tokio::test]
    async fn test_blocking_returns_value() {
        let (sup, _cancel) = make_supervisor();

        let handle: BlockingHandle<u32> = sup.spawn_blocking("compute", || async { 42_u32 });
        let result = handle.join().await.expect("should return value");
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_blocking_panic() {
        let (sup, _cancel) = make_supervisor();

        let handle: BlockingHandle<u32> =
            sup.spawn_blocking("panicking-compute", || async { panic!("intentional") });
        let err = handle
            .join()
            .await
            .expect_err("should return error on panic");
        assert_eq!(err, BlockingError::Panicked);
    }

    #[tokio::test]
    async fn test_restart_max_zero() {
        let (sup, _cancel) = make_supervisor();

        let counter = Arc::new(AtomicU32::new(0));
        let counter2 = Arc::clone(&counter);

        sup.spawn(TaskDescriptor {
            name: "zero-max",
            restart: RestartPolicy::Restart {
                max: 0,
                delay: Duration::from_millis(10),
            },
            factory: move || {
                let c = Arc::clone(&counter2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    panic!("always panic");
                }
            },
        });

        tokio::time::sleep(Duration::from_millis(200)).await;

        // max=0 means no restarts: exactly 1 invocation.
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "max=0 should not restart"
        );

        let snaps = sup.snapshot();
        let snap = snaps.iter().find(|s| s.name == "zero-max");
        assert!(snap.is_some(), "entry should remain as Failed");
        assert!(
            matches!(snap.unwrap().status, TaskStatus::Failed { .. }),
            "status should be Failed"
        );
    }

    #[tokio::test]
    async fn test_shutdown_timeout_expiry() {
        let cancel = CancellationToken::new();
        let sup = TaskSupervisor::new(cancel.clone());

        // Spawn a task that ignores cancellation (doesn't use select! on cancel token).
        sup.spawn(TaskDescriptor {
            name: "stubborn",
            restart: RestartPolicy::RunOnce,
            factory: || async {
                // This task cooperates with cancellation via the outer select! in do_spawn,
                // so we need to test the force-abort path instead.
                // The supervisor's select! will cancel this, so use a very short timeout.
                tokio::time::sleep(Duration::from_secs(60)).await;
            },
        });

        assert_eq!(sup.active_count(), 1);

        // Use a very short timeout — tasks won't exit fast enough via cooperative cancel.
        // We verify shutdown completes (force-aborts if needed) within the outer timeout.
        tokio::time::timeout(
            Duration::from_secs(2),
            sup.shutdown_all(Duration::from_millis(50)),
        )
        .await
        .expect("shutdown_all should return even on timeout expiry");

        // After shutdown, cancellation token must be cancelled.
        assert!(
            cancel.is_cancelled(),
            "cancel token must be cancelled after shutdown"
        );
    }

    #[tokio::test]
    async fn test_cancellation_token() {
        let cancel = CancellationToken::new();
        let sup = TaskSupervisor::new(cancel.clone());

        assert!(!sup.cancellation_token().is_cancelled());

        sup.shutdown_all(Duration::from_millis(100)).await;

        assert!(
            sup.cancellation_token().is_cancelled(),
            "token must be cancelled after shutdown"
        );
    }
}
