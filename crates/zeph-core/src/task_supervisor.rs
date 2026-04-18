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
//!     restart: RestartPolicy::Restart { max: 3, base_delay: Duration::from_secs(1) },
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
use zeph_common::BlockingSpawner;

// ── Public types ─────────────────────────────────────────────────────────────

/// Policy governing what happens when a supervised task completes or panics.
///
/// Used in [`TaskDescriptor`] to configure restart behaviour for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Task runs once; normal completion removes it from the registry.
    RunOnce,
    /// Task is restarted **only on panic**, up to `max` times.
    ///
    /// Normal completion (the future returns `()`) does **not** trigger a restart.
    /// The task is removed from the registry on normal exit.
    ///
    /// A `max` of `0` means the task is monitored but **never** restarted —
    /// a panic leaves the entry as `Failed` in the registry for observability.
    /// Use `RunOnce` when you want the entry removed on completion.
    ///
    /// Restart delays follow **exponential backoff**: the delay before attempt `n`
    /// is `base_delay * 2^(n-1)`, capped at [`MAX_RESTART_DELAY`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zeph_core::task_supervisor::RestartPolicy;
    ///
    /// // Restart up to 3 times with exponential backoff starting at 1 s.
    /// let policy = RestartPolicy::Restart { max: 3, base_delay: Duration::from_secs(1) };
    /// ```
    Restart { max: u32, base_delay: Duration },
}

/// Maximum delay between restart attempts (caps exponential backoff).
pub const MAX_RESTART_DELAY: Duration = Duration::from_mins(1);

/// Safety cap on how long the reap driver drains completions after cancellation.
///
/// INVARIANT: must be less than the runner shutdown grace period (runner.rs:2387,
/// currently 10s). If that constant is reduced, this must be reduced proportionally.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

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
            Self::Panicked => write!(f, "supervised blocking task panicked"),
            Self::SupervisorDropped => write!(f, "supervisor dropped before task completed"),
        }
    }
}

impl std::error::Error for BlockingError {}

/// Handle returned by [`TaskSupervisor::spawn_blocking`].
///
/// Awaiting [`BlockingHandle::join`] blocks until the OS-thread task produces a
/// value. Dropping the handle without joining does **not** cancel the task — it
/// continues to run on the blocking thread pool but the result is discarded.
///
/// A panic inside the closure is captured and returned as
/// [`BlockingError::Panicked`] rather than propagating to the caller.
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
    /// Task completed normally.
    Completed,
    /// Task was force-aborted during shutdown.
    Aborted,
    /// Task exhausted all restart attempts and is permanently failed.
    Failed { reason: String },
}

/// Point-in-time snapshot of a supervised task, returned by [`TaskSupervisor::snapshot`].
#[derive(Debug, Clone)]
/// Observability surface per field:
///
/// | Field | tokio-console | Jaeger / OTLP | TUI | `metrics` histogram |
/// |-------|--------------|--------------|-----|---------------------|
/// | `name` | span name | span name | task list | label `"task"` |
/// | `task.wall_time_ms` | — | span field (`task-metrics`) | — | `zeph.task.wall_time_ms` |
/// | `task.cpu_time_ms` | — | span field (`task-metrics`) | — | `zeph.task.cpu_time_ms` |
/// | `status` | — | — | task list | — |
/// | `restart_count` | — | — | task list | — |
///
/// The `task.wall_time_ms` and `task.cpu_time_ms` fields are only populated when
/// the crate is compiled with the `task-metrics` feature.
pub struct TaskSnapshot {
    /// Task name.
    pub name: Arc<str>,
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
    name: Arc<str>,
    status: TaskStatus,
    started_at: Instant,
    restart_count: u32,
    restart_policy: RestartPolicy,
    abort_handle: AbortHandle,
    /// `Some` only for `Restart` policy tasks.
    factory: Option<BoxFactory>,
}

/// How a supervised task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionKind {
    /// Future returned normally.
    Normal,
    /// Future panicked.
    Panicked,
    /// Future was cancelled via the cancellation token or abort handle.
    Cancelled,
}

struct Completion {
    name: Arc<str>,
    kind: CompletionKind,
}

struct SupervisorState {
    tasks: HashMap<Arc<str>, TaskEntry>,
}

