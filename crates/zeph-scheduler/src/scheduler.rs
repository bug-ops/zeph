use std::collections::HashMap;
use std::time::Duration;

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

    /// Initialize the store and sync task definitions.
    ///
    /// # Errors
    ///
    /// Returns an error if DB init or upsert fails.
    pub async fn init(&self) -> Result<(), SchedulerError> {
        self.store.init().await?;
        for task in &self.tasks {
            self.store
                .upsert_job(&task.name, &task.schedule.to_string(), task.kind.as_str())
                .await?;
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
        let now_utc = chrono_now_utc();
        for task in &self.tasks {
            let should_run = match self.store.last_run(&task.name).await {
                Ok(last_run) => is_task_due(&task.schedule, last_run.as_deref()),
                Err(e) => {
                    tracing::warn!(task = %task.name, "failed to check last_run: {e}");
                    false
                }
            };

            if should_run {
                if let Some(handler) = self.handlers.get(task.kind.as_str()) {
                    tracing::info!(task = %task.name, kind = task.kind.as_str(), "executing task");
                    match handler.execute(&task.config).await {
                        Ok(()) => {
                            if let Err(e) = self.store.record_run(&task.name, &now_utc).await {
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

/// Check if a task is due by finding the first cron occurrence after `last_run`
/// and verifying it is <= `now`.
fn is_task_due(schedule: &cron::Schedule, last_run: Option<&str>) -> bool {
    let now_chrono = chrono::Utc::now();
    let after = match last_run {
        Some(s) => match s.parse::<chrono::DateTime<chrono::Utc>>() {
            Ok(dt) => dt,
            Err(_) => return true,
        },
        None => return true,
    };
    // First scheduled time after the last run
    schedule.after(&after).take(1).any(|dt| dt <= now_chrono)
}

fn chrono_now_utc() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let month_days: [u64; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if days < md {
            month = i as u64 + 1;
            break;
        }
        days -= md;
    }
    (year, month, days + 1)
}

fn is_leap(y: u64) -> bool {
    y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400))
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
        let store = JobStore::new(pool);
        let (_tx, rx) = watch::channel(false);
        let mut scheduler = Scheduler::new(store, rx);

        let task = ScheduledTask::new(
            "test",
            "0 * * * * *",
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
        scheduler.tick().await;
        assert_eq!(count.load(Ordering::Relaxed), 1);
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

    #[test]
    fn chrono_now_format() {
        let ts = chrono_now_utc();
        assert!(ts.ends_with('Z'));
        assert!(ts.contains('T'));
        assert_eq!(ts.len(), 20);
    }
}
