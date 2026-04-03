// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end orchestration integration tests (Phase 7, #1242).
//!
//! These tests exercise the full orchestration pipeline:
//!   plan → `DagScheduler` tick loop → `LlmAggregator`
//!
//! Run with:
//!   cargo nextest run --test `orchestration_integration`

mod orchestration_integration {
    use zeph_core::config::{OrchestrationConfig, ProviderName};
    use zeph_core::orchestration::{
        AgentRouter, Aggregator, DagScheduler, FailureStrategy, GraphStatus, LlmAggregator,
        SchedulerAction, TaskEvent, TaskGraph, TaskId, TaskNode, TaskOutcome, TaskStatus,
    };
    use zeph_core::subagent::{
        SkillFilter, SubAgentDef, SubAgentPermissions, SubagentHooks, ToolPolicy,
    };
    use zeph_llm::mock::MockProvider;

    // ── Helpers ────────────────────────────────────────────────────────────────

    fn make_agent(name: &str) -> SubAgentDef {
        SubAgentDef {
            name: name.to_string(),
            description: format!("{name} agent"),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: String::new(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        }
    }

    fn default_config() -> OrchestrationConfig {
        OrchestrationConfig {
            enabled: true,
            max_tasks: 20,
            max_parallel: 4,
            // task_timeout_secs = 0 uses the internal 600s fallback; fine for unit tests.
            task_timeout_secs: 0,
            default_failure_strategy: "abort".to_string(),
            default_max_retries: 3,
            planner_provider: ProviderName::default(),
            planner_max_tokens: 4096,
            dependency_context_budget: 16384,
            confirm_before_execute: false,
            aggregator_max_tokens: 1024,
            deferral_backoff_ms: 250,
            ..OrchestrationConfig::default()
        }
    }

    /// Router that always picks the first available agent.
    struct FirstRouter;
    impl AgentRouter for FirstRouter {
        fn route(&self, _task: &TaskNode, available: &[SubAgentDef]) -> Option<String> {
            available.first().map(|d| d.name.clone())
        }
    }

    /// Build a linear 2-task graph: task 0 → task 1.
    ///
    /// `TaskId(u32)` has a `pub(crate)` field, so we construct the dependency
    /// list via `serde_json` deserialization rather than direct construction.
    fn linear_2_task_graph(failure_strategy: Option<FailureStrategy>) -> TaskGraph {
        let mut graph = TaskGraph::new("test goal");
        let mut t0 = TaskNode::new(0, "task-0", "do first thing");
        let mut t1 = TaskNode::new(1, "task-1", "do second thing");
        t1.depends_on = serde_json::from_str("[0]").expect("valid TaskId JSON");
        if let Some(s) = failure_strategy {
            t0.failure_strategy = Some(s);
            t1.failure_strategy = Some(s);
        }
        graph.tasks = vec![t0, t1];
        graph
    }

    /// Drive the scheduler to completion by simulating agent execution.
    ///
    /// For each `Spawn` action, `outcome_fn` is called with the `task_id` to produce
    /// the `TaskOutcome`. Returns the terminal `GraphStatus` when `Done` is emitted.
    async fn drive_scheduler<F>(scheduler: &mut DagScheduler, mut outcome_fn: F) -> GraphStatus
    where
        F: FnMut(TaskId) -> TaskOutcome,
    {
        let tx = scheduler.event_sender();
        loop {
            let actions = scheduler.tick();
            let mut done_status = None;
            let mut spawned = Vec::new();

            for action in actions {
                match action {
                    SchedulerAction::Spawn {
                        task_id,
                        agent_def_name,
                        ..
                    } => {
                        let handle_id = format!("handle-{}", task_id.index());
                        scheduler.record_spawn(task_id, handle_id.clone(), agent_def_name);
                        spawned.push((task_id, handle_id));
                    }
                    SchedulerAction::Done { status } => {
                        done_status = Some(status);
                    }
                    SchedulerAction::Cancel { .. }
                    | SchedulerAction::RunInline { .. }
                    | SchedulerAction::Verify { .. } => {}
                }
            }

            for (task_id, handle_id) in spawned {
                let outcome = outcome_fn(task_id);
                tx.send(TaskEvent {
                    task_id,
                    agent_handle_id: handle_id,
                    outcome,
                })
                .await
                .expect("event channel open");
            }

            if let Some(status) = done_status {
                return status;
            }

            scheduler.wait_event().await;
        }
    }

    // ── Test 1 ─────────────────────────────────────────────────────────────────
    // Happy path: 2-task linear graph where both tasks complete successfully.
    // Verifies the full pipeline: scheduler drives to Completed, then LlmAggregator
    // synthesizes a coherent response from completed task outputs.

    #[tokio::test]
    async fn test_plan_execute_aggregate_happy_path() {
        let graph = linear_2_task_graph(None);
        let config = default_config();
        let agents = vec![make_agent("worker")];
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), agents).unwrap();

