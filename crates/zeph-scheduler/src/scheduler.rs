// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::watch;

use crate::error::SchedulerError;
use crate::store::JobStore;
use crate::task::{ScheduledTask, TaskHandler, TaskKind};

pub struct Scheduler {
    tasks: Vec<ScheduledTask>,
    store: JobStore,
    handlers: HashMap<String, Box<dyn TaskHandler>>,
    shutdown_rx: watch::Receiver<bool>,
}

impl Scheduler {
    #[must_use]
    pub fn new(store: JobStore, shutdown_rx: watch::Receiver<bool>) -> Self {
        Self {
            tasks: Vec::new(),
            store,
            handlers: HashMap::new(),
            shutdown_rx,
        }
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
            self.store
                .upsert_job(&task.name, &task.schedule.to_string(), task.kind.as_str())
                .await?;
            // Only set next_run if not already persisted (preserves across restarts).
            if self.store.get_next_run(&task.name).await?.is_none()
                && let Some(next) = task.schedule.after(&now).next()
            {
                self.store
                    .set_next_run(&task.name, &next.to_rfc3339())
                    .await?;
            }
        }
        Ok(())
    }

    /// Run the scheduler loop, checking every 60 seconds for due tasks.
    pub async fn run(&mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = interval.tick() => {
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

    async fn tick(&self) {
        let now = Utc::now();
        for task in &self.tasks {
            let should_run = match self.store.get_next_run(&task.name).await {
                Ok(Some(ref s)) => s.parse::<chrono::DateTime<Utc>>().is_ok_and(|dt| dt <= now),
                Ok(None) => true,
                Err(e) => {
                    tracing::warn!(task = %task.name, "failed to check next_run: {e}");
                    false
                }
            };

            if should_run {
                if let Some(handler) = self.handlers.get(task.kind.as_str()) {
                    tracing::info!(task = %task.name, kind = task.kind.as_str(), "executing task");
                    match handler.execute(&task.config).await {
                        Ok(()) => {
                            let next = task
                                .schedule
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
                        Err(e) => {
                            tracing::warn!(task = %task.name, "task execution failed: {e}");
                        }
                    }
                } else {
                    tracing::debug!(task = %task.name, kind = task.kind.as_str(), "no handler registered");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::task::TaskHandler;
    use sqlx::SqlitePool;

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

    async fn test_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn scheduler_init_and_tick() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let mut scheduler = Scheduler::new(store, rx);

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
        sqlx::query(
            "UPDATE scheduled_jobs SET next_run = '2000-01-01T00:00:00+00:00' WHERE name = 'test'",
        )
        .execute(&pool)
        .await
        .unwrap();

        scheduler.tick().await;
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    /// A task whose next_run is in the future must not fire.
    #[tokio::test]
    async fn task_does_not_fire_before_next_run() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let mut scheduler = Scheduler::new(store, rx);

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
        sqlx::query("UPDATE scheduled_jobs SET next_run = ? WHERE name = 'future'")
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

    /// After a task fires, next_run is advanced to the following occurrence.
    #[tokio::test]
    async fn next_run_advances_after_execution() {
        let pool = test_pool().await;
        let store = JobStore::new(pool.clone());
        let (_tx, rx) = watch::channel(false);
        let mut scheduler = Scheduler::new(store, rx);

        let task = ScheduledTask::new(
            "adv",
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
        scheduler.tick().await; // fires once (no next_run yet)

        // next_run must now be in the future.
        let next: Option<String> =
            sqlx::query_scalar("SELECT next_run FROM scheduled_jobs WHERE name = 'adv'")
                .fetch_optional(&pool)
                .await
                .unwrap()
                .flatten();
        let next_str = next.expect("next_run should be set after execution");
        let next_dt = next_str
            .parse::<chrono::DateTime<Utc>>()
            .expect("should parse as RFC3339");
        assert!(
            next_dt > Utc::now(),
            "next_run must be in the future after firing"
        );
    }

    #[tokio::test]
    async fn scheduler_shutdown() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        let (tx, rx) = watch::channel(false);
        let mut scheduler = Scheduler::new(store, rx);
        scheduler.init().await.unwrap();

        let handle = tokio::spawn(async move { scheduler.run().await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx.send(true);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("scheduler should stop")
            .expect("task should complete");
    }
}
