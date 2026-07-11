// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(feature = "scheduler")]
use std::sync::Arc;

#[cfg(feature = "scheduler")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "scheduler")]
use tokio::sync::{mpsc, watch};
#[cfg(feature = "scheduler")]
use zeph_core::config::Config;
#[cfg(feature = "scheduler")]
use zeph_experiments::{
    BenchmarkSet, ConfigSnapshot, Evaluator, ExperimentEngine, ExperimentSource, GridStep,
    SearchSpace,
};
#[cfg(feature = "scheduler")]
use zeph_llm::any::AnyProvider;
#[cfg(feature = "scheduler")]
use zeph_memory::five_signal::FiveSignalRuntime;
#[cfg(feature = "scheduler")]
use zeph_memory::semantic::SemanticMemory;
#[cfg(feature = "scheduler")]
use zeph_scheduler::{
    CustomTaskHandler, JobStore, ScheduledTask, Scheduler, SchedulerMessage, TaskDescriptor,
    TaskHandler, TaskKind, TaskMode, UpdateCheckHandler, normalize_cron_expr,
};

#[cfg(feature = "scheduler")]
use crate::scheduler_executor::SchedulerExecutor;

/// Handler for scheduled experiment sessions.
///
/// Spawns the experiment engine on a separate Tokio task (S1 fix — does not block tick loop).
/// Holds a clone of the scheduler's `shutdown_rx` so it can wire shutdown to the engine (S2 fix).
/// Uses an [`AtomicBool`] guard to prevent overlapping runs.
#[cfg(feature = "scheduler")]
struct ExperimentTaskHandler {
    config: zeph_core::config::ExperimentConfig,
    /// Subject provider under test.
    provider: Arc<AnyProvider>,
    /// Judge provider used to score the subject's outputs. Resolved from
    /// `[experiments] eval_provider` (falling back to the subject provider when unset) so the
    /// judge is independent from the subject, matching the interactive `/experiment` command
    /// and `--experiment-run` CLI flag — see #5947.
    eval_provider: Arc<AnyProvider>,
    memory: Option<Arc<SemanticMemory>>,
    /// Clone of the scheduler's shutdown watch receiver for shutdown propagation.
    shutdown_rx: watch::Receiver<bool>,
    /// Prevents overlapping experiment runs.
    running: Arc<AtomicBool>,
}

#[cfg(feature = "scheduler")]
impl TaskHandler for ExperimentTaskHandler {
    fn execute(
        &self,
        _config: &serde_json::Value,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), zeph_scheduler::SchedulerError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            // Guard: skip if a previous run is still in flight.
            if self
                .running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                tracing::info!("experiment task: previous run still in progress, skipping");
                return Ok(());
            }

            let mut run_config = self.config.clone();
            run_config.max_experiments = self.config.schedule.max_experiments_per_run;
            // Override wall-time with schedule-specific limit so long interactive sessions
            // cannot block the next cron trigger.
            run_config.max_wall_time_secs = self.config.schedule.max_wall_time_secs;

            let provider = Arc::clone(&self.provider);
            let eval_provider = Arc::clone(&self.eval_provider);
            let memory = self.memory.clone();
            let running = Arc::clone(&self.running);
            let mut shutdown_watcher = self.shutdown_rx.clone();

            tokio::spawn(async move {
                // EXEMPT(#5143): per-experiment one-shot task; managed by `running` AtomicBool flag
                let benchmark_file = run_config.benchmark_file.clone();

                // Load benchmark via spawn_blocking: from_file uses blocking std::fs I/O.
                let benchmark = if let Some(path) = benchmark_file {
                    match tokio::task::spawn_blocking(move || BenchmarkSet::from_file(&path)).await
                    {
                        Ok(Ok(b)) => b,
                        Ok(Err(e)) => {
                            tracing::warn!("experiment task: benchmark load failed: {e}");
                            running.store(false, Ordering::Release);
                            return;
                        }
                        Err(e) => {
                            tracing::warn!("experiment task: spawn_blocking panicked: {e}");
                            running.store(false, Ordering::Release);
                            return;
                        }
                    }
                } else {
                    tracing::warn!("experiment task: no benchmark_file configured, skipping run");
                    running.store(false, Ordering::Release);
                    return;
                };

                let evaluator =
                    match Evaluator::new(eval_provider, benchmark, run_config.eval_budget_tokens) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::warn!("experiment task: evaluator init failed: {e}");
                            running.store(false, Ordering::Release);
                            return;
                        }
                    };

                // GridStep with default search space is used for scheduled runs.
                // Configurable search space is a planned follow-up (not in this phase).
                let generator = Box::new(GridStep::new(SearchSpace::default()));
                let baseline = ConfigSnapshot::default();

                let mut engine = ExperimentEngine::new(
                    evaluator, generator, provider, baseline, run_config, memory,
                )
                .with_source(ExperimentSource::Scheduled);

                // Wire shutdown via select!: cancels engine token when shutdown signal arrives,
                // and self-cleans when engine completes (no leaked watcher task).
                let engine_token = engine.cancel_token();
                tokio::select! {
                    biased;
                    () = async {
                        loop {
                            if shutdown_watcher.changed().await.is_err() {
                                break;
                            }
                            if *shutdown_watcher.borrow() {
                                engine_token.cancel();
                                break;
                            }
                        }
                    } => {
                        tracing::info!("experiment task: shutdown received");
                    }
                    result = engine.run() => {
                        match result {
                            Ok(report) => {
                                tracing::info!(
                                    session = %report.session_id,
                                    experiments = report.results.len(),
                                    accepted = report.results.iter().filter(|r| r.accepted).count(),
                                    improvement = report.total_improvement,
                                    wall_time_ms = report.wall_time_ms,
                                    "scheduled experiment session completed"
                                );
                            }
                            Err(e) => {
                                tracing::warn!("experiment task: engine failed: {e}");
                            }
                        }
                    }
                }

                running.store(false, Ordering::Release);
            });

            Ok(())
        })
    }
}

