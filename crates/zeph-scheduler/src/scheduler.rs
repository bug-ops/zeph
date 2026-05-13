// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
#[allow(unused_imports)]
use zeph_db::sql;

use chrono::Utc;
use tokio::sync::{Mutex, mpsc, watch};

use crate::error::SchedulerError;
use crate::sanitize::sanitize_task_prompt;
use crate::store::JobStore;
use crate::task::{ScheduledTask, TaskDescriptor, TaskHandler, TaskKind, TaskMode};

/// Messages sent to the [`Scheduler`] over its control channel.
///
/// Obtain the sender from [`Scheduler::new`] or [`Scheduler::with_max_tasks`]
/// and use it to add or cancel tasks while the scheduler loop is running.
///
/// # Examples
///
/// ```rust,no_run
/// use tokio::sync::watch;
/// use zeph_scheduler::{JobStore, Scheduler, SchedulerMessage, TaskDescriptor, TaskKind, TaskMode};
/// use chrono::Utc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let store = JobStore::open("sqlite:scheduler.db").await?;
/// let (_shutdown_tx, shutdown_rx) = watch::channel(false);
/// let (_scheduler, msg_tx) = Scheduler::new(store, shutdown_rx);
///
/// // Add a one-shot task that runs immediately.
/// let desc = TaskDescriptor {
///     name: "generate-report".into(),
///     mode: TaskMode::OneShot { run_at: Utc::now() },
///     kind: TaskKind::Custom("report".into()),
///     config: serde_json::json!({"task": "Generate weekly report"}),
/// };
/// msg_tx.send(SchedulerMessage::Add(Box::new(desc))).await?;
///
/// // Cancel a previously registered task.
/// msg_tx.send(SchedulerMessage::Cancel("generate-report".into())).await?;
/// # Ok(())
/// # }
/// ```
pub enum SchedulerMessage {
    /// Register a new task (or replace an existing one with the same name).
    Add(Box<TaskDescriptor>),
    /// Cancel and delete the task with the given name.
    Cancel(String),
}

