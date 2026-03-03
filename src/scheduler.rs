// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "scheduler")]
use std::sync::Arc;

#[cfg(feature = "scheduler")]
use tokio::sync::{mpsc, watch};
#[cfg(feature = "scheduler")]
use zeph_core::config::Config;
#[cfg(feature = "scheduler")]
use zeph_scheduler::{
    CustomTaskHandler, JobStore, ScheduledTask, Scheduler, SchedulerMessage, TaskDescriptor,
    TaskHandler, TaskKind, TaskMode, UpdateCheckHandler,
};

#[cfg(feature = "scheduler")]
use crate::scheduler_executor::SchedulerExecutor;

/// Enqueue config-declared tasks onto the scheduler channel, skipping invalid entries.
#[cfg(feature = "scheduler")]
fn load_config_tasks(
    tasks: &[zeph_core::config::ScheduledTaskConfig],
    tx: &mpsc::Sender<SchedulerMessage>,
) {
    use std::str::FromStr;

    use zeph_core::config::ScheduledTaskKind;

    for task in tasks {
        match (&task.cron, &task.run_at) {
            (Some(_), Some(_)) => {
                tracing::warn!(
                    "scheduler: task '{}' has both cron and run_at set, skipping",
                    task.name
                );
                continue;
            }
            (None, None) => {
                tracing::warn!(
                    "scheduler: task '{}' has neither cron nor run_at set, skipping",
                    task.name
                );
                continue;
            }
            _ => {}
        }

        let kind = match &task.kind {
            ScheduledTaskKind::MemoryCleanup => TaskKind::MemoryCleanup,
            ScheduledTaskKind::SkillRefresh => TaskKind::SkillRefresh,
            ScheduledTaskKind::HealthCheck => TaskKind::HealthCheck,
            ScheduledTaskKind::UpdateCheck => TaskKind::UpdateCheck,
            ScheduledTaskKind::Custom(s) => TaskKind::Custom(s.clone()),
        };

        let mode = if let Some(cron_expr) = &task.cron {
            match cron::Schedule::from_str(cron_expr) {
                Ok(s) => TaskMode::Periodic {
                    schedule: Box::new(s),
                },
                Err(e) => {
                    tracing::warn!(
                        "scheduler: task '{}' invalid cron '{}': {e}, skipping",
                        task.name,
                        cron_expr
                    );
                    continue;
                }
            }
        } else if let Some(run_at_str) = &task.run_at {
            if let Ok(dt) = run_at_str.parse::<chrono::DateTime<chrono::Utc>>() {
                TaskMode::OneShot { run_at: dt }
            } else {
                tracing::warn!(
                    "scheduler: task '{}' invalid run_at '{}', skipping",
                    task.name,
                    run_at_str
                );
                continue;
            }
        } else {
            continue;
        };

        let desc = TaskDescriptor {
            name: task.name.clone(),
            mode,
            kind,
            config: task.config.clone(),
        };
        if tx.try_send(SchedulerMessage::Add(Box::new(desc))).is_err() {
            tracing::warn!(
                "scheduler: channel full, dropping config task '{}'",
                task.name
            );
        }
    }
}

#[cfg(feature = "scheduler")]
pub(crate) async fn bootstrap_scheduler<C>(
    agent: zeph_core::agent::Agent<C>,
    config: &Config,
    shutdown_rx: watch::Receiver<bool>,
) -> (zeph_core::agent::Agent<C>, Option<SchedulerExecutor>)
where
    C: zeph_core::channel::Channel,
{
    if !config.scheduler.enabled {
        if config.agent.auto_update_check {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let handler = UpdateCheckHandler::new(env!("CARGO_PKG_VERSION"), tx);
            tokio::spawn(async move {
                let _ = handler.execute(&serde_json::Value::Null).await;
            });
            return (agent.with_update_notifications(rx), None);
        }
        return (agent, None);
    }

    let store = match JobStore::open(&config.memory.sqlite_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("scheduler: failed to open store: {e}");
            return (agent, None);
        }
    };

    let store_arc = Arc::new(store);
    let scheduler_store = match JobStore::open(&config.memory.sqlite_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("scheduler: failed to open second store handle: {e}");
            return (agent, None);
        }
    };

    let (scheduler, task_tx) =
        Scheduler::with_max_tasks(scheduler_store, shutdown_rx, config.scheduler.max_tasks);
    let (custom_tx, custom_rx) = mpsc::channel::<String>(16);
    let mut scheduler = scheduler.with_custom_task_sender(custom_tx.clone());

    load_config_tasks(&config.scheduler.tasks, &task_tx);

    if config.agent.auto_update_check {
        let (update_tx, update_rx) = tokio::sync::mpsc::channel(4);
        let update_task = match ScheduledTask::new(
            "update_check",
            "0 0 9 * * *",
            TaskKind::UpdateCheck,
            serde_json::Value::Null,
        ) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("scheduler: invalid update_check cron: {e}");
                return (agent, None);
            }
        };
        scheduler.add_task(update_task);
        scheduler.register_handler(
            &TaskKind::UpdateCheck,
            Box::new(UpdateCheckHandler::new(
                env!("CARGO_PKG_VERSION"),
                update_tx.clone(),
            )),
        );
        scheduler.register_handler(
            &TaskKind::Custom("custom".to_owned()),
            Box::new(CustomTaskHandler::new(custom_tx)),
        );

        if let Err(e) = scheduler.init().await {
            tracing::warn!("scheduler init failed: {e}");
            return (agent, None);
        }

        let tick_secs = config.scheduler.tick_interval_secs;
        tokio::spawn(async move { scheduler.run_with_interval(tick_secs).await });
        tracing::info!("scheduler started");

        let executor = SchedulerExecutor::new(task_tx, store_arc);
        return (
            agent
                .with_update_notifications(update_rx)
                .with_custom_task_rx(custom_rx),
            Some(executor),
        );
    }

    scheduler.register_handler(
        &TaskKind::Custom("custom".to_owned()),
        Box::new(CustomTaskHandler::new(custom_tx)),
    );

    if let Err(e) = scheduler.init().await {
        tracing::warn!("scheduler init failed: {e}");
        return (agent, None);
    }

    let tick_secs = config.scheduler.tick_interval_secs;
    tokio::spawn(async move { scheduler.run_with_interval(tick_secs).await });
    tracing::info!("scheduler started");

    let executor = SchedulerExecutor::new(task_tx, store_arc);
    (agent.with_custom_task_rx(custom_rx), Some(executor))
}