/// Enqueue config-declared tasks onto the scheduler channel, skipping invalid entries.
#[cfg(feature = "scheduler")]
pub(crate) fn load_config_tasks(
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
            ScheduledTaskKind::Experiment => TaskKind::Experiment,
            ScheduledTaskKind::Custom(s) => TaskKind::Custom(s.clone()),
            _ => continue,
        };

        let mode = if let Some(cron_expr) = &task.cron {
            let normalized = normalize_cron_expr(cron_expr);
            match cron::Schedule::from_str(&normalized) {
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
            provenance: zeph_scheduler::TaskProvenance::Static,
        };
        if tx.try_send(SchedulerMessage::Add(Box::new(desc))).is_err() {
            tracing::warn!(
                "scheduler: channel full, dropping config task '{}'",
                task.name
            );
        }
    }
}

/// Dependencies needed to register the scheduled experiment task: the subject provider under
/// test, the judge (eval) provider used to score it, and optional memory for persisting results.
#[cfg(feature = "scheduler")]
pub(crate) type ExperimentDeps = (
    Arc<AnyProvider>,
    Arc<AnyProvider>,
    Option<Arc<SemanticMemory>>,
);

/// Scheduler init result: executor plus optional channel receivers for agent wiring.
#[cfg(feature = "scheduler")]
pub(crate) struct SchedulerInitResult {
    pub(crate) executor: SchedulerExecutor,
    /// Present when `auto_update_check` is enabled.
    pub(crate) update_rx: Option<mpsc::Receiver<String>>,
    pub(crate) custom_rx: mpsc::Receiver<String>,
}

