// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "scheduler")]
use tokio::sync::watch;
#[cfg(feature = "scheduler")]
use zeph_core::config::Config;
#[cfg(feature = "scheduler")]
use zeph_scheduler::{
    JobStore, ScheduledTask, Scheduler, TaskHandler, TaskKind, UpdateCheckHandler,
};

#[cfg(feature = "scheduler")]
pub(crate) async fn bootstrap_scheduler<C>(
    agent: zeph_core::agent::Agent<C>,
    config: &Config,
    shutdown_rx: watch::Receiver<bool>,
) -> zeph_core::agent::Agent<C>
where
    C: zeph_core::channel::Channel,
{
    if !config.scheduler.enabled {
        if config.agent.auto_update_check {
            // Fire-and-forget single check at startup when scheduler is disabled.
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let handler = UpdateCheckHandler::new(env!("CARGO_PKG_VERSION"), tx);
            tokio::spawn(async move {
                let _ = handler.execute(&serde_json::Value::Null).await;
            });
            return agent.with_update_notifications(rx);
        }
        return agent;
    }

    let store = match JobStore::open(&config.memory.sqlite_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("scheduler: failed to open store: {e}");
            return agent;
        }
    };

    let mut scheduler = Scheduler::new(store, shutdown_rx);

    let agent = if config.agent.auto_update_check {
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
                return agent;
            }
        };
        scheduler.add_task(update_task);
        scheduler.register_handler(
            &TaskKind::UpdateCheck,
            Box::new(UpdateCheckHandler::new(
                env!("CARGO_PKG_VERSION"),
                update_tx,
            )),
        );
        agent.with_update_notifications(update_rx)
    } else {
        agent
    };

    if let Err(e) = scheduler.init().await {
        tracing::warn!("scheduler init failed: {e}");
        return agent;
    }

    tokio::spawn(async move { scheduler.run().await });
    tracing::info!("scheduler started");

    agent
}