        let final_status = drive_scheduler(&mut scheduler, |task_id| TaskOutcome::Completed {
            output: format!("output from task {}", task_id.index()),
            artifacts: vec![],
        })
        .await;

        assert_eq!(final_status, GraphStatus::Completed);

        let graph = scheduler.into_graph();
        assert_eq!(graph.status, GraphStatus::Completed);
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);
        assert_eq!(graph.tasks[1].status, TaskStatus::Completed);

        // MockProvider.chat() does not retry, so a single response is sufficient (I1).
        let provider = MockProvider::with_responses(vec!["synthesis result".to_string()]);
        let aggregator = LlmAggregator::new(provider, &config);
        let (result, _usage) = aggregator.aggregate(&graph).await.unwrap();
        assert!(
            !result.is_empty(),
            "aggregator must produce non-empty output"
        );
        assert_eq!(result, "synthesis result");
    }

    // ── Test 2 ─────────────────────────────────────────────────────────────────
    // Single-task graph with no dependencies completes in one tick.

    #[tokio::test]
    async fn test_single_task_graph() {
        let mut graph = TaskGraph::new("single task goal");
        graph.tasks = vec![TaskNode::new(0, "solo-task", "do the thing")];

        let config = default_config();
        let agents = vec![make_agent("worker")];
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), agents).unwrap();

        let final_status = drive_scheduler(&mut scheduler, |_| TaskOutcome::Completed {
            output: "done".to_string(),
            artifacts: vec![],
        })
        .await;

        assert_eq!(final_status, GraphStatus::Completed);

        let graph = scheduler.into_graph();
        assert_eq!(graph.tasks.len(), 1);
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);
    }

    // ── Test 3 ─────────────────────────────────────────────────────────────────
    // Abort strategy: task 0 fails → graph immediately enters Failed state and
    // task 1 is never spawned (its dependency failed before it could be scheduled).

    #[tokio::test]
    async fn test_failure_abort_strategy() {
        let graph = linear_2_task_graph(Some(FailureStrategy::Abort));
        let config = default_config();
        let agents = vec![make_agent("worker")];
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), agents).unwrap();

        let final_status = drive_scheduler(&mut scheduler, |task_id| {
            if task_id.index() == 0 {
                TaskOutcome::Failed {
                    error: "task 0 failed".to_string(),
                }
            } else {
                // task 1 must never be spawned under Abort — this branch is unreachable.
                TaskOutcome::Completed {
                    output: "unreachable".to_string(),
                    artifacts: vec![],
                }
            }
        })
        .await;

        assert_eq!(final_status, GraphStatus::Failed);

        let graph = scheduler.into_graph();
        assert_eq!(graph.status, GraphStatus::Failed);
        assert_eq!(graph.tasks[0].status, TaskStatus::Failed);
        assert_ne!(
            graph.tasks[1].status,
            TaskStatus::Running,
            "task 1 must not be running after abort"
        );
        assert_ne!(
            graph.tasks[1].status,
            TaskStatus::Completed,
            "task 1 must not complete after abort"
        );
    }

    // ── Test 4 ─────────────────────────────────────────────────────────────────
    // Skip strategy on a 2-task linear graph (task 0 → task 1):
    //   propagate_failure(Skip) marks the failed task as Skipped and all transitive
    //   dependents as Skipped. graph.status is NOT set to Failed by the Skip branch.
    //   When tick() detects all tasks are terminal with graph still Running, it sets
    //   graph.status = Completed. The final status is therefore Completed, not Failed.

    #[tokio::test]
    async fn test_failure_skip_strategy() {
        let graph = linear_2_task_graph(Some(FailureStrategy::Skip));
        let config = default_config();
        let agents = vec![make_agent("worker")];
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), agents).unwrap();

        let final_status = drive_scheduler(&mut scheduler, |task_id| {
            if task_id.index() == 0 {
                TaskOutcome::Failed {
                    error: "task 0 failed with skip".to_string(),
                }
            } else {
                // task 1 is a dependent of task 0 and will be Skipped before it can be
                // spawned, so this branch is unreachable in normal execution.
                TaskOutcome::Completed {
                    output: "unreachable".to_string(),
                    artifacts: vec![],
                }
            }
        })
        .await;

        // Skip does not set graph.status = Failed; all tasks becoming terminal drives
        // the scheduler to emit Done(Completed).
        assert_eq!(
            final_status,
            GraphStatus::Completed,
            "Skip strategy: all tasks Skipped → graph reaches Completed, not Failed"
        );

        let graph = scheduler.into_graph();
        assert_eq!(graph.status, GraphStatus::Completed);
        // Both tasks must be Skipped: the failed task and its dependent.
        assert_eq!(
            graph.tasks[0].status,
            TaskStatus::Skipped,
            "task 0 (failed with Skip) must be Skipped"
        );
        assert_eq!(
            graph.tasks[1].status,
            TaskStatus::Skipped,
            "task 1 (dependent of skipped task 0) must be Skipped"
        );
    }

    // ── Test 5 ─────────────────────────────────────────────────────────────────
    // Retry exhausted: single task with max_retries=1 is failed twice.
    // After the second failure retry_count reaches max_retries and the Retry branch
    // falls through to Abort, setting graph.status = Failed.

    #[tokio::test]
    async fn test_retry_exhausted() {
        let mut graph = TaskGraph::new("retry test goal");
        let mut task = TaskNode::new(0, "retryable-task", "this task will fail");
        task.failure_strategy = Some(FailureStrategy::Retry);
        task.max_retries = Some(1); // allow 1 retry = 2 total attempts
        graph.tasks = vec![task];

        let config = default_config();
        let agents = vec![make_agent("worker")];
        let mut scheduler =
            DagScheduler::new(graph, &config, Box::new(FirstRouter), agents).unwrap();

        let tx = scheduler.event_sender();
        let mut attempt = 0u32;

        let final_status = loop {
            let actions = scheduler.tick();
            let mut done = None;
            let mut spawned = Vec::new();

            for action in actions {
                match action {
                    SchedulerAction::Spawn {
                        task_id,
                        agent_def_name,
                        ..
                    } => {
                        let handle_id = format!("handle-{attempt}");
                        attempt += 1;
                        scheduler.record_spawn(task_id, handle_id.clone(), agent_def_name);
                        spawned.push((task_id, handle_id));
                    }
                    SchedulerAction::Done { status } => {
                        done = Some(status);
                    }
                    SchedulerAction::Cancel { .. }
                    | SchedulerAction::RunInline { .. }
                    | SchedulerAction::Verify { .. } => {}
                }
            }

            for (task_id, handle_id) in spawned {
                tx.send(TaskEvent {
                    task_id,
                    agent_handle_id: handle_id,
                    outcome: TaskOutcome::Failed {
                        error: format!("attempt {attempt} failed"),
                    },
                })
                .await
                .expect("channel open");
            }

            if let Some(status) = done {
                break status;
            }

            scheduler.wait_event().await;
        };

        assert_eq!(
            final_status,
            GraphStatus::Failed,
            "graph must be Failed after retries exhausted"
        );
        assert_eq!(scheduler.into_graph().status, GraphStatus::Failed);
        assert!(
            attempt >= 2,
            "task must be attempted at least twice (initial + 1 retry), got {attempt}"
        );
    }
}