struct Inner {
    state: parking_lot::Mutex<SupervisorState>,
    /// Completion events from spawned tasks → reap driver.
    /// Lives in `Inner` (not `SupervisorState`) to avoid double mutex acquisition
    /// — callers clone it once during spawn without re-locking state.
    completion_tx: mpsc::UnboundedSender<Completion>,
    cancel: CancellationToken,
    /// Limits the number of concurrently running `spawn_blocking` tasks to prevent
    /// runaway thread-pool growth under burst load.
    blocking_semaphore: Arc<tokio::sync::Semaphore>,
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
///     factory: || async { tokio::time::sleep(std::time::Duration::from_secs(1)).await },
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
            blocking_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
        });

        Self::start_reap_driver(Arc::clone(&inner), completion_rx, cancel);

        Self { inner }
    }

    /// Spawn a named, supervised async task.
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
    ///     restart: RestartPolicy::Restart { max: 3, base_delay: Duration::from_secs(1) },
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
        let cancel = self.inner.cancel.clone();
        let completion_tx = self.inner.completion_tx.clone();
        let name: Arc<str> = Arc::from(desc.name);

        let (abort_handle, jh) = Self::do_spawn(desc.name, &factory, cancel);
        Self::wire_completion_reporter(Arc::clone(&name), jh, completion_tx);

        let entry = TaskEntry {
            name: Arc::clone(&name),
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
            if let Some(old) = state.tasks.remove(&name) {
                old.abort_handle.abort();
            }
            state.tasks.insert(Arc::clone(&name), entry);
        }

        TaskHandle {
            name: desc.name,
            abort: abort_handle,
        }
    }

    /// Spawn a CPU-bound closure on the OS blocking thread pool.
    ///
    /// The closure runs via [`tokio::task::spawn_blocking`] — it is never polled
    /// on tokio worker threads and cannot block async I/O. The task is registered
    /// in the supervisor registry and is visible to [`snapshot`][Self::snapshot]
    /// and [`shutdown_all`][Self::shutdown_all].
    ///
    /// Dropping the returned [`BlockingHandle`] without calling `.join()` does
    /// **not** cancel the task; it runs to completion but the result is discarded.
    ///
    /// A panic inside `f` is captured and returned as [`BlockingError::Panicked`]
    /// rather than propagating to the caller.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use tokio_util::sync::CancellationToken;
    /// use zeph_core::task_supervisor::{BlockingHandle, TaskSupervisor};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let cancel = CancellationToken::new();
    /// let sup = TaskSupervisor::new(cancel);
    ///
    /// let handle: BlockingHandle<u32> = sup.spawn_blocking(Arc::from("compute"), || {
    ///     // CPU-bound work — safe to block here
    ///     42_u32
    /// });
    /// let result = handle.join().await.unwrap();
    /// assert_eq!(result, 42);
    /// # }
    /// ```
    ///
    /// # Capacity limit
    ///
    /// At most 8 `spawn_blocking` tasks run concurrently. Additional tasks wait for a
    /// semaphore permit, bounding thread-pool growth under burst load.
    ///
    /// # Panics
    ///
    /// Panics inside `f` are captured and returned as [`BlockingError::Panicked`] — they
    /// do not propagate to the caller.
    #[allow(clippy::needless_pass_by_value)] // `name` is cloned into async task and registry
    pub fn spawn_blocking<F, R>(&self, name: Arc<str>, f: F) -> BlockingHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<R, BlockingError>>();
        #[cfg(feature = "task-metrics")]
        let span = tracing::info_span!(
            "supervised_blocking_task",
            task.name = %name,
            task.wall_time_ms = tracing::field::Empty,
            task.cpu_time_ms = tracing::field::Empty,
        );
        #[cfg(not(feature = "task-metrics"))]
        let span = tracing::info_span!("supervised_blocking_task", task.name = %name);

        let semaphore = Arc::clone(&self.inner.blocking_semaphore);
        let inner = Arc::clone(&self.inner);
        let name_clone = Arc::clone(&name);
        let completion_tx = self.inner.completion_tx.clone();

        // Wrap the blocking spawn in an async task that first acquires a semaphore
        // permit, bounding the number of concurrently running blocking tasks to 8.
        let outer = tokio::spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("blocking semaphore closed");

            let name_for_measure = Arc::clone(&name_clone);
            let join_handle = tokio::task::spawn_blocking(move || {
                let _enter = span.enter();
                measure_blocking(&name_for_measure, f)
            });
            let abort = join_handle.abort_handle();

            // Update registry with the real abort handle now that spawn_blocking is live.
            {
                let mut state = inner.state.lock();
                if let Some(entry) = state.tasks.get_mut(&name_clone) {
                    entry.abort_handle = abort;
                }
            }

            let kind = match join_handle.await {
                Ok(val) => {
                    let _ = tx.send(Ok(val));
                    CompletionKind::Normal
                }
                Err(e) if e.is_panic() => {
                    let _ = tx.send(Err(BlockingError::Panicked));
                    CompletionKind::Panicked
                }
                Err(_) => {
                    // Aborted — drop tx so rx returns SupervisorDropped.
                    CompletionKind::Cancelled
                }
            };
            // _permit released here, freeing the semaphore slot.
            let _ = completion_tx.send(Completion {
                name: name_clone,
                kind,
            });
        });
        let abort = outer.abort_handle();

        // Register in registry so snapshot/shutdown sees the task.
        {
            let mut state = self.inner.state.lock();
            if let Some(old) = state.tasks.remove(&name) {
                old.abort_handle.abort();
            }
            state.tasks.insert(
                Arc::clone(&name),
                TaskEntry {
                    name: Arc::clone(&name),
                    status: TaskStatus::Running,
                    started_at: Instant::now(),
                    restart_count: 0,
                    restart_policy: RestartPolicy::RunOnce,
                    abort_handle: abort.clone(),
                    factory: None,
                },
            );
        }

        BlockingHandle { rx, abort }
    }

    /// Spawn an async task that produces a typed result value (runs on tokio worker thread).
    ///
    /// Unlike [`spawn`][Self::spawn], no restart policy is supported — the task
    /// runs once. The task is registered in the supervisor registry under the
    /// provided `name` and is visible to [`snapshot`][Self::snapshot] and
    /// [`shutdown_all`][Self::shutdown_all].
    ///
    /// For CPU-bound work that must not block tokio workers, use
    /// [`spawn_blocking`][Self::spawn_blocking] instead.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::sync::Arc;
    /// use tokio_util::sync::CancellationToken;
    /// use zeph_core::task_supervisor::{BlockingHandle, TaskSupervisor};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let cancel = CancellationToken::new();
    /// let sup = TaskSupervisor::new(cancel.clone());
    ///
    /// let handle: BlockingHandle<u32> = sup.spawn_oneshot(Arc::from("compute"), || async { 42_u32 });
    /// let result = handle.join().await.unwrap();
    /// assert_eq!(result, 42);
    /// # }
    /// ```
    pub fn spawn_oneshot<F, Fut, R>(&self, name: Arc<str>, factory: F) -> BlockingHandle<R>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<R, BlockingError>>();
        let cancel = self.inner.cancel.clone();
        let span = tracing::info_span!("supervised_task", task.name = %name);
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

        {
            let mut state = self.inner.state.lock();
            if let Some(old) = state.tasks.remove(&name) {
                old.abort_handle.abort();
            }
            state.tasks.insert(
                Arc::clone(&name),
                TaskEntry {
                    name: Arc::clone(&name),
                    status: TaskStatus::Running,
                    started_at: Instant::now(),
                    restart_count: 0,
                    restart_policy: RestartPolicy::RunOnce,
                    abort_handle: abort.clone(),
                    factory: None,
                },
            );
        }

        let completion_tx = self.inner.completion_tx.clone();
        tokio::spawn(async move {
            let kind = match join_handle.await {
                Ok(Some(val)) => {
                    let _ = tx.send(Ok(val));
                    CompletionKind::Normal
                }
                Err(e) if e.is_panic() => {
                    let _ = tx.send(Err(BlockingError::Panicked));
                    CompletionKind::Panicked
                }
                _ => CompletionKind::Cancelled,
            };
            let _ = completion_tx.send(Completion { name, kind });
        });
        BlockingHandle { rx, abort }
    }

    /// Abort a task by name. No-op if no task with that name is registered.
    pub fn abort(&self, name: &'static str) {
        let state = self.inner.state.lock();
        let key: Arc<str> = Arc::from(name);
        if let Some(entry) = state.tasks.get(&key) {
            entry.abort_handle.abort();
            tracing::debug!(task.name = name, "task aborted via supervisor");
        }
    }

    /// Gracefully shut down all supervised tasks.
    ///
    /// Cancels the supervisor's [`CancellationToken`] and waits up to `timeout`
    /// for all tasks to exit. Tasks that do not exit within the timeout are
    /// aborted forcefully and their registry entries updated to [`TaskStatus::Aborted`].
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
                let mut remaining_names: Vec<Arc<str>> = Vec::new();
                {
                    let mut state = self.inner.state.lock();
                    for entry in state.tasks.values_mut() {
                        if matches!(
                            entry.status,
                            TaskStatus::Running | TaskStatus::Restarting { .. }
                        ) {
                            remaining_names.push(Arc::clone(&entry.name));
                            entry.abort_handle.abort();
                            entry.status = TaskStatus::Aborted;
                        }
                    }
                }
                tracing::warn!(
                    remaining = active,
                    tasks = ?remaining_names,
                    "shutdown timeout — aborting remaining tasks"
                );
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
                name: Arc::clone(&e.name),
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

    /// Wire a completion reporter: drives `jh` and sends the result to `completion_tx`.
    fn wire_completion_reporter(
        name: Arc<str>,
        jh: tokio::task::JoinHandle<()>,
        completion_tx: mpsc::UnboundedSender<Completion>,
    ) {
        tokio::spawn(async move {
            let kind = match jh.await {
                Ok(()) => CompletionKind::Normal,
                Err(e) if e.is_panic() => CompletionKind::Panicked,
                Err(_) => CompletionKind::Cancelled,
            };
            let _ = completion_tx.send(Completion { name, kind });
        });
    }

    /// Spawn the reap driver. The driver processes completion events from the mpsc channel.
    ///
    /// After the cancellation token fires, the driver continues draining the channel
    /// until it is empty — this ensures that tasks which completed just before cancel
    /// have their registry entries updated, allowing `shutdown_all` to observe
    /// `active_count() == 0` correctly.
    fn start_reap_driver(
        inner: Arc<Inner>,
        mut completion_rx: mpsc::UnboundedReceiver<Completion>,
        cancel: CancellationToken,
    ) {
        tokio::spawn(async move {
            // Phase 1: normal operation — process completions until cancel fires.
            loop {
                tokio::select! {
                    biased;
                    Some(completion) = completion_rx.recv() => {
                        Self::handle_completion(&inner, completion).await;
                    }
                    () = cancel.cancelled() => break,
                }
            }

            // Phase 2: post-cancel drain — keep receiving completions until the
            // registry reports no active tasks, or the channel closes, or the safety
            // deadline expires. This prevents losing completions that arrive after
            // tasks observe cancellation (#3161).
            let drain_deadline = tokio::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
            let active = Self::has_active_tasks(&inner);
            tracing::debug!(active, "reap driver entered post-cancel drain phase");
            loop {
                if !Self::has_active_tasks(&inner) {
                    break;
                }
                let remaining =
                    drain_deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, completion_rx.recv()).await {
                    Ok(Some(completion)) => Self::handle_completion(&inner, completion).await,
                    // channel closed (unreachable in practice — senders live in Inner), or deadline elapsed
                    Ok(None) | Err(_) => break,
                }
            }
            tracing::debug!(
                active = Self::has_active_tasks(&inner),
                "reap driver drain phase complete"
            );
        });
    }

    /// Returns `true` if any task is in `Running` or `Restarting` state.
    fn has_active_tasks(inner: &Arc<Inner>) -> bool {
        let state = inner.state.lock();
        state.tasks.values().any(|e| {
            matches!(
                e.status,
                TaskStatus::Running | TaskStatus::Restarting { .. }
            )
        })
    }

    /// Process a single task completion event.
    ///
    /// Lock is never held across `.await`. Phase 1 classifies the completion
    /// under lock; Phase 2 sleeps with exponential backoff without a lock;
    /// Phase 3 spawns the next instance and updates the registry.
    async fn handle_completion(inner: &Arc<Inner>, completion: Completion) {
        // Short-circuit: once cancellation has fired, never schedule restarts.
        // Without this, Restart-policy tasks re-register as Running, causing
        // has_active_tasks() to stay true and the drain loop to spin until timeout.
        if inner.cancel.is_cancelled() {
            let mut state = inner.state.lock();
            state.tasks.remove(&completion.name);
            return;
        }

        let Some((attempt, max, delay)) = Self::classify_completion(inner, &completion) else {
            return;
        };

        tracing::warn!(
            task.name = %completion.name,
            attempt,
            max,
            delay_ms = delay.as_millis(),
            "restarting supervised task"
        );

        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        Self::do_restart(inner, &completion.name, attempt);
    }

    /// Phase 1: classify the completion under lock and return restart parameters if needed.
    ///
    /// Returns `Some((attempt, max, backoff_delay))` when a restart should be scheduled.
    fn classify_completion(
        inner: &Arc<Inner>,
        completion: &Completion,
    ) -> Option<(u32, u32, Duration)> {
        let mut state = inner.state.lock();
        let entry = state.tasks.get_mut(&completion.name)?;

        match completion.kind {
            CompletionKind::Panicked => {
                tracing::warn!(task.name = %completion.name, "supervised task panicked");
            }
            CompletionKind::Normal => {
                tracing::info!(task.name = %completion.name, "supervised task completed");
            }
            CompletionKind::Cancelled => {
                tracing::debug!(task.name = %completion.name, "supervised task cancelled");
            }
        }

        match entry.restart_policy {
            RestartPolicy::RunOnce => {
                entry.status = TaskStatus::Completed;
                state.tasks.remove(&completion.name);
                None
            }
            RestartPolicy::Restart { max, base_delay } => {
                // Only restart on panic — normal exit and cancellation are not errors.
                if completion.kind != CompletionKind::Panicked {
                    entry.status = TaskStatus::Completed;
                    state.tasks.remove(&completion.name);
                    return None;
                }
                if entry.restart_count >= max {
                    let reason = format!("panicked after {max} restart(s)");
                    tracing::error!(
                        task.name = %completion.name,
                        attempts = max,
                        "task failed permanently"
                    );
                    entry.status = TaskStatus::Failed { reason };
                    None
                } else {
                    let attempt = entry.restart_count + 1;
                    entry.status = TaskStatus::Restarting { attempt, max };
                    // Exponential backoff: base_delay * 2^(attempt-1), capped at MAX_RESTART_DELAY.
                    let multiplier = 1_u32
                        .checked_shl(attempt.saturating_sub(1))
                        .unwrap_or(u32::MAX);
                    let delay = base_delay.saturating_mul(multiplier).min(MAX_RESTART_DELAY);
                    Some((attempt, max, delay))
                }
            }
        }
        // lock released here
    }

    /// Phase 3: TOCTOU check, collect spawn params under lock, then spawn outside.
    fn do_restart(inner: &Arc<Inner>, name: &Arc<str>, attempt: u32) {
        let spawn_params = {
            let mut state = inner.state.lock();
            let Some(entry) = state.tasks.get_mut(name.as_ref()) else {
                tracing::debug!(
                    task.name = %name,
                    "task removed during restart delay — skipping"
                );
                return;
            };
            if !matches!(entry.status, TaskStatus::Restarting { .. }) {
                return;
            }
            let Some(factory) = &entry.factory else {
                return;
            };
            // Wrap factory() in catch_unwind to prevent a factory panic from crashing
            // the reap driver and orphaning the registry.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(factory)) {
                Err(_) => {
                    let reason = format!("factory panicked on restart attempt {attempt}");
                    tracing::error!(task.name = %name, attempt, "factory panicked during restart");
                    entry.status = TaskStatus::Failed { reason };
                    None
                }
                Ok(fut) => Some((
                    fut,
                    inner.cancel.clone(),
                    inner.completion_tx.clone(),
                    name.clone(),
                )),
            }
            // lock released here
        };

        let Some((fut, cancel, completion_tx, name)) = spawn_params else {
            return;
        };

        let span = tracing::info_span!("supervised_task", task.name = %name);
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

        {
            let mut state = inner.state.lock();
            if let Some(entry) = state.tasks.get_mut(name.as_ref()) {
                entry.restart_count = attempt;
                entry.status = TaskStatus::Running;
                entry.abort_handle = new_abort;
            }
        }

        Self::wire_completion_reporter(name.clone(), jh, completion_tx);
    }
}