/// Cron-based periodic task scheduler.
///
/// `Scheduler` owns the in-memory task list and drives execution on a configurable
/// tick interval. It persists job state to `SQLite` via [`JobStore`] so task schedules
/// survive restarts.
///
/// # Creation
///
/// Use [`Scheduler::new`] (defaults: 100-task cap, 60-second tick) or
/// [`Scheduler::with_max_tasks`] to set a custom capacity.
///
/// # Registration
///
/// - **Before start**: call [`Scheduler::add_task`] and [`Scheduler::register_handler`].
/// - **At runtime**: send [`SchedulerMessage::Add`] / [`SchedulerMessage::Cancel`]
///   on the `mpsc::Sender` returned by the constructor.
///
/// # Lifecycle
///
/// ```text
/// Scheduler::new()  →  add_task / register_handler  →  init()  →  run()
///                                                                      │
///                                                            shutdown_rx receives true
///                                                                      │
///                                                                    exit
/// ```
pub struct Scheduler {
    tasks: Vec<ScheduledTask>,
    store: JobStore,
    handlers: HashMap<String, Box<dyn TaskHandler>>,
    shutdown_rx: watch::Receiver<bool>,
    task_rx: mpsc::Receiver<SchedulerMessage>,
    /// Optional sender for injecting custom task prompts into the agent loop.
    custom_task_tx: Option<mpsc::Sender<String>>,
    max_tasks: usize,
    /// Per-task execution mutex: task names of tasks currently being executed.
    ///
    /// SIGNIFICANT-5: prevents concurrent executions of the same task when the
    /// handler is slow and `catch_up_missed` + `tick` overlap.
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl Scheduler {
    /// Create a scheduler with a default task cap of 100 and a 60-second tick interval.
    ///
    /// Returns `(Scheduler, sender)` where `sender` is used to add or cancel tasks at
    /// runtime via [`SchedulerMessage`].
    #[must_use]
    pub fn new(
        store: JobStore,
        shutdown_rx: watch::Receiver<bool>,
    ) -> (Self, mpsc::Sender<SchedulerMessage>) {
        Self::with_max_tasks(store, shutdown_rx, 100)
    }

    /// Create a scheduler with a custom maximum number of concurrent tasks.
    ///
    /// Tasks arriving via the control channel when `max_tasks` is already reached are
    /// silently dropped and a warning is emitted via `tracing`.
    ///
    /// Returns `(Scheduler, sender)` where `sender` is used to add or cancel tasks at
    /// runtime via [`SchedulerMessage`].
    #[must_use]
    pub fn with_max_tasks(
        store: JobStore,
        shutdown_rx: watch::Receiver<bool>,
        max_tasks: usize,
    ) -> (Self, mpsc::Sender<SchedulerMessage>) {
        let (tx, rx) = mpsc::channel(64);
        let scheduler = Self {
            tasks: Vec::new(),
            store,
            handlers: HashMap::new(),
            shutdown_rx,
            task_rx: rx,
            custom_task_tx: None,
            max_tasks,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        };
        (scheduler, tx)
    }

    /// Attach a sender for injecting custom task prompts into the agent loop.
    #[must_use]
    pub fn with_custom_task_sender(mut self, tx: mpsc::Sender<String>) -> Self {
        self.custom_task_tx = Some(tx);
        self
    }

    /// Add a task to the scheduler.
    ///
    /// This method must be called before [`Scheduler::init`]. To add tasks while the
    /// scheduler is already running, send a [`SchedulerMessage::Add`] on the control
    /// channel instead.
    pub fn add_task(&mut self, task: ScheduledTask) {
        self.tasks.push(task);
    }

    /// Register a handler for tasks of the given kind.
    ///
    /// When a task is due, the scheduler looks up its [`TaskKind`]'s string key and
    /// calls the matching handler. Tasks whose kind has no registered handler are
    /// skipped with a debug-level log.
    pub fn register_handler(&mut self, kind: &TaskKind, handler: Box<dyn TaskHandler>) {
        self.handlers.insert(kind.as_str().to_owned(), handler);
    }

    /// Initialize the store, sync task definitions, compute initial `next_run` for each task,
    /// and hydrate any CLI-added periodic jobs that live only in the DB back into `self.tasks`.
    ///
    /// Static tasks registered via [`Scheduler::add_task`] are upserted into the store first.
    /// Then all periodic jobs stored in the DB that are not already present in `self.tasks`
    /// (by name) are reconstructed from their persisted `cron_expr` and appended — this ensures
    /// that jobs added via the CLI (which write directly to the store) are visible to
    /// `tick` and [`Scheduler::catch_up_missed`] on the next startup.
    ///
    /// # Errors
    ///
    /// Returns an error if DB init, upsert, `next_run` persistence, or job listing fails.
    #[allow(clippy::too_many_lines)]
    pub async fn init(&mut self) -> Result<(), SchedulerError> {
        self.store.init().await?;
        let now = Utc::now();
        for task in &self.tasks {
            match &task.mode {
                TaskMode::Periodic { schedule } => {
                    self.store
                        .upsert_job_with_mode(
                            &task.name,
                            &schedule.to_string(),
                            task.kind.as_str(),
                            "periodic",
                            None,
                            "",
                        )
                        .await?;
                    // Always set next_run for periodic tasks if not already persisted.
                    if self.store.get_next_run(&task.name).await?.is_none() {
                        match schedule.after(&now).next() {
                            Some(next) => {
                                self.store
                                    .set_next_run(&task.name, &next.to_rfc3339())
                                    .await?;
                            }
                            None => {
                                tracing::warn!(
                                    task = %task.name,
                                    "cron produces no future occurrence, skipping next_run"
                                );
                            }
                        }
                    }
                }
                TaskMode::OneShot { run_at } => {
                    self.store
                        .upsert_job_with_mode(
                            &task.name,
                            "",
                            task.kind.as_str(),
                            "oneshot",
                            Some(&run_at.to_rfc3339()),
                            "",
                        )
                        .await?;
                }
            }
        }

        // Hydrate periodic jobs added via CLI (or other out-of-process writers) that were
        // persisted in the store but never registered in self.tasks. Without this step,
        // tick() and catch_up_missed() silently ignore them on every restart.
        let stored_jobs = self.store.list_jobs_full().await?;
        // Collect owned strings to release the borrow on self.tasks before mutating it below.
        let static_names: std::collections::HashSet<String> =
            self.tasks.iter().map(|t| t.name.clone()).collect();

        for job in stored_jobs {
            if job.task_mode != "periodic" || static_names.contains(&job.name) {
                continue;
            }
            match ScheduledTask::periodic(
                job.name.clone(),
                &job.cron_expr,
                crate::task::TaskKind::from_str_kind(&job.kind),
                serde_json::Value::Null,
            ) {
                Ok(task) => {
                    // Compute next_run if not already stored (same logic as for static tasks).
                    if self.store.get_next_run(&job.name).await?.is_none()
                        && let Some(schedule) = task.cron_schedule()
                    {
                        match schedule.after(&now).next() {
                            Some(next) => {
                                if let Err(e) =
                                    self.store.set_next_run(&job.name, &next.to_rfc3339()).await
                                {
                                    tracing::warn!(
                                        task = %job.name,
                                        "failed to persist next_run for hydrated job: {e}"
                                    );
                                }
                            }
                            None => {
                                tracing::warn!(
                                    task = %job.name,
                                    "cron produces no future occurrence, skipping next_run"
                                );
                            }
                        }
                    }
                    tracing::debug!(task = %job.name, "hydrated CLI-added periodic job from store");
                    self.tasks.push(task);
                }
                Err(e) => {
                    tracing::error!(
                        task = %job.name,
                        cron_expr = %job.cron_expr,
                        "skipping persisted job with invalid cron expression: {e}"
                    );
                    if let Err(db_err) = self.store.mark_error(&job.name).await {
                        tracing::warn!(
                            task = %job.name,
                            "failed to mark job as error in store: {db_err}"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    /// Fire overdue periodic tasks once on startup, then advance their `next_run`.
    ///
    /// For each periodic task whose `next_run <= now`, the task is executed via
    /// the registered handler exactly once. One-shot tasks are handled by the
    /// normal `tick()` path and are NOT replayed here.
    ///
    /// SIGNIFICANT-5: uses the same `in_flight` mutex as `tick()` so that
    /// `catch_up_missed` and a concurrent `tick()` cannot execute the same task.
    ///
    /// # Errors
    ///
    /// Returns the first error encountered during store or handler operations.
    pub async fn catch_up_missed(&mut self) -> Result<(), SchedulerError> {
        let _span =
            tracing::info_span!("scheduler.daemon.catch_up", tasks = self.tasks.len()).entered();

        let now = chrono::Utc::now();
        let mut replayed = 0usize;

        // Collect overdue periodic tasks first so we don't borrow self.tasks while executing.
        let overdue: Vec<_> = {
            let mut v = Vec::new();
            for task in &self.tasks {
                let TaskMode::Periodic { .. } = &task.mode else {
                    continue;
                };
                if let Ok(Some(ref s)) = self.store.get_next_run(&task.name).await
                    && s.parse::<chrono::DateTime<chrono::Utc>>()
                        .is_ok_and(|dt| dt <= now)
                {
                    v.push(task.name.clone());
                }
            }
            v
        };

        for name in &overdue {
            // Per-task mutex: skip if already running (safety against overlap with tick).
            {
                let mut guard = self.in_flight.lock().await;
                if guard.contains(name.as_str()) {
                    tracing::debug!(task = %name, "catch_up_missed: task in-flight, skipping");
                    continue;
                }
                guard.insert(name.clone());
            }

            let result = self.run_periodic_task_by_name(name, &now).await;

            self.in_flight.lock().await.remove(name.as_str());

            match result {
                Ok(true) => replayed += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(task = %name, "catch_up_missed: handler error: {e}"),
            }
        }

        tracing::info!(replayed, "catch_up_missed complete");
        Ok(())
    }

    /// Execute a named periodic task and advance its `next_run`.
    ///
    /// Returns `Ok(true)` if the task was found and executed, `Ok(false)` if not found.
    async fn run_periodic_task_by_name(
        &self,
        name: &str,
        now: &chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, SchedulerError> {
        let Some(task) = self.tasks.iter().find(|t| t.name == name) else {
            return Ok(false);
        };
        let TaskMode::Periodic { schedule } = &task.mode else {
            return Ok(false);
        };
        let Some(handler) = self.handlers.get(task.kind.as_str()) else {
            tracing::debug!(task = %name, "catch_up_missed: no handler, skipping");
            return Ok(false);
        };

        tracing::info!(task = %name, "catch_up_missed: executing overdue task");
        handler.execute(&task.config).await?;

        let next = schedule
            .after(now)
            .next()
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
        self.store
            .record_run(name, &now.to_rfc3339(), &next)
            .await?;
        Ok(true)
    }

    /// Run the scheduler loop with a configurable tick interval and graceful shutdown window.
    ///
    /// The interval is clamped to `5..=3600` seconds. Missed ticks are skipped to avoid
    /// burst storms. After the shutdown channel fires, in-flight ticks are allowed to
    /// complete but no new ticks start. The `grace_secs` window gives handlers time to
    /// finish before the function returns.
    ///
    /// The grace window is clamped to 60 seconds. Values above 60 have no additional effect.
    /// Note: the sleep is a best-effort delay, not a join on in-flight handlers — handlers
    /// that outlive the grace window are dropped, not awaited.
    ///
    /// The `grace_secs` parameter corresponds to `scheduler.daemon.shutdown_grace_secs`
    /// in config (default 30). Pass 0 for immediate exit after shutdown signal.
    pub async fn run_with_interval_and_grace(&mut self, tick_secs: u64, grace_secs: u64) {
        let secs = tick_secs.clamp(5, 3600);
        let mut interval = tokio::time::interval(Duration::from_secs(secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let _tick_span = tracing::info_span!(
                        "scheduler.daemon.tick",
                        tasks = self.tasks.len()
                    ).entered();
                    self.drain_channel().await;
                    self.tick().await;
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        tracing::info!("scheduler shutting down (grace {}s)", grace_secs);
                        if grace_secs > 0 {
                            let deadline = tokio::time::Instant::now()
                                + Duration::from_secs(grace_secs.min(60));
                            loop {
                                if self.in_flight.lock().await.is_empty() {
                                    tracing::debug!("scheduler: no in-flight tasks, exiting immediately");
                                    break;
                                }
                                if tokio::time::Instant::now() >= deadline {
                                    tracing::warn!("scheduler: grace period elapsed with tasks still in-flight");
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    /// Run the scheduler loop with a configurable tick interval.
    ///
    /// The interval is clamped to a minimum of 1 second. Missed ticks (caused by a
    /// slow `tick()` call) are skipped instead of burst-replayed, preventing runaway
    /// execution storms on slow hosts.
    ///
    /// This method runs until `true` is sent on the shutdown channel.
    pub async fn run_with_interval(&mut self, tick_secs: u64) {
        let secs = tick_secs.max(1);
        let mut interval = tokio::time::interval(Duration::from_secs(secs));
        // Skip missed ticks instead of bursting to catch up. Without this, a slow `tick()`
        // call causes tokio to fire the interval in a tight loop to "catch up", producing
        // hundreds of executions per second (#2737 leak 4).
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.drain_channel().await;
                    self.tick().await;
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        tracing::info!("scheduler shutting down");
                        break;
                    }
                }
            }
        }
    }

    /// Run the scheduler loop, checking for due tasks every 60 seconds.
    ///
    /// This is a convenience wrapper around [`Scheduler::run_with_interval`] with a
    /// 60-second tick. It runs until `true` is sent on the shutdown channel.
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(Duration::from_mins(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.drain_channel().await;
                    self.tick().await;
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        tracing::info!("scheduler shutting down");
                        break;
                    }
                }
            }
        }
    }

    async fn drain_channel(&mut self) {
        while let Ok(msg) = self.task_rx.try_recv() {
            match msg {
                SchedulerMessage::Add(boxed) => {
                    let desc = *boxed;
                    self.register_descriptor(desc).await;
                }
                SchedulerMessage::Cancel(name) => {
                    self.tasks.retain(|t| t.name != name);
                    if let Err(e) = self.store.delete_job(&name).await {
                        tracing::warn!(task = %name, "failed to delete job from store: {e}");
                    }
                }
            }
        }
    }

    async fn register_descriptor(&mut self, desc: TaskDescriptor) {
        // Check capacity only when adding a new task (upsert of existing name does not count).
        let is_new = !self.tasks.iter().any(|t| t.name == desc.name);
        if is_new && self.tasks.len() >= self.max_tasks {
            tracing::warn!(
                task = %desc.name,
                max_tasks = self.max_tasks,
                "max_tasks limit reached, dropping task"
            );
            return;
        }
        let now = Utc::now();
        match &desc.mode {
            TaskMode::Periodic { schedule } => {
                if let Err(e) = self
                    .store
                    .upsert_job_with_mode(
                        &desc.name,
                        &schedule.to_string(),
                        desc.kind.as_str(),
                        "periodic",
                        None,
                        "",
                    )
                    .await
                {
                    tracing::warn!(task = %desc.name, "failed to upsert job: {e}");
                    return;
                }
                if let Some(next) = schedule.after(&now).next() {
                    let _ = self
                        .store
                        .set_next_run(&desc.name, &next.to_rfc3339())
                        .await;
                }
            }
            TaskMode::OneShot { run_at } => {
                if let Err(e) = self
                    .store
                    .upsert_job_with_mode(
                        &desc.name,
                        "",
                        desc.kind.as_str(),
                        "oneshot",
                        Some(&run_at.to_rfc3339()),
                        "",
                    )
                    .await
                {
                    tracing::warn!(task = %desc.name, "failed to upsert oneshot job: {e}");
                    return;
                }
            }
        }
        // Remove old entry with same name if present.
        self.tasks.retain(|t| t.name != desc.name);
        self.tasks.push(ScheduledTask {
            name: desc.name,
            mode: desc.mode,
            kind: desc.kind,
            config: desc.config,
        });
    }

    #[allow(clippy::too_many_lines)]
    async fn tick(&mut self) {
        let now = Utc::now();
        let mut completed_oneshots: Vec<String> = Vec::new();

        for task in &self.tasks {
            let should_run = match &task.mode {
                TaskMode::Periodic { .. } => {
                    match self.store.get_next_run(&task.name).await {
                        Ok(Some(ref s)) => {
                            s.parse::<chrono::DateTime<Utc>>().is_ok_and(|dt| dt <= now)
                        }
                        // PERF-SC-04 fix: missing next_run must not mean "fire now".
                        // Compute and persist next occurrence, then skip this tick.
                        Ok(None) => {
                            if let Some(schedule) = task.cron_schedule()
                                && let Some(next) = schedule.after(&now).next()
                            {
                                let _ = self
                                    .store
                                    .set_next_run(&task.name, &next.to_rfc3339())
                                    .await;
                            }
                            false
                        }
                        Err(e) => {
                            tracing::warn!(task = %task.name, "failed to check next_run: {e}");
                            false
                        }
                    }
                }
                TaskMode::OneShot { run_at } => *run_at <= now,
            };

            if should_run {
                let is_periodic = matches!(&task.mode, TaskMode::Periodic { .. });

                // SIGNIFICANT-5: guard against concurrent executions of the same periodic task
                // (e.g. overlap between catch_up_missed and tick). Drop the guard before any
                // handler .await so the MutexGuard never crosses an await point.
                if is_periodic {
                    let mut guard = self.in_flight.lock().await;
                    if guard.contains(task.name.as_str()) {
                        tracing::debug!(task = %task.name, "tick: periodic task in-flight, skipping");
                        drop(guard);
                        continue;
                    }
                    guard.insert(task.name.clone());
                    drop(guard);
                }

                if let Some(handler) = self.handlers.get(task.kind.as_str()) {
                    tracing::info!(task = %task.name, kind = task.kind.as_str(), "executing task");
                    match handler.execute(&task.config).await {
                        Ok(()) => match &task.mode {
                            TaskMode::Periodic { schedule } => {
                                let next = schedule
                                    .after(&now)
                                    .next()
                                    .map(|dt| dt.to_rfc3339())
                                    .unwrap_or_default();
                                if let Err(e) = self
                                    .store
                                    .record_run(&task.name, &now.to_rfc3339(), &next)
                                    .await
                                {
                                    tracing::warn!(task = %task.name, "failed to record run: {e}");
                                }
                            }
                            TaskMode::OneShot { .. } => {
                                if let Err(e) = self.store.mark_done(&task.name).await {
                                    tracing::warn!(task = %task.name, "failed to mark done: {e}");
                                }
                                completed_oneshots.push(task.name.clone());
                            }
                        },
                        Err(e) => {
                            tracing::warn!(task = %task.name, "task execution failed: {e}");
                        }
                    }
                } else if let TaskMode::OneShot { .. } = &task.mode {
                    // Dual-path for custom oneshot tasks without a registered handler:
                    // when `CustomTaskHandler` is registered it handles the task via the
                    // handler interface above.  This branch is a fallback that injects the
                    // prompt directly into the agent loop through `custom_task_tx` for cases
                    // where no handler was registered (e.g. scheduler created without one).
                    if let (TaskKind::Custom(_), Some(tx)) = (&task.kind, &self.custom_task_tx) {
                        let raw =
                            task.config.get("task").and_then(|v| v.as_str()).unwrap_or(
                                "Execute the following scheduled task now: check status",
                            );
                        let prompt = sanitize_task_prompt(raw);
                        let _ = tx.try_send(prompt);
                        if let Err(e) = self.store.mark_done(&task.name).await {
                            tracing::warn!(task = %task.name, "failed to mark done: {e}");
                        }
                        completed_oneshots.push(task.name.clone());
                    } else {
                        tracing::debug!(
                            task = %task.name,
                            kind = task.kind.as_str(),
                            "no handler registered"
                        );
                    }
                } else {
                    tracing::debug!(task = %task.name, kind = task.kind.as_str(), "no handler registered");
                }

                // Release the in_flight slot after execution completes (success or error).
                if is_periodic {
                    self.in_flight.lock().await.remove(task.name.as_str());
                }
            }
        }

        // Remove completed one-shot tasks from memory.
        self.tasks.retain(|t| !completed_oneshots.contains(&t.name));
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use chrono::Duration;

    use super::*;
    use crate::task::TaskHandler;
    use zeph_db::DbPool;

    struct CountingHandler {
        count: Arc<AtomicU32>,
    }

    impl TaskHandler for CountingHandler {
        fn execute(
            &self,
            _config: &serde_json::Value,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), SchedulerError>> + Send + '_>>
        {
            let count = self.count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        }
    }

    async fn test_pool() -> DbPool {
        zeph_db::sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn scheduler_init_and_tick() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let task = ScheduledTask::new(
            "test",
            "* * * * * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        )
        .unwrap();
        scheduler.add_task(task);

        let count = Arc::new(AtomicU32::new(0));
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: count.clone(),
            }),
        );

        scheduler.init().await.unwrap();

        // Backdate next_run to simulate a due task.
        zeph_db::query(sql!(
            "UPDATE scheduled_jobs SET next_run = '2000-01-01T00:00:00+00:00' WHERE name = 'test'"
        ))
        .execute(&pool)
        .await
        .unwrap();

        scheduler.tick().await;
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    /// PERF-SC-04 regression: a task with no `next_run` must not fire.
    #[tokio::test]
    async fn tick_does_not_fire_without_next_run() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let task = ScheduledTask::new(
            "yearly",
            "0 0 1 1 * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        )
        .unwrap();
        scheduler.add_task(task);

        let count = Arc::new(AtomicU32::new(0));
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: count.clone(),
            }),
        );

        // Init the store but do NOT set next_run (simulate missing next_run).
        scheduler.store.init().await.unwrap();
        scheduler
            .store
            .upsert_job("yearly", "0 0 1 1 * *", "health_check")
            .await
            .unwrap();
        // Explicitly clear next_run to ensure it's NULL.
        zeph_db::query(sql!(
            "UPDATE scheduled_jobs SET next_run = NULL WHERE name = 'yearly'"
        ))
        .execute(&pool)
        .await
        .unwrap();

        scheduler.tick().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "task without next_run must not fire (PERF-SC-04)"
        );
    }

    /// After `init()`, every periodic task must have a non-null `next_run`.
    #[tokio::test]
    async fn init_always_sets_next_run() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let task = ScheduledTask::new(
            "periodic",
            "0 * * * * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        )
        .unwrap();
        scheduler.add_task(task);
        scheduler.init().await.unwrap();

        let next: Option<String> = zeph_db::query_scalar(sql!(
            "SELECT next_run FROM scheduled_jobs WHERE name = 'periodic'"
        ))
        .fetch_optional(&pool)
        .await
        .unwrap()
        .flatten();
        assert!(
            next.is_some(),
            "next_run must be set after init() for periodic task"
        );
    }

    /// A task whose `next_run` is in the future must not fire.
    #[tokio::test]
    async fn task_does_not_fire_before_next_run() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let task = ScheduledTask::new(
            "future",
            "0 0 1 1 * *", // once a year
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        )
        .unwrap();
        scheduler.add_task(task);

        let count = Arc::new(AtomicU32::new(0));
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: count.clone(),
            }),
        );

