// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::time::Duration;
#[allow(unused_imports)]
use zeph_db::sql;

use chrono::Utc;
use tokio::sync::{mpsc, watch};

use crate::error::SchedulerError;
use crate::sanitize::sanitize_task_prompt;
use crate::store::JobStore;
use crate::task::{ScheduledTask, TaskDescriptor, TaskHandler, TaskKind, TaskMode};

/// Message type for runtime scheduler control.
pub enum SchedulerMessage {
    Add(Box<TaskDescriptor>),
    Cancel(String),
}

pub struct Scheduler {
    tasks: Vec<ScheduledTask>,
    store: JobStore,
    handlers: HashMap<String, Box<dyn TaskHandler>>,
    shutdown_rx: watch::Receiver<bool>,
    task_rx: mpsc::Receiver<SchedulerMessage>,
    /// Optional sender for injecting custom task prompts into the agent loop.
    custom_task_tx: Option<mpsc::Sender<String>>,
    max_tasks: usize,
}

impl Scheduler {
    #[must_use]
    pub fn new(
        store: JobStore,
        shutdown_rx: watch::Receiver<bool>,
    ) -> (Self, mpsc::Sender<SchedulerMessage>) {
        Self::with_max_tasks(store, shutdown_rx, 100)
    }

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
        };
        (scheduler, tx)
    }

    /// Attach a sender for injecting custom task prompts into the agent loop.
    #[must_use]
    pub fn with_custom_task_sender(mut self, tx: mpsc::Sender<String>) -> Self {
        self.custom_task_tx = Some(tx);
        self
    }

    pub fn add_task(&mut self, task: ScheduledTask) {
        self.tasks.push(task);
    }

    pub fn register_handler(&mut self, kind: &TaskKind, handler: Box<dyn TaskHandler>) {
        self.handlers.insert(kind.as_str().to_owned(), handler);
    }

    /// Initialize the store, sync task definitions, and compute initial `next_run` for each task.
    ///
    /// # Errors
    ///
    /// Returns an error if DB init, upsert, or `next_run` persistence fails.
    pub async fn init(&self) -> Result<(), SchedulerError> {
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
        Ok(())
    }

    /// Run the scheduler loop with configurable tick interval (minimum 1 second).
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

    /// Run the scheduler loop, checking every 60 seconds for due tasks.
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
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
}