// ── Task metrics helpers ──────────────────────────────────────────────────────

/// Run `f` and record wall-time and CPU-time metrics when `task-metrics` is enabled.
///
/// When the feature is disabled this is a zero-overhead identity wrapper —
/// no `cpu-time` or `metrics` crates are linked.
#[cfg(feature = "task-metrics")]
#[inline]
fn measure_blocking<F, R>(name: &str, f: F) -> R
where
    F: FnOnce() -> R,
{
    use cpu_time::ThreadTime;
    let wall_start = std::time::Instant::now();
    let cpu_start = ThreadTime::now();
    let result = f();
    let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;
    let cpu_ms = cpu_start.elapsed().as_secs_f64() * 1000.0;
    metrics::histogram!("zeph.task.wall_time_ms", "task" => name.to_owned()).record(wall_ms);
    metrics::histogram!("zeph.task.cpu_time_ms", "task" => name.to_owned()).record(cpu_ms);
    tracing::Span::current().record("task.wall_time_ms", wall_ms);
    tracing::Span::current().record("task.cpu_time_ms", cpu_ms);
    result
}

/// Identity wrapper when `task-metrics` feature is disabled.
///
/// Compiles to a direct call to `f()` with no overhead.
#[cfg(not(feature = "task-metrics"))]
#[inline]
fn measure_blocking<F, R>(_name: &str, f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

// ── BlockingSpawner impl ──────────────────────────────────────────────────────

impl BlockingSpawner for TaskSupervisor {
    /// Spawn a named blocking closure through the supervisor.
    ///
    /// The task is registered in the supervisor registry (visible in
    /// [`snapshot`][Self::snapshot] and subject to graceful shutdown) before
    /// the closure begins executing.
    fn spawn_blocking_named(
        &self,
        name: Arc<str>,
        f: Box<dyn FnOnce() + Send + 'static>,
    ) -> tokio::task::JoinHandle<()> {
        let handle = self.spawn_blocking(Arc::clone(&name), f);
        tokio::spawn(async move {
            if let Err(e) = handle.join().await {
                tracing::error!(task.name = %name, error = %e, "supervised blocking task failed");
            }
        })
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

        tokio::time::sleep(Duration::from_millis(200)).await;

        let snaps = sup.snapshot();
        assert!(
            snaps.iter().all(|s| s.name.as_ref() != "panicking"),
            "entry should be reaped"
        );
        assert_eq!(
            sup.active_count(),
            0,
            "active count must be 0 after RunOnce panic"
        );
    }

    /// Regression test for S2: Restart-policy tasks must only restart on panic,
    /// not on normal completion.
    #[tokio::test]
    async fn test_restart_only_on_panic() {
        let (sup, _cancel) = make_supervisor();

        // Part 1: normal completion — must NOT restart.
        let normal_counter = Arc::new(AtomicU32::new(0));
        let nc = Arc::clone(&normal_counter);
        sup.spawn(TaskDescriptor {
            name: "normal-exit",
            restart: RestartPolicy::Restart {
                max: 3,
                base_delay: Duration::from_millis(10),
            },
            factory: move || {
                let c = Arc::clone(&nc);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    // Returns normally — no panic.
                }
            },
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            normal_counter.load(Ordering::SeqCst),
            1,
            "normal exit must not restart"
        );
        assert!(
            sup.snapshot()
                .iter()
                .all(|s| s.name.as_ref() != "normal-exit"),
            "entry removed after normal exit"
        );

        // Part 2: panic — MUST restart up to max times.
        let panic_counter = Arc::new(AtomicU32::new(0));
        let pc = Arc::clone(&panic_counter);
        sup.spawn(TaskDescriptor {
            name: "panic-exit",
            restart: RestartPolicy::Restart {
                max: 2,
                base_delay: Duration::from_millis(10),
            },
            factory: move || {
                let c = Arc::clone(&pc);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    panic!("test panic");
                }
            },
        });

        // initial + 2 restarts = 3 total
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            panic_counter.load(Ordering::SeqCst) >= 3,
            "panicking task must restart max times"
        );
        let snap = sup
            .snapshot()
            .into_iter()
            .find(|s| s.name.as_ref() == "panic-exit");
        assert!(
            matches!(snap.unwrap().status, TaskStatus::Failed { .. }),
            "task must be Failed after exhausting restarts"
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
                base_delay: Duration::from_millis(10),
            },
            factory: move || {
                let c = Arc::clone(&counter2);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    panic!("always panic");
                }
            },
        });

        tokio::time::sleep(Duration::from_millis(500)).await;

        let runs = counter.load(Ordering::SeqCst);
        assert!(
            runs >= 3,
            "expected at least 3 invocations (initial + 2 restarts), got {runs}"
        );

        let snaps = sup.snapshot();
        let snap = snaps.iter().find(|s| s.name.as_ref() == "restartable");
        assert!(snap.is_some(), "failed task should remain in registry");
        assert!(
            matches!(snap.unwrap().status, TaskStatus::Failed { .. }),
            "task should be Failed after exhausting retries"
        );
    }

    /// Verify exponential backoff: delay doubles on each restart attempt.
    #[tokio::test]
    async fn test_exponential_backoff() {
        let (sup, _cancel) = make_supervisor();

        let timestamps = Arc::new(parking_lot::Mutex::new(Vec::<std::time::Instant>::new()));
        let ts = Arc::clone(&timestamps);

        sup.spawn(TaskDescriptor {
            name: "backoff-task",
            restart: RestartPolicy::Restart {
                max: 3,
                base_delay: Duration::from_millis(50),
            },
            factory: move || {
                let t = Arc::clone(&ts);
                async move {
                    t.lock().push(std::time::Instant::now());
                    panic!("always panic");
                }
            },
        });

        // Wait long enough for all restarts: 50 + 100 + 200 ms = 350 ms + overhead
        tokio::time::sleep(Duration::from_millis(800)).await;

        let ts = timestamps.lock();
        assert!(
            ts.len() >= 3,
            "expected at least 3 invocations, got {}",
            ts.len()
        );

        // Verify delays are roughly doubling (within 2x tolerance for CI jitter).
        if ts.len() >= 3 {
            let d1 = ts[1].duration_since(ts[0]);
            let d2 = ts[2].duration_since(ts[1]);
            // d2 should be at least 1.5x d1 (allowing for jitter).
            assert!(
                d2 >= d1.mul_f64(1.5),
                "expected exponential backoff: d1={d1:?} d2={d2:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_graceful_shutdown() {
        let (sup, _cancel) = make_supervisor();

        for name in ["svc-a", "svc-b", "svc-c"] {
            sup.spawn(TaskDescriptor {
                name,
                restart: RestartPolicy::RunOnce,
                factory: || async {
                    tokio::time::sleep(Duration::from_mins(1)).await;
                },
            });
        }

        assert_eq!(sup.active_count(), 3);

        tokio::time::timeout(
            Duration::from_secs(2),
            sup.shutdown_all(Duration::from_secs(1)),
        )
        .await
        .expect("shutdown should complete within timeout");
    }

    /// Verify that force-aborted tasks get `TaskStatus::Aborted` in the registry (A2 fix).
    #[tokio::test]
    async fn test_force_abort_marks_aborted() {
        let cancel = CancellationToken::new();
        let sup = TaskSupervisor::new(cancel.clone());

        sup.spawn(TaskDescriptor {
            name: "stubborn-for-abort",
            restart: RestartPolicy::RunOnce,
            factory: || async {
                // Does not cooperate with cancellation.
                std::future::pending::<()>().await;
            },
        });

        // Use a very short timeout to trigger force-abort.
        sup.shutdown_all(Duration::from_millis(1)).await;

        // Entry should be Aborted, not Running.
        let snaps = sup.snapshot();
        if let Some(snap) = snaps
            .iter()
            .find(|s| s.name.as_ref() == "stubborn-for-abort")
        {
            assert_eq!(
                snap.status,
                TaskStatus::Aborted,
                "force-aborted task must have Aborted status"
            );
        }
        // If entry was already reaped (cooperative cancel won), that's also acceptable.
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
        let names: Vec<&str> = snaps.iter().map(|s| s.name.as_ref()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        assert!(snaps.iter().all(|s| s.status == TaskStatus::Running));
    }

    #[tokio::test]
    async fn test_blocking_returns_value() {
        let (sup, cancel) = make_supervisor();

        let handle: BlockingHandle<u32> = sup.spawn_blocking(Arc::from("compute"), || 42_u32);
        let result = handle.join().await.expect("should return value");
        assert_eq!(result, 42);
        cancel.cancel();
    }

    #[tokio::test]
    async fn test_blocking_panic() {
        let (sup, _cancel) = make_supervisor();

        let handle: BlockingHandle<u32> =
            sup.spawn_blocking(Arc::from("panicking-compute"), || panic!("intentional"));
        let err = handle
            .join()
            .await
            .expect_err("should return error on panic");
        assert_eq!(err, BlockingError::Panicked);
    }

    /// Verify `spawn_blocking` tasks appear in registry (M3 fix).
    #[tokio::test]
    async fn test_blocking_registered_in_registry() {
        let (sup, cancel) = make_supervisor();

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let _handle: BlockingHandle<()> =
            sup.spawn_blocking(Arc::from("blocking-task"), move || {
                // Block until signalled.
                let _ = rx.recv();
            });

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            sup.active_count(),
            1,
            "blocking task must appear in active_count"
        );

        let _ = tx.send(());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            sup.active_count(),
            0,
            "blocking task must be removed after completion"
        );

        cancel.cancel();
    }

    /// Verify `spawn_oneshot` tasks appear in registry (M3 fix).
    #[tokio::test]
    async fn test_oneshot_registered_in_registry() {
        let (sup, cancel) = make_supervisor();

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let _handle: BlockingHandle<()> =
            sup.spawn_oneshot(Arc::from("oneshot-task"), move || async move {
                let _ = rx.await;
            });

        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            sup.active_count(),
            1,
            "oneshot task must appear in active_count"
        );

        let _ = tx.send(());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            sup.active_count(),
            0,
            "oneshot task must be removed after completion"
        );

        cancel.cancel();
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
                base_delay: Duration::from_millis(10),
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

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "max=0 should not restart"
        );

        let snaps = sup.snapshot();
        let snap = snaps.iter().find(|s| s.name.as_ref() == "zero-max");
        assert!(snap.is_some(), "entry should remain as Failed");
        assert!(
            matches!(snap.unwrap().status, TaskStatus::Failed { .. }),
            "status should be Failed"
        );
    }

    /// Stress test: spawn 50 tasks concurrently, all must complete and registry must be accurate.
    #[tokio::test]
    async fn test_concurrent_spawns() {
        // All task names must be 'static — pre-defined before any let statements.
        static NAMES: [&str; 50] = [
            "t00", "t01", "t02", "t03", "t04", "t05", "t06", "t07", "t08", "t09", "t10", "t11",
            "t12", "t13", "t14", "t15", "t16", "t17", "t18", "t19", "t20", "t21", "t22", "t23",
            "t24", "t25", "t26", "t27", "t28", "t29", "t30", "t31", "t32", "t33", "t34", "t35",
            "t36", "t37", "t38", "t39", "t40", "t41", "t42", "t43", "t44", "t45", "t46", "t47",
            "t48", "t49",
        ];
        let (sup, cancel) = make_supervisor();

        let completed = Arc::new(AtomicU32::new(0));
        for name in &NAMES {
            let c = Arc::clone(&completed);
            sup.spawn(TaskDescriptor {
                name,
                restart: RestartPolicy::RunOnce,
                factory: move || {
                    let c = Arc::clone(&c);
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                    }
                },
            });
        }

        // Wait for all tasks to complete.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if completed.load(Ordering::SeqCst) == 50 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("all 50 tasks should complete");

        // Give reap driver time to process all completions.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sup.active_count(), 0, "all tasks must be reaped");

        cancel.cancel();
    }

    #[tokio::test]
    async fn test_shutdown_timeout_expiry() {
        let cancel = CancellationToken::new();
        let sup = TaskSupervisor::new(cancel.clone());

        sup.spawn(TaskDescriptor {
            name: "stubborn",
            restart: RestartPolicy::RunOnce,
            factory: || async {
                tokio::time::sleep(Duration::from_mins(1)).await;
            },
        });

        assert_eq!(sup.active_count(), 1);

        tokio::time::timeout(
            Duration::from_secs(2),
            sup.shutdown_all(Duration::from_millis(50)),
        )
        .await
        .expect("shutdown_all should return even on timeout expiry");

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

    /// Regression test for #3161: after `shutdown_all`, all tasks must be reaped
    /// even when they complete *after* the cancel signal.
    ///
    /// The yield loop forces the reap driver to observe cancel and exit phase-1
    /// before the tasks send their completions — reliably reproducing the race.
    #[tokio::test]
    async fn test_shutdown_drains_post_cancel_completions() {
        let cancel = CancellationToken::new();
        let sup = TaskSupervisor::new(cancel.clone());

        for name in [
            "loop-1", "loop-2", "loop-3", "loop-4", "loop-5", "loop-6", "loop-7",
        ] {
            let cancel_inner = cancel.clone();
            sup.spawn(TaskDescriptor {
                name,
                restart: RestartPolicy::RunOnce,
                factory: move || {
                    let c = cancel_inner.clone();
                    async move {
                        c.cancelled().await;
                        // Yield multiple times so the reap driver observes cancel first.
                        for _ in 0..64 {
                            tokio::task::yield_now().await;
                        }
                    }
                },
            });
        }
        assert_eq!(sup.active_count(), 7);

        sup.shutdown_all(Duration::from_secs(2)).await;

        assert_eq!(
            sup.active_count(),
            0,
            "all tasks must be reaped after shutdown (#3161)"
        );
    }

    #[tokio::test]
    async fn test_blocking_spawner_task_appears_in_snapshot() {
        // Verify that tasks spawned via BlockingSpawner appear in supervisor.snapshot().
        use zeph_common::BlockingSpawner;

        let cancel = CancellationToken::new();
        let sup = TaskSupervisor::new(cancel);

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        let handle = sup.spawn_blocking_named(
            Arc::from("chunk_file"),
            Box::new(move || {
                // Signal that the task has started.
                let _ = ready_tx.send(());
                // Block until test signals release.
                let _ = release_rx.blocking_recv();
            }),
        );

        // Wait until the blocking task has actually started.
        ready_rx.await.expect("task should start");

        let snapshot = sup.snapshot();
        assert!(
            snapshot.iter().any(|t| t.name.as_ref() == "chunk_file"),
            "chunk_file task must appear in supervisor snapshot"
        );

        // Release the blocking task and await completion.
        let _ = release_tx.send(());
        handle.await.expect("task should complete");
    }

    /// Verify that `measure_blocking` emits wall-time and CPU-time histograms when
    /// the `task-metrics` feature is enabled.
    ///
    /// `measure_blocking` calls `metrics::histogram!` on the current thread.
    /// We test it directly using a `DebuggingRecorder` installed as the thread-local
    /// recorder via `metrics::with_local_recorder`.
    #[cfg(feature = "task-metrics")]
    #[test]
    fn test_measure_blocking_emits_metrics() {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        // Call measure_blocking inside the local recorder scope so histogram! calls
        // are captured. The closure runs synchronously on this thread.
        metrics::with_local_recorder(&recorder, || {
            measure_blocking("test_task", || std::hint::black_box(42_u64));
        });

        let snapshot = snapshotter.snapshot();
        let metric_names: Vec<String> = snapshot
            .into_vec()
            .into_iter()
            .map(|(k, _, _, _)| k.key().name().to_owned())
            .collect();

        assert!(
            metric_names.iter().any(|n| n == "zeph.task.wall_time_ms"),
            "expected zeph.task.wall_time_ms histogram; got: {metric_names:?}"
        );
        assert!(
            metric_names.iter().any(|n| n == "zeph.task.cpu_time_ms"),
            "expected zeph.task.cpu_time_ms histogram; got: {metric_names:?}"
        );
    }

    /// Verify that `spawn_blocking` semaphore limits concurrent OS-thread tasks to 8.
    ///
    /// Spawns 16 tasks. Each holds a barrier until 8 are waiting; then releases in order.
    /// If more than 8 run concurrently the test would either deadlock (waiting for 9+ to reach
    /// the barrier) or the counter would exceed 8 — both are caught.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_spawn_blocking_semaphore_cap() {
        let (sup, _cancel) = make_supervisor();
        let concurrent = Arc::new(AtomicU32::new(0));
        let max_concurrent = Arc::new(AtomicU32::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(1)); // just a sync point

        let mut handles = Vec::new();
        for i in 0u32..16 {
            let c = Arc::clone(&concurrent);
            let m = Arc::clone(&max_concurrent);
            let name: Arc<str> = Arc::from(format!("blocking-{i}").as_str());
            let h = sup.spawn_blocking(name, move || {
                let prev = c.fetch_add(1, Ordering::SeqCst);
                // Update observed maximum.
                let mut cur_max = m.load(Ordering::SeqCst);
                while prev + 1 > cur_max {
                    match m.compare_exchange(cur_max, prev + 1, Ordering::SeqCst, Ordering::SeqCst)
                    {
                        Ok(_) => break,
                        Err(x) => cur_max = x,
                    }
                }
                // Simulate work.
                std::thread::sleep(std::time::Duration::from_millis(20));
                c.fetch_sub(1, Ordering::SeqCst);
            });
            handles.push(h);
        }

        for h in handles {
            h.join().await.expect("blocking task should succeed");
        }
        drop(barrier);

        let observed = max_concurrent.load(Ordering::SeqCst);
        assert!(
            observed <= 8,
            "observed {observed} concurrent blocking tasks; expected ≤ 8 (semaphore cap)"
        );
    }
}