        scheduler.init().await.unwrap();

        // Manually set next_run to far future to prevent firing.
        let far_future = "2099-01-01T00:00:00+00:00";
        zeph_db::query(sql!(
            "UPDATE scheduled_jobs SET next_run = ? WHERE name = 'future'"
        ))
        .bind(far_future)
        .execute(&pool)
        .await
        .unwrap();

        scheduler.tick().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "should not fire before next_run"
        );
    }

    /// After a task fires, `next_run` is advanced to the following occurrence.
    #[tokio::test]
    async fn next_run_advances_after_execution() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let task = ScheduledTask::new(
            "adv",
            "0 * * * * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        )
        .unwrap();
        scheduler.add_task(task);
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: Arc::new(AtomicU32::new(0)),
            }),
        );

        scheduler.init().await.unwrap();

        // Backdate next_run to force execution.
        zeph_db::query(sql!(
            "UPDATE scheduled_jobs SET next_run = '2000-01-01T00:00:00+00:00' WHERE name = 'adv'"
        ))
        .execute(&pool)
        .await
        .unwrap();

        scheduler.tick().await;

        // next_run must now be in the future.
        let next: Option<String> = zeph_db::query_scalar(sql!(
            "SELECT next_run FROM scheduled_jobs WHERE name = 'adv'"
        ))
        .fetch_optional(&pool)
        .await
        .unwrap()
        .flatten();
        let next_str = next.expect("next_run should be set after execution");
        let next_dt = next_str
            .parse::<chrono::DateTime<Utc>>()
            .expect("should parse as RFC3339");
        // The backdated value was 2000-01-01; after tick() the scheduler must have
        // advanced next_run to a future occurrence (at least year 2001+).
        // We avoid comparing against Utc::now() here because on slow CI hosts
        // (e.g. Windows) a per-second cron can tick past the assertion window.
        let epoch_2001 = chrono::DateTime::parse_from_rfc3339("2001-01-01T00:00:00+00:00")
            .expect("static parse")
            .with_timezone(&Utc);
        assert!(
            next_dt > epoch_2001,
            "next_run must have advanced beyond the backdated value after firing"
        );
    }

    #[tokio::test]
    async fn scheduler_shutdown() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        let (tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);
        scheduler.init().await.unwrap();

        let handle = tokio::spawn(async move { scheduler.run().await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("scheduler should stop")
            .expect("task should complete");
    }

    /// One-shot task fires when `run_at` is in the past.
    #[tokio::test]
    async fn oneshot_fires_at_run_at() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let past = Utc::now() - Duration::hours(1);
        let task = ScheduledTask::oneshot(
            "os_fire",
            past,
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        );
        scheduler.add_task(task);

        let count = Arc::new(AtomicU32::new(0));
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: count.clone(),
            }),
        );
        scheduler.init().await.unwrap();
        scheduler.tick().await;

        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "oneshot must fire when run_at is past"
        );
    }

    /// One-shot task must NOT fire when `run_at` is in the future.
    #[tokio::test]
    async fn oneshot_does_not_fire_before_run_at() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let future = Utc::now() + Duration::hours(1);
        let task = ScheduledTask::oneshot(
            "os_future",
            future,
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        );
        scheduler.add_task(task);

        let count = Arc::new(AtomicU32::new(0));
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: count.clone(),
            }),
        );
        scheduler.init().await.unwrap();
        scheduler.tick().await;

        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "oneshot must not fire before run_at"
        );
    }

    /// After a one-shot fires, it is removed from self.tasks.
    #[tokio::test]
    async fn oneshot_removed_after_execution() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let past = Utc::now() - Duration::seconds(1);
        let task = ScheduledTask::oneshot(
            "os_rm",
            past,
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        );
        scheduler.add_task(task);
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: Arc::new(AtomicU32::new(0)),
            }),
        );
        scheduler.init().await.unwrap();
        assert_eq!(scheduler.tasks.len(), 1);
        scheduler.tick().await;
        assert_eq!(
            scheduler.tasks.len(),
            0,
            "completed oneshot must be removed from tasks"
        );
    }

    /// `init()` hydrates periodic jobs that were written to the store out-of-process
    /// (e.g. via the CLI) and are NOT present in `self.tasks` at construction time.
    ///
    /// Regression test for fix #3499: before the fix, CLI-added jobs were never fired
    /// because `init()` did not call `store.list_jobs_full()` to backfill `self.tasks`.
    #[tokio::test]
    async fn init_hydrates_cli_added_periodic_jobs_from_store() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());

        // Simulate CLI insertion: write a periodic job directly to the store
        // *before* the Scheduler is constructed — mimicking a CLI `schedule add` command
        // that writes to the DB while the daemon is not running.
        store.init().await.unwrap();
        store
            .upsert_job_with_mode(
                "cli-job",
                "0 * * * * *",
                "health_check",
                "periodic",
                None,
                "",
            )
            .await
            .unwrap();

        // Construct a fresh Scheduler with an empty task list (no add_task calls),
        // pointing at the same pool that already has the CLI-added job.
        let store2 = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store2, rx);

        // Before init() self.tasks is empty.
        assert_eq!(
            scheduler.tasks.len(),
            0,
            "tasks must be empty before init()"
        );

        scheduler.init().await.unwrap();

        // After init() the CLI-added periodic job must have been hydrated.
        assert_eq!(
            scheduler.tasks.len(),
            1,
            "init() must hydrate the CLI-added periodic job from the store"
        );
        assert_eq!(
            scheduler.tasks[0].name, "cli-job",
            "hydrated task name must match the DB row"
        );

        // next_run must have been computed and persisted.
        let next_run = store.get_next_run("cli-job").await.unwrap();
        assert!(
            next_run.is_some(),
            "init() must compute and persist next_run for the hydrated job"
        );
        let dt = next_run
            .unwrap()
            .parse::<chrono::DateTime<chrono::Utc>>()
            .expect("next_run must be a valid RFC3339 timestamp");
        assert!(
            dt > chrono::Utc::now(),
            "next_run must be in the future after hydration"
        );
    }

    /// `init()` does NOT re-add jobs that are already present in `self.tasks` — avoids
    /// duplicates when both `add_task()` and a DB record exist for the same name.
    #[tokio::test]
    async fn init_does_not_duplicate_static_tasks_already_in_tasks() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        // Register via add_task (static path).
        let task = ScheduledTask::new(
            "static-job",
            "0 * * * * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        )
        .unwrap();
        scheduler.add_task(task);

        // init() upserts the task into the store AND then calls list_jobs_full().
        // The job will be in both self.tasks AND the DB; hydration must skip it.
        scheduler.init().await.unwrap();

        assert_eq!(
            scheduler.tasks.len(),
            1,
            "init() must not duplicate a static task that is already in self.tasks"
        );
    }

    /// Task registered via channel fires on next tick.
    #[tokio::test]
    async fn channel_registration() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, msg_tx) = Scheduler::new(store, rx);

        let count = Arc::new(AtomicU32::new(0));
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: count.clone(),
            }),
        );
        scheduler.init().await.unwrap();

        // Register a task via channel with a past run_at.
        let past = Utc::now() - Duration::hours(1);
        let desc = TaskDescriptor {
            name: "chan_task".to_owned(),
            mode: TaskMode::OneShot { run_at: past },
            kind: TaskKind::HealthCheck,
            config: serde_json::Value::Null,
        };
        msg_tx
            .send(SchedulerMessage::Add(Box::new(desc)))
            .await
            .unwrap();

        // drain_channel + tick.
        scheduler.drain_channel().await;
        scheduler.tick().await;

        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "channel-registered task must fire"
        );
    }

    /// `tick()` must skip a periodic task that is already present in `in_flight` (SIGNIFICANT-5).
    ///
    /// Simulates the overlap scenario: a slow handler is still running (name in `in_flight`)
    /// when the next tick fires. The task must not execute a second time.
    #[tokio::test]
    async fn tick_skips_in_flight_periodic_task() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let task = ScheduledTask::new(
            "slow_task",
            "* * * * * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        )
        .unwrap();
        scheduler.add_task(task);

        let count = Arc::new(AtomicU32::new(0));
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: count.clone(),
            }),
        );
        scheduler.init().await.unwrap();

        // Backdate next_run to make the task due.
        zeph_db::query(sql!(
            "UPDATE scheduled_jobs SET next_run = '2000-01-01T00:00:00+00:00' WHERE name = 'slow_task'"
        ))
        .execute(&pool)
        .await
        .unwrap();

        // Pre-populate in_flight to simulate a concurrent execution already running.
        scheduler
            .in_flight
            .lock()
            .await
            .insert("slow_task".to_owned());

        scheduler.tick().await;

        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "in-flight periodic task must not fire again on tick"
        );

        // Clean up so in_flight is empty for any subsequent assertions.
        scheduler.in_flight.lock().await.remove("slow_task");
    }

    /// After `tick()` executes a periodic task, the task name is removed from `in_flight`.
    #[tokio::test]
    async fn tick_releases_in_flight_after_execution() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        let task = ScheduledTask::new(
            "release_task",
            "* * * * * *",
            TaskKind::HealthCheck,
            serde_json::Value::Null,
        )
        .unwrap();
        scheduler.add_task(task);
        scheduler.register_handler(
            &TaskKind::HealthCheck,
            Box::new(CountingHandler {
                count: Arc::new(AtomicU32::new(0)),
            }),
        );
        scheduler.init().await.unwrap();

        zeph_db::query(sql!(
            "UPDATE scheduled_jobs SET next_run = '2000-01-01T00:00:00+00:00' WHERE name = 'release_task'"
        ))
        .execute(&pool)
        .await
        .unwrap();

        scheduler.tick().await;

        assert!(
            !scheduler.in_flight.lock().await.contains("release_task"),
            "in_flight must be empty after tick() completes for a periodic task"
        );
    }

    /// `init()` marks a DB job with an invalid cron expression as `'error'` and emits error-level log.
    ///
    /// Covers issue #3810: an external tool writing a malformed cron directly to the `SQLite` table
    /// must not silently disappear — it must be surfaced via `zeph scheduler list`.
    #[tokio::test]
    async fn init_marks_error_for_invalid_cron_job() {
        let pool = test_pool().await;

        // Write a job with an invalid cron expression directly, bypassing the Rust API.
        let store_pre = JobStore::new(pool.clone());
        store_pre.init().await.unwrap();
        zeph_db::query(sql!(
            "INSERT INTO scheduled_jobs (name, cron_expr, kind, task_mode, status) \
             VALUES ('bad-cron', 'not-a-valid-cron', 'health_check', 'periodic', 'pending')"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let (mut scheduler, _msg_tx) = Scheduler::new(store, rx);

        scheduler.init().await.unwrap();

        // The invalid job must not have been hydrated into self.tasks.
        assert!(
            scheduler.tasks.iter().all(|t| t.name != "bad-cron"),
            "invalid cron job must not be added to self.tasks"
        );

        // The DB row must now carry status = 'error' so it is visible in the job list.
        let status: String = zeph_db::query_scalar(sql!(
            "SELECT status FROM scheduled_jobs WHERE name = 'bad-cron'"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            status, "error",
            "invalid cron job must be marked as error in the DB (issue #3810)"
        );
    }
}