/// Initialize the scheduler: open stores, build executor, spawn tick loop.
///
/// Does NOT touch any `Agent` — returns raw channel receivers so callers can apply them to
/// whichever `Agent<C>` they control. Returns `None` when the scheduler is disabled.
#[cfg(feature = "scheduler")]
#[allow(clippy::too_many_lines)]
pub(crate) async fn init_scheduler(
    config: &Config,
    shutdown_rx: watch::Receiver<bool>,
    experiment_deps: Option<ExperimentDeps>,
    five_signal: Option<Arc<FiveSignalRuntime>>,
    supervisor: Option<&zeph_common::TaskSupervisor>,
) -> Option<SchedulerInitResult> {
    if !config.scheduler.enabled {
        return None;
    }

    let db_url = crate::db_url::resolve_db_url(config);
    let store = match JobStore::open(db_url).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("scheduler: failed to open store: {e}");
            return None;
        }
    };

    let store_arc = Arc::new(store);
    let scheduler_store = match JobStore::open(db_url).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("scheduler: failed to open second store handle: {e}");
            return None;
        }
    };

    // Clone before moving into Scheduler so the experiment handler can watch shutdown (S2 fix).
    let shutdown_rx_for_experiments = shutdown_rx.clone();

    let (scheduler, task_tx) =
        Scheduler::with_max_tasks(scheduler_store, shutdown_rx, config.scheduler.max_tasks);
    let (custom_tx, custom_rx) = mpsc::channel::<String>(16);
    let mut scheduler = scheduler
        .with_custom_task_sender(custom_tx.clone())
        .with_handler_timeout(std::time::Duration::from_secs(
            config.scheduler.daemon.handler_timeout_secs,
        ))
        .with_reentry_defense(
            config.scheduler.security.enabled,
            config.scheduler.security.injection_pattern_check,
            config.scheduler.security.attenuate_after_external_read,
        );

    load_config_tasks(&config.scheduler.tasks, &task_tx);

    // Register experiment handler when both features are enabled and schedule is configured.
    if config.experiments.enabled
        && config.experiments.schedule.enabled
        && let Some((exp_provider, exp_eval_provider, exp_memory)) = experiment_deps
    {
        let handler = ExperimentTaskHandler {
            config: config.experiments.clone(),
            provider: exp_provider,
            eval_provider: exp_eval_provider,
            memory: exp_memory,
            shutdown_rx: shutdown_rx_for_experiments,
            running: Arc::new(AtomicBool::new(false)),
        };
        match ScheduledTask::new(
            "auto-experiment",
            &config.experiments.schedule.cron,
            TaskKind::Experiment,
            serde_json::Value::Null,
        ) {
            Ok(task) => {
                scheduler.add_task(task);
                scheduler.register_handler(&TaskKind::Experiment, Box::new(handler));
                tracing::info!(
                    cron = %config.experiments.schedule.cron,
                    max_per_run = config.experiments.schedule.max_experiments_per_run,
                    "experiment scheduler task registered"
                );
            }
            Err(e) => tracing::warn!("scheduler: invalid experiment cron: {e}"),
        }
    }

    let update_rx = if config.agent.auto_update_check {
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
                return None;
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
        Some(update_rx)
    } else {
        None
    };

    scheduler.register_handler(
        &TaskKind::Custom("custom".to_owned()),
        Box::new(CustomTaskHandler::new(custom_tx)),
    );

    if let Some(five_signal_runtime) = five_signal {
        let consolidation_cfg = config.memory.five_signal.consolidation_daemon.clone();
        let interval_secs = consolidation_cfg.interval_seconds;
        let cron_expr = if interval_secs < 60 {
            format!("*/{} * * * * *", interval_secs.max(1))
        } else if interval_secs < 3600 {
            format!("0 */{} * * * *", interval_secs / 60)
        } else {
            format!("0 0 */{} * * *", (interval_secs / 3600).max(1))
        };
        let handler = zeph_memory::five_signal::consolidation::ConsolidationHandler::new(
            five_signal_runtime,
            consolidation_cfg,
        );
        match ScheduledTask::new(
            "five_signal_consolidation",
            &cron_expr,
            TaskKind::Custom("five_signal_consolidation".into()),
            serde_json::Value::Null,
        ) {
            Ok(task) => {
                scheduler.add_task(task);
                scheduler.register_handler(
                    &TaskKind::Custom("five_signal_consolidation".into()),
                    Box::new(handler),
                );
                tracing::info!(
                    cron = %cron_expr,
                    interval_secs,
                    "five_signal consolidation daemon registered"
                );
            }
            Err(e) => tracing::warn!("scheduler: invalid five_signal consolidation cron: {e}"),
        }
    }

    if let Err(e) = scheduler.init().await {
        tracing::warn!("scheduler init failed: {e}");
        return None;
    }

    let tick_secs = config.scheduler.tick_interval_secs;
    if let Some(sup) = supervisor {
        let fut = async move { scheduler.run_with_interval(tick_secs).await };
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
        sup.spawn(zeph_common::TaskDescriptor {
            name: "scheduler_loop",
            restart: zeph_common::RestartPolicy::Restart {
                max: 0,
                base_delay: std::time::Duration::from_secs(1),
            },
            factory: move || {
                let f = cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    } else {
        tokio::spawn(async move { scheduler.run_with_interval(tick_secs).await }); // EXEMPT(#5143): no-supervisor fallback for scheduler_loop
    }
    tracing::info!("scheduler started");

    let executor = SchedulerExecutor::new(task_tx, store_arc);
    Some(SchedulerInitResult {
        executor,
        update_rx,
        custom_rx,
    })
}

#[cfg(feature = "scheduler")]
#[allow(clippy::too_many_lines)]
pub(crate) async fn bootstrap_scheduler<C>(
    agent: zeph_core::agent::Agent<C>,
    config: &Config,
    shutdown_rx: watch::Receiver<bool>,
    experiment_deps: Option<ExperimentDeps>,
    five_signal: Option<Arc<FiveSignalRuntime>>,
    supervisor: Option<&zeph_common::TaskSupervisor>,
) -> (zeph_core::agent::Agent<C>, Option<SchedulerExecutor>)
where
    C: zeph_core::channel::Channel,
{
    if !config.scheduler.enabled {
        if config.agent.auto_update_check {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let handler = UpdateCheckHandler::new(env!("CARGO_PKG_VERSION"), tx);
            if let Some(sup) = supervisor {
                let fut = async move {
                    let _ = handler.execute(&serde_json::Value::Null).await;
                };
                let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
                sup.spawn(zeph_common::TaskDescriptor {
                    name: "update_check",
                    restart: zeph_common::RestartPolicy::RunOnce,
                    factory: move || {
                        let f = cell.lock().take();
                        async move {
                            if let Some(f) = f {
                                f.await;
                            }
                        }
                    },
                });
            } else {
                tokio::spawn(async move {
                    // EXEMPT(#5143): no-supervisor fallback for update_check
                    let _ = handler.execute(&serde_json::Value::Null).await;
                });
            }
            return (agent.with_update_notifications(rx), None);
        }
        return (agent, None);
    }

    let Some(result) = init_scheduler(
        config,
        shutdown_rx,
        experiment_deps,
        five_signal,
        supervisor,
    )
    .await
    else {
        return (agent, None);
    };

    let agent = if let Some(rx) = result.update_rx {
        agent
            .with_update_notifications(rx)
            .with_custom_task_rx(result.custom_rx)
    } else {
        agent.with_custom_task_rx(result.custom_rx)
    };

    (agent, Some(result.executor))
}

#[cfg(all(test, feature = "scheduler"))]
mod tests {
    use tokio::sync::mpsc;
    use zeph_core::config::{ScheduledTaskConfig, ScheduledTaskKind};
    use zeph_scheduler::TaskKind;

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
            zeph_tools::tool_executor_no_inner_defaults!();
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
            Box::pin(bootstrap_scheduler(
                agent,
                &config,
                shutdown_rx,
                None,
                None,
                None,
            ))
            .await;
        assert!(
            executor_opt.is_some(),
            "expected Some(SchedulerExecutor) when scheduler is enabled"
        );
    }

    /// Regression test for #5947: the judge (`eval_provider`) and subject (`provider`) must be
    /// distinct provider instances that both actually get invoked during a scheduled experiment
    /// run. Pre-fix, `ExperimentTaskHandler` only held one `provider` field used for both roles,
    /// so a second, distinct eval provider would never see any calls — exactly what this test
    /// guards against by asserting both mock recorders captured traffic.
    #[tokio::test]
    async fn experiment_task_handler_uses_distinct_eval_provider() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use tokio::sync::watch;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_scheduler::TaskHandler;

        use super::ExperimentTaskHandler;

        // Minimal 1-case benchmark so `execute()` doesn't bail out early on a missing file.
        let benchmark_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            benchmark_file.path(),
            "[[cases]]\nprompt = \"What is 6x7?\"\nreference = \"42\"\n",
        )
        .unwrap();

        let (subject_provider, subject_recorder) =
            MockProvider::with_responses(vec!["42".into()]).with_recording();
        let (judge_provider, judge_recorder) =
            MockProvider::with_responses(vec![r#"{"score": 8.0, "reason": "correct"}"#.into()])
                .with_recording();

        let mut config = zeph_core::config::ExperimentConfig {
            benchmark_file: Some(benchmark_file.path().to_path_buf()),
            ..Default::default()
        };
        config.schedule.max_experiments_per_run = 1;

        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let handler = ExperimentTaskHandler {
            config,
            provider: Arc::new(AnyProvider::Mock(subject_provider)),
            eval_provider: Arc::new(AnyProvider::Mock(judge_provider)),
            memory: None,
            shutdown_rx,
            running: Arc::new(AtomicBool::new(false)),
        };

        handler
            .execute(&serde_json::Value::Null)
            .await
            .expect("execute() itself only spawns the run; it must not error synchronously");

        // The actual run happens in a background tokio::spawn; poll for completion.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while handler.running.load(Ordering::Acquire) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "experiment run did not complete within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert!(
            !subject_recorder.lock().unwrap().is_empty(),
            "subject provider must have been called for generation"
        );
        assert!(
            !judge_recorder.lock().unwrap().is_empty(),
            "eval_provider (judge) must have been called for scoring — if this is empty, \
             the judge and subject collapsed back to the same provider (the #5947 regression)"
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

    #[test]
    fn load_config_tasks_maps_experiment_kind() {
        use zeph_scheduler::SchedulerMessage;

        let (tx, mut rx) = mpsc::channel(8);
        let tasks = vec![ScheduledTaskConfig {
            name: "exp".into(),
            cron: Some("0 * * * * *".into()),
            run_at: None,
            kind: ScheduledTaskKind::Experiment,
            config: serde_json::Value::Null,
        }];
        load_config_tasks(&tasks, &tx);
        let msg = rx.try_recv().expect("experiment task must be enqueued");
        let SchedulerMessage::Add(desc) = msg else {
            panic!("expected Add message");
        };
        assert_eq!(
            desc.kind,
            TaskKind::Experiment,
            "kind must map to Experiment"
        );
    }
}