#[cfg(all(test, feature = "scheduler"))]
mod tests {
    use tokio::sync::mpsc;
    use zeph_core::config::{ScheduledTaskConfig, ScheduledTaskKind};

    use super::load_config_tasks;

    #[tokio::test]
    async fn bootstrap_returns_executor() {
        use tokio::sync::watch;
        use zeph_core::LoopbackChannel;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_skills::registry::SkillRegistry;
        use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
        use zeph_tools::registry::ToolDef;

        use super::bootstrap_scheduler;

        struct StubExec;
        impl ToolExecutor for StubExec {
            async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
            fn tool_definitions(&self) -> Vec<ToolDef> {
                vec![]
            }
            async fn execute_tool_call(
                &self,
                _: &ToolCall,
            ) -> Result<Option<ToolOutput>, ToolError> {
                Ok(None)
            }
        }

        let (channel, _handle) = LoopbackChannel::pair(16);
        let provider = AnyProvider::Mock(MockProvider::with_responses(vec![]));
        let registry = SkillRegistry::default();
        let agent = zeph_core::agent::Agent::new(provider, channel, registry, None, 5, StubExec);

        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut config = zeph_core::config::Config::default();
        config.scheduler.enabled = true;
        config.memory.sqlite_path = ":memory:".into();

        let (_agent, executor_opt): (_, Option<crate::scheduler_executor::SchedulerExecutor>) =
            bootstrap_scheduler(agent, &config, shutdown_rx).await;
        assert!(
            executor_opt.is_some(),
            "expected Some(SchedulerExecutor) when scheduler is enabled"
        );
    }

    #[test]
    fn load_config_tasks_skips_both_cron_and_run_at() {
        let (tx, mut rx) = mpsc::channel(8);
        let tasks = vec![ScheduledTaskConfig {
            name: "bad".into(),
            cron: Some("0 * * * * *".into()),
            run_at: Some("2099-01-01T00:00:00Z".into()),
            kind: ScheduledTaskKind::Custom("test".into()),
            config: serde_json::Value::Null,
        }];
        load_config_tasks(&tasks, &tx);
        assert!(
            rx.try_recv().is_err(),
            "task with both cron and run_at must be skipped"
        );
    }

    #[test]
    fn load_config_tasks_skips_neither_cron_nor_run_at() {
        let (tx, mut rx) = mpsc::channel(8);
        let tasks = vec![ScheduledTaskConfig {
            name: "empty".into(),
            cron: None,
            run_at: None,
            kind: ScheduledTaskKind::Custom("test".into()),
            config: serde_json::Value::Null,
        }];
        load_config_tasks(&tasks, &tx);
        assert!(
            rx.try_recv().is_err(),
            "task with neither cron nor run_at must be skipped"
        );
    }

    #[test]
    fn load_config_tasks_enqueues_valid_periodic_task() {
        let (tx, mut rx) = mpsc::channel(8);
        let tasks = vec![ScheduledTaskConfig {
            name: "periodic".into(),
            cron: Some("0 * * * * *".into()),
            run_at: None,
            kind: ScheduledTaskKind::Custom("test".into()),
            config: serde_json::Value::Null,
        }];
        load_config_tasks(&tasks, &tx);
        assert!(
            rx.try_recv().is_ok(),
            "valid periodic task must be enqueued"
        );
    }
}
