// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unit tests for the orchestration scheduler tick loop.

use std::time::Duration;

use super::*;
use crate::scheduler::tests::*;
use crate::verifier::ToolCallSummary;

#[test]
fn test_tick_produces_spawn_for_ready() {
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut scheduler = make_scheduler(graph);
    let actions = scheduler.tick();
    let spawns: Vec<_> = actions
        .iter()
        .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
        .collect();
    assert_eq!(spawns.len(), 2);
}

#[test]
fn test_tick_dispatches_all_regardless_of_max_parallel() {
    // tick() enforces max_parallel as a pre-dispatch cap.
    // With 5 independent tasks and max_parallel=2, only 2 are dispatched per tick.
    let graph = graph_from_nodes(vec![
        make_node(0, &[]),
        make_node(1, &[]),
        make_node(2, &[]),
        make_node(3, &[]),
        make_node(4, &[]),
    ]);
    let mut config = make_config();
    config.max_parallel = 2;
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();
    let actions = scheduler.tick();
    let spawn_count = actions
        .iter()
        .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
        .count();
    assert_eq!(
        spawn_count, 2,
        "max_parallel=2 caps dispatched tasks per tick"
    );
}

#[test]
fn test_tick_detects_completion() {
    let mut graph = graph_from_nodes(vec![make_node(0, &[])]);
    graph.tasks[0].status = TaskStatus::Completed;
    let config = make_config();
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();
    // Manually set graph to Running since new() validated Created status
    // — but all tasks are terminal. tick() should detect completion.
    let actions = scheduler.tick();
    let has_done = actions.iter().any(|a| {
        matches!(
            a,
            SchedulerAction::Done {
                status: GraphStatus::Completed
            }
        )
    });
    assert!(
        has_done,
        "should emit Done(Completed) when all tasks are terminal"
    );
}

#[test]
fn test_completion_event_marks_deps_ready() {
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
    let mut scheduler = make_scheduler(graph);

    // Simulate task 0 running.
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "handle-0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let event = TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "handle-0".to_string(),
        outcome: TaskOutcome::Completed {
            output: "done".to_string(),
            artifacts: vec![],
            tool_trace: None,
        },
    };
    scheduler.buffered_events.push_back(event);

    let actions = scheduler.tick();
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Completed);
    // Task 1 should now be Ready or Spawn action emitted.
    let has_spawn_1 = actions
        .iter()
        .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(1)));
    assert!(
        has_spawn_1 || scheduler.graph.tasks[1].status == TaskStatus::Ready,
        "task 1 should be spawned or marked Ready"
    );
}

#[test]
fn test_handoff_event_marks_source_completed_and_activates_target() {
    // spec-080 (#6363) integration coverage: dag.rs's try_handoff tests exercise the pure
    // routing function directly, and scheduler_loop.rs's determine_task_outcome tests
    // exercise the zeph-core produce-side seam directly, but neither drives a real
    // TaskOutcome::Handoff through the tick()/process_event loop that connects them
    // (handle_handoff_outcome). This closes that gap: verifies the emitting node becomes
    // terminal Completed, the goto target activates with commanded_from set, the per-graph
    // handoff budget increments, and the target is dispatched/marked Ready in the same tick
    // (mirrors test_completion_event_marks_deps_ready above for the ordinary Completed path).
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "handle-0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    scheduler.buffered_events.push_back(TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "handle-0".to_string(),
        outcome: TaskOutcome::Handoff {
            output: "handing off".to_string(),
            goto: TaskRef::ById(TaskId(1)),
            tool_trace: None,
        },
    });

    let actions = scheduler.tick();

    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Completed);
    assert_eq!(
        scheduler.graph.tasks[0]
            .result
            .as_ref()
            .map(|r| r.output.as_str()),
        Some("handing off"),
        "the source node's own output must be preserved on the Handoff outcome"
    );
    assert_eq!(scheduler.graph.tasks[1].commanded_from, Some(TaskId(0)));
    assert_eq!(scheduler.graph.handoff_count, 1);
    assert!(
        scheduler.graph.tasks[0].handoff_rejected.is_none(),
        "a successful handoff must not set the rejection signal"
    );
    let has_spawn_1 = actions
        .iter()
        .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(1)));
    assert!(
        has_spawn_1 || scheduler.graph.tasks[1].status == TaskStatus::Ready,
        "handoff target should be spawned or marked Ready in the same tick"
    );
}

#[test]
fn test_handoff_event_rejection_leaves_source_completed_not_failed() {
    // spec-080 FR-B-006: a try_handoff rejection (forward-only violation, unsatisfied
    // deps, live route_to reservation, exhausted budget) must not escalate the emitting
    // node's own outcome to Failed -- it stays Completed with its real output preserved,
    // only the extra routing fails to activate. Exercised here via the full tick loop
    // (not dag::try_handoff in isolation) so the actual event-handling call site is
    // proven to honor this, not just the pure function.
    let mut graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    graph.tasks[1].status = TaskStatus::Completed; // forward-only rejection target
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "handle-0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    scheduler.buffered_events.push_back(TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "handle-0".to_string(),
        outcome: TaskOutcome::Handoff {
            output: "handing off".to_string(),
            goto: TaskRef::ById(TaskId(1)),
            tool_trace: None,
        },
    });

    let actions = scheduler.tick();

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Completed,
        "a rejected handoff must not escalate the source node to Failed"
    );
    assert_eq!(
        scheduler.graph.handoff_count, 0,
        "a rejected handoff must not consume the budget"
    );
    assert!(
        scheduler.graph.tasks[0].handoff_rejected.is_some(),
        "critic finding C1: a rejected handoff must be recorded as a graph-visible, \
         persisted signal, not just a log line"
    );
    // Tester Gap 2 (2026-07-17): the CheckToolOutcome/Verify action-emission block runs
    // unconditionally after the try_handoff match (both Ok and Err arms fall through to
    // the same trailing code, tick/mod.rs) -- prove that on the actual returned actions,
    // not just by code inspection, so a rejected handoff still gets the same completeness
    // treatment as an accepted one.
    assert!(
        actions.iter().any(|a| matches!(
            a,
            SchedulerAction::CheckToolOutcome { task_id, .. } if *task_id == TaskId(0)
        )),
        "CheckToolOutcome must still be emitted for a rejected handoff's own task_id: \
         {actions:?}"
    );
}

#[test]
fn test_handoff_event_emits_check_tool_outcome_action() {
    // #6394: a Handoff outcome must get the same deterministic tool-outcome check an
    // ordinary Completed outcome gets — unconditionally, not gated on verify_completeness.
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "handle-0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    scheduler.buffered_events.push_back(TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "handle-0".to_string(),
        outcome: TaskOutcome::Handoff {
            output: "handing off".to_string(),
            goto: TaskRef::ById(TaskId(1)),
            tool_trace: None,
        },
    });

    let actions = scheduler.tick();

    let has_check = actions.iter().any(|a| {
        matches!(
            a,
            SchedulerAction::CheckToolOutcome { task_id, .. } if *task_id == TaskId(0)
        )
    });
    assert!(
        has_check,
        "a Handoff outcome must emit CheckToolOutcome for its own task_id (#6394)"
    );
    // Tester Gap 1 (2026-07-17): `make_scheduler`/`make_config` default
    // `verify_completeness` to `false` — prove the flag actually suppresses `Verify` for
    // the Handoff outcome too, not just that the enabled case (separately tested below)
    // emits it.
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Verify { .. })),
        "Verify must NOT be emitted when verify_completeness is disabled (#6394): {actions:?}"
    );
}

#[test]
fn test_handoff_event_emits_verify_action_when_verify_completeness_enabled() {
    // #6394: with verify_completeness enabled, a Handoff outcome must also emit Verify,
    // mirroring handle_completed_outcome, carrying the handoff node's own output.
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut config = make_config();
    config.verify_completeness = true;
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "handle-0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    scheduler.buffered_events.push_back(TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "handle-0".to_string(),
        outcome: TaskOutcome::Handoff {
            output: "handing off".to_string(),
            goto: TaskRef::ById(TaskId(1)),
            tool_trace: None,
        },
    });

    let actions = scheduler.tick();

    let verify_output = actions.iter().find_map(|a| match a {
        SchedulerAction::Verify {
            task_id, output, ..
        } if *task_id == TaskId(0) => Some(output.clone()),
        _ => None,
    });
    assert_eq!(
        verify_output.as_deref(),
        Some("handing off"),
        "Verify must be emitted for the handoff node and carry its own output (#6394)"
    );
}

#[test]
fn test_handoff_event_all_tools_failed_marks_task_failed_not_handoff() {
    // #6394/#6380/#6397 parity: a Command-handoff node whose synchronously-known
    // (RunInline) tool trace shows every call failed or was policy-blocked must not be
    // allowed to route anywhere — the "I'm done, go to X" claim is bogus, so it is routed
    // to handle_failed_outcome before any Handoff side effect (cascade record,
    // try_handoff activation) runs, mirroring handle_completed_outcome's early branch.
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "handle-0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    scheduler.buffered_events.push_back(TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "handle-0".to_string(),
        outcome: TaskOutcome::Handoff {
            output: "claiming completion".to_string(),
            goto: TaskRef::ById(TaskId(1)),
            tool_trace: Some(vec![ToolCallSummary {
                tool: "write".to_string(),
                args_summary: None,
                ok: false,
                is_read_only: false,
            }]),
        },
    });

    scheduler.tick();

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Failed,
        "a Handoff outcome with an all-failed tool trace must be Failed, not Completed"
    );
    assert!(
        scheduler.graph.tasks[1].commanded_from.is_none(),
        "try_handoff must never run when the handoff node itself is corrected to Failed"
    );
    assert_eq!(
        scheduler.graph.handoff_count, 0,
        "no handoff budget must be consumed when the outcome is corrected to Failed"
    );
    assert!(
        scheduler.graph.tasks[0].handoff_rejected.is_none(),
        "handoff_rejected is a try_handoff-rejection signal, not used for the \
         all-tool-calls-failed short-circuit"
    );
}

#[cfg(feature = "llm-planning")]
#[test]
fn test_plan_with_verify_criteria_and_predicate_disabled_reaches_completed() {
    // End-to-end regression for #5403: a planner response where a task has a
    // verify_criteria acceptance check and a downstream dependent, converted with
    // verify_predicate_enabled = false (the reported bug's default config), must
    // dispatch the dependent once the parent completes and drive the graph to
    // GraphStatus::Completed instead of a false scheduler deadlock.
    use crate::graph::PlanSlug;
    use crate::planner::{PlannedTask, PlannerResponse, convert_response_pub};

    let response = PlannerResponse {
        tasks: vec![
            PlannedTask {
                task_id: PlanSlug::from("parent"),
                title: "Parent".to_string(),
                description: "do parent work".to_string(),
                agent_hint: None,
                depends_on: vec![],
                failure_strategy: None,
                execution_mode: None,
                verify_criteria: Some("output must be valid JSON".to_string()),
                tool_allowlist: None,
            },
            PlannedTask {
                task_id: PlanSlug::from("child"),
                title: "Child".to_string(),
                description: "do child work".to_string(),
                agent_hint: None,
                depends_on: vec![PlanSlug::from("parent")],
                failure_strategy: None,
                execution_mode: None,
                verify_criteria: None,
                tool_allowlist: None,
            },
        ],
    };
    let graph = convert_response_pub(response, "goal", &[make_def("worker")], 20, false).unwrap();
    assert!(
        graph.tasks[0].verify_predicate.is_none(),
        "verify_predicate must be dropped when verify_predicate_enabled is false"
    );

    let mut scheduler = make_scheduler(graph);

    // Tick 1: parent has no dependencies, should be dispatched.
    let actions = scheduler.tick();
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(0)))
    );
    scheduler.record_spawn(
        TaskId(0),
        "handle-parent".to_string(),
        "worker".to_string(),
        None,
    );
    scheduler.buffered_events.push_back(TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "handle-parent".to_string(),
        outcome: TaskOutcome::Completed {
            output: "parent done".to_string(),
            artifacts: vec![],
            tool_trace: None,
        },
    });

    // Tick 2: parent completion processed; child must be dispatched, not blocked by a
    // dangling verify_predicate gate.
    let actions = scheduler.tick();
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Completed);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(1))),
        "child must be dispatched once its only dependency completes"
    );
    scheduler.record_spawn(
        TaskId(1),
        "handle-child".to_string(),
        "worker".to_string(),
        None,
    );
    scheduler.buffered_events.push_back(TaskEvent {
        task_id: TaskId(1),
        agent_handle_id: "handle-child".to_string(),
        outcome: TaskOutcome::Completed {
            output: "child done".to_string(),
            artifacts: vec![],
            tool_trace: None,
        },
    });

    // Tick 3: graph must reach Completed, not a false deadlock/Failed.
    let actions = scheduler.tick();
    assert!(
        actions.iter().any(|a| matches!(
            a,
            SchedulerAction::Done {
                status: GraphStatus::Completed
            }
        )),
        "graph should complete successfully, not deadlock"
    );
    assert_eq!(scheduler.graph.status, GraphStatus::Completed);
}

#[test]
fn test_failure_abort_cancels_running() {
    let graph = graph_from_nodes(vec![
        make_node(0, &[]),
        make_node(1, &[]),
        make_node(2, &[0, 1]),
    ]);
    let mut scheduler = make_scheduler(graph);

    // Simulate tasks 0 and 1 running.
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );
    scheduler.graph.tasks[1].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(1),
        RunningTask {
            agent_handle_id: "h1".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    // Task 0 fails with default Abort strategy.
    let event = TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "h0".to_string(),
        outcome: TaskOutcome::Failed {
            error: "boom".to_string(),
        },
    };
    scheduler.buffered_events.push_back(event);

    let actions = scheduler.tick();
    assert_eq!(scheduler.graph.status, GraphStatus::Failed);
    let cancel_ids: Vec<_> = actions
        .iter()
        .filter_map(|a| {
            if let SchedulerAction::Cancel { agent_handle_id } = a {
                Some(agent_handle_id.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(cancel_ids.contains(&"h1"), "task 1 should be canceled");
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Done { .. }))
    );
}

#[test]
fn test_failure_skip_propagates() {
    use crate::graph::FailureStrategy;

    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
    let mut scheduler = make_scheduler(graph);

    // Set failure strategy to Skip on task 0.
    scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Skip);
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let event = TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "h0".to_string(),
        outcome: TaskOutcome::Failed {
            error: "skip me".to_string(),
        },
    };
    scheduler.buffered_events.push_back(event);
    scheduler.tick();

    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Skipped);
    assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Skipped);
}

#[test]
fn test_failure_retry_reschedules() {
    use crate::graph::FailureStrategy;

    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
    scheduler.graph.tasks[0].max_retries = Some(3);
    scheduler.graph.tasks[0].retry_count = 0;
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let event = TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "h0".to_string(),
        outcome: TaskOutcome::Failed {
            error: "transient".to_string(),
        },
    };
    scheduler.buffered_events.push_back(event);
    let actions = scheduler.tick();

    // Task should be rescheduled (Ready) and a Spawn action emitted.
    let has_spawn = actions
        .iter()
        .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(0)));
    assert!(
        has_spawn || scheduler.graph.tasks[0].status == TaskStatus::Ready,
        "retry should produce spawn or Ready status"
    );
    // retry_count incremented
    assert_eq!(scheduler.graph.tasks[0].retry_count, 1);
}

#[test]
fn test_process_event_failed_retry() {
    use crate::graph::FailureStrategy;

    // End-to-end: send Failed event, verify retry path produces Ready -> Spawn.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
    scheduler.graph.tasks[0].max_retries = Some(2);
    scheduler.graph.tasks[0].retry_count = 0;
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let event = TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "h0".to_string(),
        outcome: TaskOutcome::Failed {
            error: "first failure".to_string(),
        },
    };
    scheduler.buffered_events.push_back(event);
    let actions = scheduler.tick();

    // After retry: retry_count = 1, status = Ready or Spawn emitted.
    assert_eq!(scheduler.graph.tasks[0].retry_count, 1);
    let spawned = actions
        .iter()
        .any(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(0)));
    assert!(
        spawned || scheduler.graph.tasks[0].status == TaskStatus::Ready,
        "retry should emit Spawn or set Ready"
    );
    // Graph must still be Running.
    assert_eq!(scheduler.graph.status, GraphStatus::Running);
}

/// spec-075 §6 success criterion: "a node with `recovery` configured whose failure also trips
/// a cascade-abort threshold ends the graph `Failed`, not recovered" — the cascade check in
/// `handle_failed_outcome()` runs *before* `propagate_failure()` (where `try_recover` lives),
/// so cascade-abort must structurally preempt recovery. Uses the linear-chain cascade path
/// (`cascade_chain_threshold`), which needs no `CascadeDetector` setup: A(0) -> B(1) -> C(2),
/// A and B fail with `Retry` (not exhausted, so their own failures don't independently abort
/// the graph), C fails with `recovery` configured. C's failure is the 3rd consecutive Failed
/// entry in the chain, tripping the default `cascade_chain_threshold = 3` — the graph must
/// abort via `abort_dag_with_lineage()` before `try_recover()` for C is ever reached.
#[test]
fn test_cascade_chain_threshold_preempts_recovery() {
    let graph = graph_from_nodes(vec![
        make_node(0, &[]),
        make_node(1, &[0]),
        make_node(2, &[1]),
    ]);
    let mut config = make_config();
    config.cascade_chain_threshold = 3;
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    // A and B: Retry with retries available, so their own failures reset them to Ready
    // rather than independently aborting the graph (which would confound the test — we want
    // the cascade *chain* check to be the thing that aborts, not an ordinary per-task Abort).
    scheduler.graph.tasks[0].failure_strategy = Some(crate::graph::FailureStrategy::Retry);
    scheduler.graph.tasks[0].max_retries = Some(5);
    scheduler.graph.tasks[1].failure_strategy = Some(crate::graph::FailureStrategy::Retry);
    scheduler.graph.tasks[1].max_retries = Some(5);
    // C: recovery configured. If recovery fired, this would end Completed with the injected
    // output — the assertion below proves it never gets the chance to.
    scheduler.graph.tasks[2].recovery = Some(crate::graph::RecoveryAction {
        state_injection: Some("should never be applied".to_string()),
        route_to: None,
    });

    for (id, handle) in [(TaskId(0), "h0"), (TaskId(1), "h1"), (TaskId(2), "h2")] {
        scheduler.graph.tasks[id.index()].status = TaskStatus::Running;
        scheduler.running.insert(
            id,
            RunningTask {
                agent_handle_id: handle.to_string(),
                agent_def_name: "worker".to_string(),
                started_at: std::time::Instant::now(),
                admission_permit: None,
                last_progress_at: None,
            },
        );
    }

    // Failures processed in dependency order within a single tick() — each handle_failed_outcome
    // call records its lineage entry into self.lineage_chains before the next event is drained.
    for (id, handle) in [(TaskId(0), "h0"), (TaskId(1), "h1"), (TaskId(2), "h2")] {
        scheduler.buffered_events.push_back(TaskEvent {
            task_id: id,
            agent_handle_id: handle.to_string(),
            outcome: TaskOutcome::Failed {
                error: "boom".to_string(),
            },
        });
    }
    scheduler.tick();

    assert_eq!(
        scheduler.graph.status,
        GraphStatus::Failed,
        "cascade chain threshold must abort the graph"
    );
    assert_eq!(
        scheduler.graph.tasks[2].status,
        TaskStatus::Failed,
        "the recovery-configured node must NOT be recovered — cascade-abort preempts \
         propagate_failure() (and thus try_recover()) entirely"
    );
    assert_eq!(
        scheduler.graph.tasks[2]
            .result
            .as_ref()
            .map(|r| r.output.as_str()),
        Some("boom"),
        "result must hold the plain failure error, not a Mode-1 recovery substitution \
         (agent_id/agent_def would also be set to the recovery marker if try_recover had run)"
    );
    assert_eq!(
        scheduler.graph.tasks[2]
            .result
            .as_ref()
            .and_then(|r| r.agent_def.as_deref()),
        None,
        "no synthetic recovery TaskResult (with its recovery marker agent_def) should ever \
         have been set"
    );
}

#[test]
fn test_timeout_cancels_stalled() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 1; // 1 second timeout
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    // Simulate a running task that started just over 1 second ago.
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap(), // already timed out
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let actions = scheduler.tick();
    let has_cancel = actions.iter().any(
        |a| matches!(a, SchedulerAction::Cancel { agent_handle_id } if agent_handle_id == "h0"),
    );
    assert!(has_cancel, "timed-out task should emit Cancel action");
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
}

#[test]
fn test_per_task_timeout_override_fires_before_global_default() {
    // Two running tasks: task 0 has a short per-task override (already exceeded),
    // task 1 has no override and relies on the (much longer) global default.
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 300; // long global default
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    // Use Skip (not the graph default Abort) for the overridden task so a genuine
    // timeout on task 0 doesn't cascade-abort and collaterally cancel unrelated task 1
    // — isolates the per-task deadline filter this test targets from cascade semantics.
    scheduler.graph.tasks[0].failure_strategy = Some(zeph_config::FailureStrategy::Skip);
    scheduler.graph.tasks[0].timeout = Some(crate::graph::TimeoutPolicy {
        run_timeout_secs: Some(1),
        idle_timeout_secs: None,
    });
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.graph.tasks[1].status = TaskStatus::Running;

    let started_2s_ago = std::time::Instant::now()
        .checked_sub(Duration::from_secs(2))
        .unwrap();
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: started_2s_ago,
            admission_permit: None,
            last_progress_at: None,
        },
    );
    scheduler.running.insert(
        TaskId(1),
        RunningTask {
            agent_handle_id: "h1".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: started_2s_ago,
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let actions = scheduler.tick();
    let canceled: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            SchedulerAction::Cancel { agent_handle_id } => Some(agent_handle_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        canceled,
        vec!["h0"],
        "only the overridden task should time out; the other respects the longer global default"
    );
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Skipped);
    assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Running);
}

#[test]
fn test_no_overrides_timing_matches_pre_feature_behavior() {
    // Regression: no per-task overrides anywhere → identical timing to pre-feature code.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 1;
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let actions = scheduler.tick();
    let has_cancel = actions.iter().any(
        |a| matches!(a, SchedulerAction::Cancel { agent_handle_id } if agent_handle_id == "h0"),
    );
    assert!(
        has_cancel,
        "global timeout must still fire with no override"
    );
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
}

// --- idle_timeout_secs enforcement (issue #6245, Alt-A progress-signal plumbing) ---

/// A heartbeat recording the current instant, as if the sub-agent loop just wrote to it.
///
/// To simulate a *stale* heartbeat, record one and then let real time pass
/// (`std::thread::sleep`) before checking — subtracting an offset from `monotonic_millis()`
/// to fake staleness would underflow (`saturating_sub` clamps to 0) whenever the process,
/// and therefore its `PROCESS_START` origin, is young — which it always is at the start of
/// an isolated test binary.
fn progress_handle_now() -> std::sync::Arc<std::sync::atomic::AtomicU64> {
    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
        zeph_common::monotonic_millis(),
    ))
}

/// spec-075 §6 (FR-005, activated by #6245): a task with a short `idle_timeout_secs` and a
/// stale progress heartbeat (no turn boundary reached within the window) must be killed
/// with `TimeoutCause::Idle`, distinguishable via the synthetic `TaskResult.output` (F5).
#[test]
fn test_idle_timeout_fires_on_stale_progress() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 300; // long global run_timeout — must not be what fires
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    scheduler.graph.tasks[0].timeout = Some(crate::graph::TimeoutPolicy {
        run_timeout_secs: None,
        idle_timeout_secs: Some(1), // short — we sleep past it below
    });
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(), // recent — run_timeout nowhere close
            admission_permit: None,
            last_progress_at: Some(progress_handle_now()),
        },
    );
    std::thread::sleep(Duration::from_millis(1_100)); // let the 1s idle_timeout actually elapse

    let actions = scheduler.tick();
    let has_cancel = actions
        .iter()
        .any(|a| matches!(a, SchedulerAction::Cancel { .. }));
    assert!(
        has_cancel,
        "stale progress heartbeat must fire idle timeout"
    );
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
    let output = scheduler.graph.tasks[0]
        .result
        .as_ref()
        .expect("timeout must populate TaskResult")
        .output
        .clone();
    assert!(
        output.contains("idle timeout"),
        "TaskResult.output must name the idle cause, got: {output}"
    );
}

/// Mirror of the fire test: a task with the same short `idle_timeout_secs` but a heartbeat
/// that was just refreshed must NOT be killed — idle enforcement tracks real progress, not
/// just task age.
#[test]
fn test_idle_timeout_does_not_fire_while_progress_continues() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 300;
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    scheduler.graph.tasks[0].timeout = Some(crate::graph::TimeoutPolicy {
        run_timeout_secs: None,
        idle_timeout_secs: Some(60), // generous relative to the fresh heartbeat below
    });
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: Some(progress_handle_now()),
        },
    );

    let actions = scheduler.tick();
    let has_cancel = actions
        .iter()
        .any(|a| matches!(a, SchedulerAction::Cancel { .. }));
    assert!(
        !has_cancel,
        "a task with a fresh heartbeat must not be idle-killed"
    );
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Running);
}

/// F2 regression: a task with `last_progress_at: None` (the `RunInline` exemption — see
/// `RunningTask::last_progress_at` doc and the `record_spawn` call site in
/// `scheduler_loop.rs::handle_run_inline_action`) must never be idle-killed, even with an
/// idle timeout configured and a task age far past it. This is the `DagScheduler`-level half
/// of the guarantee; the exemption holds regardless of how stale `started_at` is because the
/// idle branch short-circuits on `last_progress_at.is_none()` before ever comparing durations.
#[test]
fn test_idle_timeout_exempt_without_progress_handle() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 300; // long — run_timeout must not be what's tested here
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    scheduler.graph.tasks[0].timeout = Some(crate::graph::TimeoutPolicy {
        run_timeout_secs: None,
        idle_timeout_secs: Some(1), // short — would fire immediately if last_progress_at were Some
    });
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now()
                .checked_sub(Duration::from_secs(5))
                .unwrap(),
            admission_permit: None,
            last_progress_at: None, // RunInline-style exemption
        },
    );

    let actions = scheduler.tick();
    let has_cancel = actions
        .iter()
        .any(|a| matches!(a, SchedulerAction::Cancel { .. }));
    assert!(
        !has_cancel,
        "a task with no progress handle must never be idle-killed, regardless of age"
    );
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Running);
}

/// Multi-task isolation: two independently-running tasks each carry their own
/// `Arc<AtomicU64>` heartbeat. A stale heartbeat on one task must not affect the other —
/// every idle-timeout test above uses a single-task graph, which cannot by itself rule out
/// a bug that reads/writes the wrong task's handle (e.g. an accidental shared `Arc`, or a
/// `check_timeouts` loop that mixes up per-task state).
#[test]
fn test_idle_timeout_multi_task_heartbeats_are_independent() {
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 300; // long — isolate idle-timeout behavior from run-timeout
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    let short_idle = crate::graph::TimeoutPolicy {
        run_timeout_secs: None,
        idle_timeout_secs: Some(1),
    };
    scheduler.graph.tasks[0].timeout = Some(short_idle.clone());
    scheduler.graph.tasks[1].timeout = Some(short_idle);
    // Use Skip (not the graph default Abort) on task 0 so its idle-timeout kill doesn't
    // cascade-abort the whole graph and collaterally cancel unrelated task 1 (mirrors
    // test_per_task_timeout_override_fires_before_global_default's rationale) — isolates
    // per-task heartbeat independence from unrelated Abort cascade semantics.
    scheduler.graph.tasks[0].failure_strategy = Some(zeph_config::FailureStrategy::Skip);
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.graph.tasks[1].status = TaskStatus::Running;

    // Task 0's heartbeat is recorded first, then we sleep past the 1s idle window, then
    // task 1's heartbeat is recorded fresh — so task 0's Arc is genuinely stale relative to
    // the idle window while task 1's is not, and each task holds a distinct Arc.
    let handle0 = progress_handle_now();
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: Some(handle0),
        },
    );
    std::thread::sleep(Duration::from_millis(1_100));
    let handle1 = progress_handle_now();
    scheduler.running.insert(
        TaskId(1),
        RunningTask {
            agent_handle_id: "h1".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: Some(handle1),
        },
    );

    let actions = scheduler.tick();
    let canceled: Vec<&str> = actions
        .iter()
        .filter_map(|a| match a {
            SchedulerAction::Cancel { agent_handle_id } => Some(agent_handle_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        canceled,
        vec!["h0"],
        "only the task with the stale heartbeat should be idle-killed"
    );
    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Skipped,
        "task 0 (stale heartbeat) must be killed — Skip strategy turns the timeout-Failed \
         status into Skipped via propagate_failure, same as the existing per-task-timeout test"
    );
    assert_eq!(
        scheduler.graph.tasks[1].status,
        TaskStatus::Running,
        "task 1's own fresh heartbeat must keep it alive, unaffected by task 0's staleness"
    );
}

/// F4: when both run-timeout and idle-timeout are exceeded on the same tick, run must win
/// — it is the hard wall-clock cap, idle the softer liveness signal. Verified indirectly via
/// the synthetic `TaskResult.output` cause string (the `TimeoutCause` enum itself is private).
#[test]
fn test_run_timeout_wins_precedence_over_idle_on_same_tick() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 1; // both run and idle will be exceeded
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    scheduler.graph.tasks[0].timeout = Some(crate::graph::TimeoutPolicy {
        run_timeout_secs: None,
        idle_timeout_secs: Some(1),
    });
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap(),
            admission_permit: None,
            // Value irrelevant to this test: run-timeout is checked first and already
            // exceeded via started_at above, so the idle branch is never reached regardless
            // of heartbeat freshness.
            last_progress_at: Some(progress_handle_now()),
        },
    );

    scheduler.tick();
    let output = scheduler.graph.tasks[0]
        .result
        .as_ref()
        .expect("timeout must populate TaskResult")
        .output
        .clone();
    assert!(
        output.contains("run timeout"),
        "run timeout must win precedence on a same-tick tie, got: {output}"
    );
    assert!(
        !output.contains("idle timeout"),
        "only one cause must be reported per NFR-OB-01, got: {output}"
    );
}

/// F5: a run-timeout kill (no idle policy configured at all) must also populate
/// `TaskResult.output` with a cause — this was a latent bug (timeout kills previously left
/// `result: None`, showing no reason anywhere in the TUI/CLI).
#[test]
fn test_run_timeout_populates_task_result_cause() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 1;
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    scheduler.tick();
    let result = scheduler.graph.tasks[0]
        .result
        .as_ref()
        .expect("run-timeout kill must populate TaskResult (F5 fix)");
    assert!(result.output.contains("run timeout"));
    assert_eq!(result.agent_id.as_deref(), Some("h0"));
    assert_eq!(result.agent_def.as_deref(), Some("worker"));
}

#[test]
fn test_effective_run_timeout_falls_back_to_global_default() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 300;
    let defs = vec![make_def("worker")];
    let scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    assert_eq!(
        scheduler.effective_run_timeout(TaskId(0)),
        Duration::from_mins(5)
    );
}

#[test]
fn test_effective_run_timeout_uses_per_task_override() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut config = make_config();
    config.task_timeout_secs = 300;
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();
    scheduler.graph.tasks[0].timeout = Some(crate::graph::TimeoutPolicy {
        run_timeout_secs: Some(45),
        idle_timeout_secs: None,
    });

    assert_eq!(
        scheduler.effective_run_timeout(TaskId(0)),
        Duration::from_secs(45)
    );
}

#[test]
fn test_effective_idle_timeout_none_when_unset_anywhere() {
    // Unlike effective_run_timeout, idle is opt-in — with no per-task override and no
    // global default configured, the effective value must be None (disabled), never a
    // sentinel Duration.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let defs = vec![make_def("worker")];
    let scheduler =
        DagScheduler::new(graph, &make_config(), Box::new(FirstRouter), defs, None).unwrap();
    assert_eq!(scheduler.effective_idle_timeout(TaskId(0)), None);
}

#[test]
fn test_effective_idle_timeout_falls_back_to_global_default() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = zeph_config::OrchestrationConfig {
        default_idle_timeout_secs: Some(30),
        ..make_config()
    };
    let defs = vec![make_def("worker")];
    let scheduler = DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    assert_eq!(
        scheduler.effective_idle_timeout(TaskId(0)),
        Some(Duration::from_secs(30))
    );
}

#[test]
fn test_effective_idle_timeout_uses_per_task_override_over_global_default() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = zeph_config::OrchestrationConfig {
        default_idle_timeout_secs: Some(30),
        ..make_config()
    };
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();
    scheduler.graph.tasks[0].timeout = Some(crate::graph::TimeoutPolicy {
        run_timeout_secs: None,
        idle_timeout_secs: Some(5),
    });

    assert_eq!(
        scheduler.effective_idle_timeout(TaskId(0)),
        Some(Duration::from_secs(5)),
        "per-task override must win over the global default (30s), not merge with it"
    );
}

#[test]
fn test_cancel_all() {
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );
    scheduler.graph.tasks[1].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(1),
        RunningTask {
            agent_handle_id: "h1".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let actions = scheduler.cancel_all();

    assert_eq!(scheduler.graph.status, GraphStatus::Canceled);
    assert!(scheduler.running.is_empty());
    let cancel_count = actions
        .iter()
        .filter(|a| matches!(a, SchedulerAction::Cancel { .. }))
        .count();
    assert_eq!(cancel_count, 2);
    assert!(actions.iter().any(|a| matches!(
        a,
        SchedulerAction::Done {
            status: GraphStatus::Canceled
        }
    )));
}

#[test]
fn test_record_spawn_failure() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    // Simulate task marked Running (by tick) but spawn failed.
    scheduler.graph.tasks[0].status = TaskStatus::Running;

    let error = SubAgentError::Spawn("spawn error".to_string());
    let actions = scheduler.record_spawn_failure(TaskId(0), &error);
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
    // With Abort strategy and no other running tasks, graph should be Failed.
    assert_eq!(scheduler.graph.status, GraphStatus::Failed);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Done { .. }))
    );
}

/// Issue #6545 (S2): unlike `ConcurrencyLimit`, `SessionSpawnLimit` is not special-cased as
/// transient inside `record_spawn_failure` — it falls through to `TaskStatus::Failed` +
/// `dag::propagate_failure`, exactly like any other `SubAgentError` variant.
/// `record_spawn_failure` performs no error-type inspection beyond the `ConcurrencyLimit`
/// check, so what happens next is entirely a function of the node's `FailureStrategy`. This
/// test pins that behavior under the shipped **default** (`FailureStrategy::Abort`): the
/// failure is terminal for the graph. It does **not** mean `SessionSpawnLimit` can never be
/// retried in general — under the opt-in `FailureStrategy::Retry`, `propagate_failure`'s own
/// `Retry` arm resurrects *any* `Failed` node (including one classified from
/// `SessionSpawnLimit`) back to `Ready`, bounded by `max_retries` — see
/// `test_record_spawn_failure_session_spawn_limit_bounded_retry_under_retry_strategy` below.
#[test]
fn test_record_spawn_failure_session_spawn_limit_marks_failed_not_ready() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;

    let error = SubAgentError::SessionSpawnLimit {
        spawned: 100,
        max: 100,
    };
    let actions = scheduler.record_spawn_failure(TaskId(0), &error);
    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Failed,
        "under the default FailureStrategy::Abort, SessionSpawnLimit is terminal"
    );
    assert_eq!(scheduler.graph.status, GraphStatus::Failed);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Done { .. }))
    );
}

/// Issue #6545 (I6): pins the `FailureStrategy::Retry` case the test above explicitly does not
/// cover. `record_spawn_failure` classifies `SessionSpawnLimit` identically to every
/// non-`ConcurrencyLimit` error — it has no special "unretryable" marking — so under a node's
/// opt-in `Retry` strategy, `dag::propagate_failure` resurrects it back to `Ready` like any
/// other failure, bounded by `max_retries` (not unbounded — the guard itself is check-only and
/// never consumes budget, so these bounded retries cost nothing either way, but the node does
/// leave `Failed` and re-enter `Ready` rather than staying terminal).
#[test]
fn test_record_spawn_failure_session_spawn_limit_bounded_retry_under_retry_strategy() {
    use crate::graph::FailureStrategy;

    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].failure_strategy = Some(FailureStrategy::Retry);
    scheduler.graph.tasks[0].max_retries = Some(2);
    scheduler.graph.tasks[0].retry_count = 0;

    let error = SubAgentError::SessionSpawnLimit {
        spawned: 100,
        max: 100,
    };

    // First two failures: resurrected to Ready, retry_count increments, no terminal actions.
    for expected_retry_count in 1..=2 {
        scheduler.graph.tasks[0].status = TaskStatus::Running;
        let actions = scheduler.record_spawn_failure(TaskId(0), &error);
        assert_eq!(
            scheduler.graph.tasks[0].status,
            TaskStatus::Ready,
            "under FailureStrategy::Retry, SessionSpawnLimit is resurrected like any other \
             failure while retry_count < max_retries"
        );
        assert_eq!(scheduler.graph.tasks[0].retry_count, expected_retry_count);
        assert!(
            actions.is_empty(),
            "propagate_failure's Retry branch returns no cancel/done actions on resurrection"
        );
    }

    // Third failure: max_retries (2) exhausted — falls through to terminal Abort behavior,
    // proving the retry is bounded, not unbounded.
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    let actions = scheduler.record_spawn_failure(TaskId(0), &error);
    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Failed,
        "retries must be bounded by max_retries, not indefinite"
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Done { .. }))
    );
}

#[test]
fn test_record_spawn_failure_concurrency_limit_reverts_to_ready() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    // Simulate tick() optimistically marking the task Running before spawn.
    scheduler.graph.tasks[0].status = TaskStatus::Running;

    // Concurrency limit hit — transient, should not fail the task.
    let error = SubAgentError::ConcurrencyLimit { active: 4, max: 4 };
    let actions = scheduler.record_spawn_failure(TaskId(0), &error);
    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Ready,
        "task must revert to Ready so the next tick can retry"
    );
    assert_eq!(
        scheduler.graph.status,
        GraphStatus::Running,
        "graph must stay Running, not transition to Failed"
    );
    assert!(
        actions.is_empty(),
        "no cancel or done actions expected for a transient deferral"
    );
}

#[test]
fn test_record_spawn_failure_concurrency_limit_variant_spawn_for_task() {
    // Both spawn() and resume() now return SubAgentError::ConcurrencyLimit — verify handling.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].status = TaskStatus::Running;

    let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
    let actions = scheduler.record_spawn_failure(TaskId(0), &error);
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
    assert!(actions.is_empty());
}

#[test]
fn test_concurrency_deferral_does_not_affect_running_task() {
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut scheduler = make_scheduler(graph);

    // Simulate both tasks optimistically marked Running by tick().
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );
    scheduler.graph.tasks[1].status = TaskStatus::Running;

    // Task 1 spawn fails with concurrency limit.
    let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
    let actions = scheduler.record_spawn_failure(TaskId(1), &error);

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Running,
        "task 0 must remain Running"
    );
    assert_eq!(
        scheduler.graph.tasks[1].status,
        TaskStatus::Ready,
        "task 1 must revert to Ready"
    );
    assert_eq!(
        scheduler.graph.status,
        GraphStatus::Running,
        "graph must stay Running"
    );
    assert!(actions.is_empty(), "no cancel or done actions expected");
}

#[test]
fn test_max_concurrent_zero_no_infinite_loop() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = zeph_config::OrchestrationConfig {
        max_parallel: 0,
        ..make_config()
    };
    let mut scheduler = DagScheduler::new(
        graph,
        &config,
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    let actions1 = scheduler.tick();
    assert!(
        actions1
            .iter()
            .all(|a| !matches!(a, SchedulerAction::Spawn { .. })),
        "no Spawn expected when max_parallel=0"
    );
    assert!(
        actions1
            .iter()
            .all(|a| !matches!(a, SchedulerAction::Done { .. })),
        "no Done(Failed) expected — ready tasks exist, so no deadlock"
    );
    assert_eq!(scheduler.graph.status, GraphStatus::Running);

    let actions2 = scheduler.tick();
    assert!(
        actions2
            .iter()
            .all(|a| !matches!(a, SchedulerAction::Done { .. })),
        "second tick must not emit Done(Failed) — ready tasks still exist"
    );
    assert_eq!(
        scheduler.graph.status,
        GraphStatus::Running,
        "graph must remain Running"
    );
}

#[test]
fn test_all_tasks_deferred_graph_stays_running() {
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut scheduler = make_scheduler(graph);

    // First tick emits Spawn for both tasks and marks them Running.
    let actions = scheduler.tick();
    assert_eq!(
        actions
            .iter()
            .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
            .count(),
        2,
        "expected 2 Spawn actions on first tick"
    );
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Running);
    assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Running);

    // Both spawns fail — both revert to Ready.
    let error = SubAgentError::ConcurrencyLimit { active: 2, max: 2 };
    let r0 = scheduler.record_spawn_failure(TaskId(0), &error);
    let r1 = scheduler.record_spawn_failure(TaskId(1), &error);
    assert!(r0.is_empty() && r1.is_empty(), "no cancel/done on deferral");
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
    assert_eq!(scheduler.graph.tasks[1].status, TaskStatus::Ready);
    assert_eq!(scheduler.graph.status, GraphStatus::Running);

    // Second tick must retry both deferred tasks.
    let retry_actions = scheduler.tick();
    let spawn_count = retry_actions
        .iter()
        .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
        .count();
    assert!(
        spawn_count > 0,
        "second tick must re-emit Spawn for deferred tasks"
    );
    assert!(
        retry_actions.iter().all(|a| !matches!(
            a,
            SchedulerAction::Done {
                status: GraphStatus::Failed,
                ..
            }
        )),
        "no Done(Failed) expected"
    );
}

#[test]
fn test_no_agent_routes_inline() {
    // NoneRouter: when no agent matches, task falls back to RunInline.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler_with_router(graph, Box::new(NoneRouter));
    let actions = scheduler.tick();
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Running);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::RunInline { .. }))
    );
}

#[test]
fn test_stale_event_rejected() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    // Simulate task running with handle "current-handle".
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "current-handle".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    // Send a completion event from the OLD agent (stale handle).
    let stale_event = TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "old-handle".to_string(),
        outcome: TaskOutcome::Completed {
            output: "stale output".to_string(),
            artifacts: vec![],
            tool_trace: None,
        },
    };
    scheduler.buffered_events.push_back(stale_event);
    let actions = scheduler.tick();

    assert_ne!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Completed,
        "stale event must not complete the task"
    );
    let has_done = actions
        .iter()
        .any(|a| matches!(a, SchedulerAction::Done { .. }));
    assert!(
        !has_done,
        "no Done action should be emitted for a stale event"
    );
    assert!(
        scheduler.running.contains_key(&TaskId(0)),
        "running task must remain after stale event"
    );
}

#[test]
fn test_duration_ms_computed_correctly() {
    // Regression test for C1: duration_ms must be non-zero after completion.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now()
                .checked_sub(Duration::from_millis(50))
                .unwrap(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let event = TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "h0".to_string(),
        outcome: TaskOutcome::Completed {
            output: "result".to_string(),
            artifacts: vec![],
            tool_trace: None,
        },
    };
    scheduler.buffered_events.push_back(event);
    scheduler.tick();

    let result = scheduler.graph.tasks[0].result.as_ref().unwrap();
    assert!(
        result.duration_ms > 0,
        "duration_ms should be > 0, got {}",
        result.duration_ms
    );
}

// --- #1619 regression tests: consecutive_spawn_failures + exponential backoff ---

#[test]
fn test_consecutive_spawn_failures_increments_on_concurrency_limit() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].status = TaskStatus::Running;

    assert_eq!(scheduler.consecutive_spawn_failures, 0, "starts at zero");

    let error = SubAgentError::ConcurrencyLimit { active: 4, max: 4 };
    scheduler.record_spawn_failure(TaskId(0), &error);
    scheduler.record_batch_backoff(false, true);
    assert_eq!(
        scheduler.consecutive_spawn_failures, 1,
        "first deferral tick: consecutive_spawn_failures must be 1"
    );

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.record_spawn_failure(TaskId(0), &error);
    scheduler.record_batch_backoff(false, true);
    assert_eq!(
        scheduler.consecutive_spawn_failures, 2,
        "second deferral tick: consecutive_spawn_failures must be 2"
    );

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.record_spawn_failure(TaskId(0), &error);
    scheduler.record_batch_backoff(false, true);
    assert_eq!(
        scheduler.consecutive_spawn_failures, 3,
        "third deferral tick: consecutive_spawn_failures must be 3"
    );
}

#[test]
fn test_consecutive_spawn_failures_resets_on_success() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].status = TaskStatus::Running;

    let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
    scheduler.record_spawn_failure(TaskId(0), &error);
    scheduler.record_batch_backoff(false, true);
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.record_spawn_failure(TaskId(0), &error);
    scheduler.record_batch_backoff(false, true);
    assert_eq!(scheduler.consecutive_spawn_failures, 2);

    scheduler.record_spawn(
        TaskId(0),
        "handle-0".to_string(),
        "worker".to_string(),
        None,
    );
    assert_eq!(
        scheduler.consecutive_spawn_failures, 0,
        "record_spawn must reset consecutive_spawn_failures to 0"
    );
}

// #6737: wait_event's deferral backoff is a real tokio::time::sleep; the test measures it via
// tokio::time::Instant (not std::time::Instant), so its elapsed() readings track the paused
// virtual clock exactly — start_paused fast-forwards ~1.9s of real sleeping without changing
// what the assertions observe. tokio-orchestration's dev-deps needed "test-util" added for this.
#[tokio::test(start_paused = true)]
async fn test_exponential_backoff_duration() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = zeph_config::OrchestrationConfig {
        deferral_backoff_ms: 50,
        ..make_config()
    };
    let mut scheduler = DagScheduler::new(
        graph,
        &config,
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    // consecutive_spawn_failures=0 → sleep ≈ 50ms (base).
    assert_eq!(scheduler.consecutive_spawn_failures, 0);
    let start = tokio::time::Instant::now();
    scheduler.wait_event().await;
    let elapsed0 = start.elapsed();
    assert!(
        elapsed0.as_millis() >= 50,
        "backoff with 0 deferrals must be >= base (50ms), got {}ms",
        elapsed0.as_millis()
    );

    // Simulate 3 consecutive deferrals: multiplier = 2^3 = 8 → 400ms, capped at 5000ms.
    scheduler.consecutive_spawn_failures = 3;
    let start = tokio::time::Instant::now();
    scheduler.wait_event().await;
    let elapsed3 = start.elapsed();
    assert!(
        elapsed3.as_millis() >= 400,
        "backoff with 3 deferrals must be >= 400ms (50 * 8), got {}ms",
        elapsed3.as_millis()
    );

    // Simulate 20 consecutive deferrals: exponent capped at 10 → 50 * 1024 = 51200 → capped at 5000ms.
    scheduler.consecutive_spawn_failures = 20;
    let start = tokio::time::Instant::now();
    scheduler.wait_event().await;
    let elapsed_capped = start.elapsed();
    assert!(
        elapsed_capped.as_millis() >= 5000,
        "backoff must be capped at 5000ms with high deferrals, got {}ms",
        elapsed_capped.as_millis()
    );
}

#[tokio::test]
async fn test_wait_event_nearest_deadline_reflects_per_task_override() {
    // task 0 has a short per-task override that has already nearly elapsed; task 1 has
    // no override and relies on a very long global default. wait_event()'s computed
    // wait must reflect the nearer (task 0's) deadline, not the uniform global one.
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let config = zeph_config::OrchestrationConfig {
        task_timeout_secs: 300,
        ..make_config()
    };
    let mut scheduler = DagScheduler::new(
        graph,
        &config,
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    scheduler.graph.tasks[0].timeout = Some(crate::graph::TimeoutPolicy {
        run_timeout_secs: Some(1),
        idle_timeout_secs: None,
    });
    let now = std::time::Instant::now();
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: now.checked_sub(Duration::from_millis(950)).unwrap(),
            admission_permit: None,
            last_progress_at: None,
        },
    );
    scheduler.running.insert(
        TaskId(1),
        RunningTask {
            agent_handle_id: "h1".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: now,
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let start = tokio::time::Instant::now();
    scheduler.wait_event().await;
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 5000,
        "wait_event must return promptly based on task 0's near-elapsed override, \
         not the 300s global default; got {}ms",
        elapsed.as_millis()
    );
}

#[tokio::test]
async fn test_wait_event_sleeps_deferral_backoff_when_running_empty() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = zeph_config::OrchestrationConfig {
        deferral_backoff_ms: 50,
        ..make_config()
    };
    let mut scheduler = DagScheduler::new(
        graph,
        &config,
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    assert!(scheduler.running.is_empty());

    let start = tokio::time::Instant::now();
    scheduler.wait_event().await;
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() >= 50,
        "wait_event must sleep at least deferral_backoff (50ms) when running is empty, but only slept {}ms",
        elapsed.as_millis()
    );
}

#[test]
fn test_current_deferral_backoff_exponential_growth() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = zeph_config::OrchestrationConfig {
        deferral_backoff_ms: 250,
        ..make_config()
    };
    let mut scheduler = DagScheduler::new(
        graph,
        &config,
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    assert_eq!(
        scheduler.current_deferral_backoff(),
        Duration::from_millis(250)
    );

    scheduler.consecutive_spawn_failures = 1;
    assert_eq!(
        scheduler.current_deferral_backoff(),
        Duration::from_millis(500)
    );

    scheduler.consecutive_spawn_failures = 2;
    assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(1));

    scheduler.consecutive_spawn_failures = 3;
    assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(2));

    scheduler.consecutive_spawn_failures = 4;
    assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(4));

    // Cap at 5 seconds.
    scheduler.consecutive_spawn_failures = 5;
    assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(5));

    scheduler.consecutive_spawn_failures = 100;
    assert_eq!(scheduler.current_deferral_backoff(), Duration::from_secs(5));
}

#[test]
fn test_record_spawn_resets_consecutive_failures() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = DagScheduler::new(
        graph,
        &make_config(),
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    scheduler.consecutive_spawn_failures = 3;
    let task_id = TaskId(0);
    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.record_spawn(task_id, "handle-1".into(), "worker".into(), None);

    assert_eq!(scheduler.consecutive_spawn_failures, 0);
}

#[test]
fn test_record_spawn_failure_reverts_to_ready_no_counter_change() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = DagScheduler::new(
        graph,
        &make_config(),
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    assert_eq!(scheduler.consecutive_spawn_failures, 0);
    let task_id = TaskId(0);
    scheduler.graph.tasks[0].status = TaskStatus::Running;

    let error = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
    scheduler.record_spawn_failure(task_id, &error);

    assert_eq!(scheduler.consecutive_spawn_failures, 0);
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Ready);
}

#[test]
fn test_parallel_dispatch_all_ready() {
    let nodes: Vec<_> = (0..6).map(|i| make_node(i, &[])).collect();
    let graph = graph_from_nodes(nodes);
    let config = zeph_config::OrchestrationConfig {
        max_parallel: 2,
        ..make_config()
    };
    let mut scheduler = DagScheduler::new(
        graph,
        &config,
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    let actions = scheduler.tick();
    let spawn_count = actions
        .iter()
        .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
        .count();
    assert_eq!(
        spawn_count, 2,
        "only max_parallel=2 tasks dispatched per tick"
    );

    let running_count = scheduler
        .graph
        .tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Running)
        .count();
    assert_eq!(running_count, 2, "only 2 tasks marked Running");
}

#[test]
fn test_batch_backoff_partial_success() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.consecutive_spawn_failures = 3;

    scheduler.record_batch_backoff(true, true);
    assert_eq!(
        scheduler.consecutive_spawn_failures, 0,
        "any success in batch must reset counter"
    );
}

#[test]
fn test_batch_backoff_all_failed() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.consecutive_spawn_failures = 2;

    scheduler.record_batch_backoff(false, true);
    assert_eq!(
        scheduler.consecutive_spawn_failures, 3,
        "all-failure tick must increment counter"
    );
}

#[test]
fn test_batch_backoff_no_spawns() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.consecutive_spawn_failures = 5;

    scheduler.record_batch_backoff(false, false);
    assert_eq!(
        scheduler.consecutive_spawn_failures, 5,
        "no spawns must not change counter"
    );
}

#[test]
fn test_buffer_guard_uses_task_count() {
    let nodes: Vec<_> = (0..10).map(|i| make_node(i, &[])).collect();
    let graph = graph_from_nodes(nodes);
    let config = zeph_config::OrchestrationConfig {
        max_parallel: 2,
        ..make_config()
    };
    let scheduler = DagScheduler::new(
        graph,
        &config,
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();
    assert_eq!(scheduler.graph.tasks.len() * 2, 20);
    assert_eq!(scheduler.max_parallel * 2, 4);
}

#[test]
fn test_batch_mixed_concurrency_and_fatal_failure() {
    use crate::graph::FailureStrategy;

    let mut nodes = vec![make_node(0, &[]), make_node(1, &[])];
    nodes[1].failure_strategy = Some(FailureStrategy::Skip);
    let graph = graph_from_nodes(nodes);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.graph.tasks[1].status = TaskStatus::Running;

    let concurrency_err = SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
    let actions0 = scheduler.record_spawn_failure(TaskId(0), &concurrency_err);
    assert!(
        actions0.is_empty(),
        "ConcurrencyLimit must produce no extra actions"
    );
    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Ready,
        "task 0 must revert to Ready"
    );

    let fatal_err = SubAgentError::Spawn("provider unavailable".to_string());
    let actions1 = scheduler.record_spawn_failure(TaskId(1), &fatal_err);
    assert_eq!(
        scheduler.graph.tasks[1].status,
        TaskStatus::Skipped,
        "task 1: Skip strategy turns Failed into Skipped via propagate_failure"
    );
    assert!(
        actions1
            .iter()
            .all(|a| !matches!(a, SchedulerAction::Done { .. })),
        "no Done action expected: task 0 is still Ready"
    );

    scheduler.consecutive_spawn_failures = 0;
    scheduler.record_batch_backoff(false, true);
    assert_eq!(
        scheduler.consecutive_spawn_failures, 1,
        "batch with only ConcurrencyLimit must increment counter"
    );
}

#[test]
fn test_deadlock_marks_non_terminal_tasks_canceled() {
    let mut nodes = vec![make_node(0, &[]), make_node(1, &[0]), make_node(2, &[0])];
    nodes[0].status = TaskStatus::Failed;
    nodes[1].status = TaskStatus::Pending;
    nodes[2].status = TaskStatus::Pending;

    let mut graph = graph_from_nodes(nodes);
    graph.status = GraphStatus::Failed;

    let mut scheduler = DagScheduler::resume_from(
        graph,
        &make_config(),
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    let actions = scheduler.tick();

    assert!(
        actions.iter().any(|a| matches!(
            a,
            SchedulerAction::Done {
                status: GraphStatus::Failed
            }
        )),
        "deadlock must emit Done(Failed); got: {actions:?}"
    );
    assert_eq!(scheduler.graph.status, GraphStatus::Failed);
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
    assert_eq!(
        scheduler.graph.tasks[1].status,
        TaskStatus::Canceled,
        "Pending task must be Canceled on deadlock"
    );
    assert_eq!(
        scheduler.graph.tasks[2].status,
        TaskStatus::Canceled,
        "Pending task must be Canceled on deadlock"
    );
}

#[test]
fn test_deadlock_not_triggered_when_task_running() {
    let mut nodes = vec![make_node(0, &[]), make_node(1, &[0])];
    nodes[0].status = TaskStatus::Running;
    nodes[0].assigned_agent = Some("handle-1".into());
    nodes[1].status = TaskStatus::Pending;

    let mut graph = graph_from_nodes(nodes);
    graph.status = GraphStatus::Failed;

    let mut scheduler = DagScheduler::resume_from(
        graph,
        &make_config(),
        Box::new(FirstRouter),
        vec![make_def("worker")],
        None,
    )
    .unwrap();

    let actions = scheduler.tick();

    assert!(
        actions
            .iter()
            .all(|a| !matches!(a, SchedulerAction::Done { .. })),
        "no Done action expected when a task is running; got: {actions:?}"
    );
    assert_eq!(scheduler.graph.status, GraphStatus::Running);
}

// ── Admission gate wiring tests ──────────────────────────────────────────────────────────────

fn make_def_with_provider(name: &str, provider: &str) -> zeph_subagent::SubAgentDef {
    let mut d = zeph_subagent::SubAgentDef::for_test(name);
    d.model = Some(zeph_subagent::ModelSpec::Named(provider.to_string()));
    d
}

#[test]
fn admission_gate_saturated_defers_task() {
    // Gate with capacity=1, already at capacity → task must not be spawned.
    let gate = crate::admission::AdmissionGate::new(&[("quality".to_string(), 1usize)]);
    // Exhaust the single permit so the gate is at capacity.
    let _held_permit = gate
        .try_acquire("quality")
        .expect("first permit must succeed");

    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = make_config();
    let defs = vec![make_def_with_provider("worker", "quality")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, Some(gate)).unwrap();

    let actions = scheduler.tick();
    let spawn_count = actions
        .iter()
        .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
        .count();
    assert_eq!(
        spawn_count, 0,
        "saturated gate must defer task — no Spawn emitted"
    );
    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Ready,
        "deferred task must stay Ready"
    );
}

#[test]
fn admission_gate_permit_transferred_to_running() {
    // After a successful spawn cycle the permit must live in RunningTask, not pending_permits.
    let gate = crate::admission::AdmissionGate::new(&[("quality".to_string(), 2usize)]);

    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = make_config();
    let defs = vec![make_def_with_provider("worker", "quality")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, Some(gate)).unwrap();

    let actions = scheduler.tick();
    let spawned = actions.iter().find_map(|a| {
        if let SchedulerAction::Spawn { task_id, .. } = a {
            Some(*task_id)
        } else {
            None
        }
    });
    let task_id = spawned.expect("task must be spawned");

    // Permit must be in pending_permits before record_spawn.
    assert!(
        scheduler.pending_permits.contains_key(&task_id),
        "permit must be in pending_permits after dispatch"
    );

    scheduler.graph.tasks[task_id.index()].status = TaskStatus::Running;
    scheduler.record_spawn(task_id, "handle-1".into(), "worker".into(), None);

    assert!(
        !scheduler.pending_permits.contains_key(&task_id),
        "pending_permits must be empty after record_spawn"
    );
    assert!(
        scheduler.running[&task_id].admission_permit.is_some(),
        "admission_permit must be set in RunningTask"
    );
}

#[test]
fn admission_gate_bypass_for_ungated_provider() {
    // Agent uses a provider not in the gate → task dispatched normally.
    let gate = crate::admission::AdmissionGate::new(&[("quality".to_string(), 1usize)]);

    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = make_config();
    // Agent maps to "fast" which has no gate entry.
    let defs = vec![make_def_with_provider("worker", "fast")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, Some(gate)).unwrap();

    let actions = scheduler.tick();
    let spawn_count = actions
        .iter()
        .filter(|a| matches!(a, SchedulerAction::Spawn { .. }))
        .count();
    assert_eq!(spawn_count, 1, "ungated provider must not be blocked");
}

#[test]
fn record_spawn_failure_releases_pending_permit() {
    // A fatal spawn failure must remove the pending admission permit (C2 fix).
    let gate = crate::admission::AdmissionGate::new(&[("quality".to_string(), 2usize)]);

    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = make_config();
    let defs = vec![make_def_with_provider("worker", "quality")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, Some(gate)).unwrap();

    // Simulate dispatch: insert a permit manually as tick() would.
    let permit = scheduler
        .admission_gate
        .as_ref()
        .unwrap()
        .try_acquire("quality")
        .expect("permit must be available");
    let task_id = TaskId(0);
    scheduler.pending_permits.insert(task_id, permit);
    scheduler.graph.tasks[0].status = TaskStatus::Running;

    assert!(scheduler.pending_permits.contains_key(&task_id));

    let fatal = zeph_subagent::SubAgentError::Spawn("provider unavailable".to_string());
    scheduler.record_spawn_failure(task_id, &fatal);

    assert!(
        !scheduler.pending_permits.contains_key(&task_id),
        "pending permit must be removed after fatal spawn failure"
    );
}

// --- graph_dirty / take_graph_dirty checkpoint flag tests ---

#[test]
fn graph_dirty_clear_at_construction() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let scheduler = make_scheduler(graph);
    assert!(
        !scheduler.graph_dirty,
        "graph_dirty must be false immediately after construction"
    );
}

#[test]
fn take_graph_dirty_returns_false_when_no_mutations() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    // No tick or mutation — flag must stay false.
    assert!(
        !scheduler.take_graph_dirty(),
        "take_graph_dirty must return false when no mutations occurred"
    );
    // Calling again must still return false (idempotent on clean state).
    assert!(
        !scheduler.take_graph_dirty(),
        "take_graph_dirty must remain false after a second call"
    );
}

#[test]
fn take_graph_dirty_true_after_task_completes() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let event = TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "h0".to_string(),
        outcome: TaskOutcome::Completed {
            output: "done".to_string(),
            artifacts: vec![],
            tool_trace: None,
        },
    };
    scheduler.buffered_events.push_back(event);
    scheduler.tick();

    assert!(
        scheduler.take_graph_dirty(),
        "take_graph_dirty must return true after a task completes"
    );
    // Reset invariant: second call must return false.
    assert!(
        !scheduler.take_graph_dirty(),
        "take_graph_dirty must return false on the second call (reset invariant)"
    );
}

#[test]
fn take_graph_dirty_true_after_task_fails() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    let event = TaskEvent {
        task_id: TaskId(0),
        agent_handle_id: "h0".to_string(),
        outcome: TaskOutcome::Failed {
            error: "boom".to_string(),
        },
    };
    scheduler.buffered_events.push_back(event);
    scheduler.tick();

    assert!(
        scheduler.take_graph_dirty(),
        "take_graph_dirty must return true after a task fails"
    );
    assert!(
        !scheduler.take_graph_dirty(),
        "take_graph_dirty must return false on the second call (reset invariant)"
    );
}

#[test]
fn take_graph_dirty_true_after_fatal_spawn_failure() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].status = TaskStatus::Running;

    let fatal = zeph_subagent::SubAgentError::Spawn("provider gone".to_string());
    scheduler.record_spawn_failure(TaskId(0), &fatal);

    assert!(
        scheduler.take_graph_dirty(),
        "take_graph_dirty must return true after a fatal spawn failure marks task Failed"
    );
    assert!(
        !scheduler.take_graph_dirty(),
        "take_graph_dirty must reset to false on second call"
    );
}

#[test]
fn take_graph_dirty_false_after_transient_concurrency_failure() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].status = TaskStatus::Running;

    // Transient: task reverts to Ready — no terminal state change.
    let transient = zeph_subagent::SubAgentError::ConcurrencyLimit { active: 1, max: 1 };
    scheduler.record_spawn_failure(TaskId(0), &transient);

    assert!(
        !scheduler.take_graph_dirty(),
        "transient concurrency deferral must not set graph_dirty (no terminal mutation)"
    );
}

#[test]
fn take_graph_dirty_true_after_cancel_all() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    scheduler.cancel_all();

    assert!(
        scheduler.take_graph_dirty(),
        "take_graph_dirty must return true after cancel_all"
    );
    assert!(
        !scheduler.take_graph_dirty(),
        "take_graph_dirty must reset to false on second call"
    );
}

#[test]
fn take_graph_dirty_true_after_timeout() {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = zeph_config::OrchestrationConfig {
        task_timeout_secs: 1,
        ..make_config()
    };
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();

    scheduler.graph.tasks[0].status = TaskStatus::Running;
    scheduler.running.insert(
        TaskId(0),
        RunningTask {
            agent_handle_id: "h0".to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap(),
            admission_permit: None,
            last_progress_at: None,
        },
    );

    scheduler.tick();

    assert!(
        scheduler.take_graph_dirty(),
        "take_graph_dirty must return true after a task times out"
    );
    assert!(
        !scheduler.take_graph_dirty(),
        "take_graph_dirty must reset to false on second call"
    );
}

// ── #6380: total tool-call failure must not leave a task Completed ────────────

fn make_running_task(scheduler: &mut DagScheduler, task_id: TaskId, handle_id: &str) {
    scheduler.graph.tasks[task_id.index()].status = TaskStatus::Running;
    scheduler.running.insert(
        task_id,
        RunningTask {
            agent_handle_id: handle_id.to_string(),
            agent_def_name: "worker".to_string(),
            started_at: std::time::Instant::now(),
            admission_permit: None,
            last_progress_at: None,
        },
    );
}

fn completed_event(
    task_id: TaskId,
    handle_id: &str,
    tool_trace: Option<Vec<ToolCallSummary>>,
) -> TaskEvent {
    TaskEvent {
        task_id,
        agent_handle_id: handle_id.to_string(),
        outcome: TaskOutcome::Completed {
            output: "narration".to_string(),
            artifacts: vec![],
            tool_trace,
        },
    }
}

#[test]
fn handle_completed_outcome_all_tools_failed_marks_task_failed() {
    // Part A (RunInline path): every real tool call in the synchronously-available trace
    // failed (including policy_blocked denials) — the task must not remain Completed.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    make_running_task(&mut scheduler, TaskId(0), "h0");

    scheduler.buffered_events.push_back(completed_event(
        TaskId(0),
        "h0",
        Some(vec![
            ToolCallSummary {
                tool: "create_directory".to_string(),
                args_summary: None,
                ok: false,
                is_read_only: false,
            },
            ToolCallSummary {
                tool: "write".to_string(),
                args_summary: None,
                ok: false,
                is_read_only: false,
            },
        ]),
    ));

    scheduler.tick();

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Failed,
        "a task whose every tool call failed must be marked Failed, not Completed"
    );
}

#[test]
fn handle_completed_outcome_mixed_trace_write_failed_marks_task_failed() {
    // Issue #6397: a mixed trace (successful read + failed/policy-blocked write) is the
    // common real-world shape of #6380 under the `quarantined` trust floor — read-type
    // tools pass through while write-type tools are policy-blocked. The task must be
    // corrected to Failed even though the read call succeeded.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    make_running_task(&mut scheduler, TaskId(0), "h0");

    scheduler.buffered_events.push_back(completed_event(
        TaskId(0),
        "h0",
        Some(vec![
            ToolCallSummary {
                tool: "write".to_string(),
                args_summary: None,
                ok: false,
                is_read_only: false,
            },
            ToolCallSummary {
                tool: "read".to_string(),
                args_summary: None,
                ok: true,
                is_read_only: true,
            },
        ]),
    ));

    scheduler.tick();

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Failed,
        "a mixed trace where every write-type call failed must be corrected to Failed, \
         regardless of a successful read-type call"
    );
}

#[test]
fn handle_completed_outcome_mixed_trace_write_succeeded_preserves_completed() {
    // The inverse of #6397's mixed-trace correction: when the write-type call actually
    // succeeded, a failed read-type call must not drag the task down to Failed — read
    // failures carry no weight in the heuristic once real (write-type) work succeeded.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    make_running_task(&mut scheduler, TaskId(0), "h0");

    scheduler.buffered_events.push_back(completed_event(
        TaskId(0),
        "h0",
        Some(vec![
            ToolCallSummary {
                tool: "write".to_string(),
                args_summary: None,
                ok: true,
                is_read_only: false,
            },
            ToolCallSummary {
                tool: "read".to_string(),
                args_summary: None,
                ok: false,
                is_read_only: true,
            },
        ]),
    ));

    scheduler.tick();

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Completed,
        "a mixed trace where the write-type call succeeded must preserve Completed even if \
         a read-type call failed"
    );
}

#[test]
fn handle_completed_outcome_empty_tool_trace_preserves_completed() {
    // A task that made zero tool calls (pure reasoning/narration) is deliberately not
    // treated as total failure, per `all_tool_calls_failed`'s doc comment.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    make_running_task(&mut scheduler, TaskId(0), "h0");

    scheduler
        .buffered_events
        .push_back(completed_event(TaskId(0), "h0", Some(vec![])));

    scheduler.tick();

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Completed,
        "an empty tool trace (no tool calls made) must preserve Completed"
    );
}

#[test]
fn handle_completed_outcome_none_tool_trace_preserves_completed() {
    // Pre-existing behavior (spawn dispatch path before the CheckToolOutcome correction
    // runs) must not regress: `tool_trace: None` never triggers the Part A check.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    make_running_task(&mut scheduler, TaskId(0), "h0");

    scheduler
        .buffered_events
        .push_back(completed_event(TaskId(0), "h0", None));

    scheduler.tick();

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Completed,
        "tool_trace: None must preserve Completed (no synchronous trace available)"
    );
}

#[test]
fn handle_completed_outcome_all_tools_failed_does_not_double_count_cascade() {
    // Part A branches out via `handle_failed_outcome` *before* any Completed-branch side
    // effect runs, so the task must be recorded in `CascadeDetector::RegionHealth` exactly
    // once (as a failure), never twice (once success, once failure).
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = zeph_config::OrchestrationConfig {
        cascade_routing: true,
        topology_selection: true,
        ..make_config()
    };
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();
    assert!(
        scheduler.cascade_detector.is_some(),
        "test precondition: cascade_detector must be enabled"
    );
    make_running_task(&mut scheduler, TaskId(0), "h0");

    scheduler.buffered_events.push_back(completed_event(
        TaskId(0),
        "h0",
        Some(vec![ToolCallSummary {
            tool: "write".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: false,
        }]),
    ));

    scheduler.tick();

    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
    let health = scheduler
        .cascade_detector
        .as_ref()
        .unwrap()
        .region_health()
        .get(&TaskId(0))
        .expect("region health must be recorded for this task's region");
    assert_eq!(
        health.total_tasks, 1,
        "task must be recorded exactly once in RegionHealth, not double-counted \
         (success then failure): {health:?}"
    );
    assert_eq!(
        health.failed_tasks, 1,
        "the single recorded outcome must be a failure"
    );
}

// ── DagScheduler::correct_completed_to_failed_if_all_tool_calls_failed ────────

fn scheduler_with_completed_task() -> (DagScheduler, TaskId) {
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    let task_id = TaskId(0);
    scheduler.graph.tasks[task_id.index()].status = TaskStatus::Completed;
    scheduler.graph.tasks[task_id.index()].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-1".to_string()),
        agent_def: Some("worker".to_string()),
    });
    let _ = scheduler.take_graph_dirty(); // reset dirty flag set by DagScheduler::new()/init
    (scheduler, task_id)
}

#[test]
fn correct_completed_to_failed_noop_on_none_trace() {
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let corrected = scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, None);
    assert!(!corrected);
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Completed
    );
    assert!(!scheduler.take_graph_dirty());
}

#[test]
fn correct_completed_to_failed_noop_on_all_ok_trace() {
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![ToolCallSummary {
        tool: "read".to_string(),
        args_summary: None,
        ok: true,
        is_read_only: true,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(!corrected);
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Completed
    );
    assert!(!scheduler.take_graph_dirty());
}

#[test]
fn correct_completed_to_failed_all_read_only_calls_failed_still_corrects() {
    // The fallback rule (#6380's pre-existing full-failure case) must still fire on a trace
    // with zero write-type calls: every entry is read-type, and every one of them failed.
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![ToolCallSummary {
        tool: "read".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: true,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(
        corrected,
        "a trace with no write-type calls must fall back to the 'every call failed' rule"
    );
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Failed
    );
}

#[test]
fn correct_completed_to_failed_mixed_trace_write_failed_corrects() {
    // Issue #6397: a mixed trace with a successful read and a failed write must still
    // correct to Failed — the direct regression test for the scheduler-level heuristic.
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![
        ToolCallSummary {
            tool: "read".to_string(),
            args_summary: None,
            ok: true,
            is_read_only: true,
        },
        ToolCallSummary {
            tool: "write".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: false,
        },
    ];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(
        corrected,
        "a mixed trace where the only write-type call failed must correct to Failed"
    );
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Failed
    );
}

#[test]
fn correct_completed_to_failed_mixed_trace_write_succeeded_preserves_completed() {
    // Inverse of #6397: a successful write-type call must not be dragged down by a failed
    // read-type call.
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![
        ToolCallSummary {
            tool: "read".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: true,
        },
        ToolCallSummary {
            tool: "write".to_string(),
            args_summary: None,
            ok: true,
            is_read_only: false,
        },
    ];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(
        !corrected,
        "a successful write-type call must preserve Completed even with a failed read"
    );
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Completed
    );
}

#[test]
fn correct_completed_to_failed_partial_write_success_preserves_completed() {
    // Real-world shape not covered by the single-write-call tests above: two write-type
    // calls, one succeeded and one failed, interleaved with a successful read. Partial
    // write success is genuine progress -- `all_tool_calls_failed` requires *every*
    // write-type call to have failed, so this must not be corrected.
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![
        ToolCallSummary {
            tool: "read".to_string(),
            args_summary: None,
            ok: true,
            is_read_only: true,
        },
        ToolCallSummary {
            tool: "write".to_string(),
            args_summary: None,
            ok: true,
            is_read_only: false,
        },
        ToolCallSummary {
            tool: "write".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: false,
        },
    ];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(
        !corrected,
        "partial write success (one write ok, one write failed) must preserve Completed"
    );
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Completed
    );
}

#[test]
fn correct_completed_to_failed_blocked_fetch_alongside_successful_read_corrects() {
    // Critic finding S2 (2026-07-17): `READONLY_TOOLS` and `QUARANTINE_DENIED` are not
    // complementary -- `fetch`, `web_scrape`, `load_skill`, and `invoke_skill` are in BOTH
    // lists (read-only for autonomy-gating purposes, yet denied under quarantine). A naive
    // `!is_read_only` classification would blind-spot a blocked `fetch` here since
    // `fetch.is_read_only == true`. The fix (`counts_toward_completion_heuristic`) must
    // still flag this trace: `read` succeeded but the only quarantine-denied call
    // (`fetch`) was blocked -- exactly the exfiltration/side-channel class quarantine
    // exists to stop, and it must not be masked by an unrelated successful plain read.
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![
        ToolCallSummary {
            tool: "read".to_string(),
            args_summary: None,
            ok: true,
            is_read_only: true,
        },
        ToolCallSummary {
            tool: "fetch".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: true,
        },
    ];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(
        corrected,
        "a blocked fetch (quarantine-denied despite is_read_only == true) alongside a \
         successful plain read must still correct to Failed"
    );
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Failed
    );
}

#[test]
fn correct_completed_to_failed_blocked_invoke_skill_alongside_successful_read_corrects() {
    // Same S2 blind spot as the `fetch` test above, for `invoke_skill` -- also in both
    // `READONLY_TOOLS` and `QUARANTINE_DENIED` (a skill body can perform arbitrary writes,
    // critic finding M2).
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![
        ToolCallSummary {
            tool: "list_directory".to_string(),
            args_summary: None,
            ok: true,
            is_read_only: true,
        },
        ToolCallSummary {
            tool: "invoke_skill".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: true,
        },
    ];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(
        corrected,
        "a blocked invoke_skill alongside a successful list_directory must correct to Failed"
    );
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Failed
    );
}

#[test]
fn correct_completed_to_failed_successful_fetch_alongside_failed_read_preserves_completed() {
    // Inverse of the S2 regression tests above: when the quarantine-denied-but-read-only
    // call actually succeeded, a failed plain read must not drag the task down -- mirrors
    // the existing mixed-trace inverse tests for plain write/read.
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![
        ToolCallSummary {
            tool: "read".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: true,
        },
        ToolCallSummary {
            tool: "fetch".to_string(),
            args_summary: None,
            ok: true,
            is_read_only: true,
        },
    ];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(
        !corrected,
        "a successful fetch (quarantine-denied-class call) must preserve Completed even with \
         a failed plain read"
    );
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Completed
    );
}

#[test]
fn correct_completed_to_failed_plain_read_only_trace_all_failed_still_corrects() {
    // Fallback-rule regression, refined for the S2 fix: a trace with no counting calls at
    // all (every entry both is_read_only == true and NOT quarantine-denied) must still fall
    // back to the "every call failed" rule when every entry failed -- `list_directory` is
    // read-only and not in QUARANTINE_DENIED, unlike `fetch`/`invoke_skill` above.
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![ToolCallSummary {
        tool: "list_directory".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: true,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(
        corrected,
        "a trace with zero counting calls (plain, non-denied reads only) must fall back to \
         the 'every call failed' rule"
    );
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Failed
    );
}

#[test]
fn correct_completed_to_failed_noop_on_empty_trace() {
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&[]));
    assert!(!corrected);
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Completed
    );
}

#[test]
fn correct_completed_to_failed_noop_when_task_not_completed() {
    // Must never clobber a later transition — e.g. a task already Failed by a different
    // path, or still Running.
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    scheduler.graph.tasks[task_id.index()].status = TaskStatus::Failed;
    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(
        !corrected,
        "correction must no-op when the task's current status is not Completed"
    );
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Failed
    );
}

#[test]
fn correct_completed_to_failed_success_flips_status_and_marks_output() {
    let (mut scheduler, task_id) = scheduler_with_completed_task();
    let trace = vec![
        ToolCallSummary {
            tool: "write".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: false,
        },
        ToolCallSummary {
            tool: "bash".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: false,
        },
    ];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(task_id, Some(&trace));
    assert!(corrected);
    assert_eq!(
        scheduler.graph.tasks[task_id.index()].status,
        TaskStatus::Failed
    );
    let output = &scheduler.graph.tasks[task_id.index()]
        .result
        .as_ref()
        .unwrap()
        .output;
    assert!(
        output.starts_with("original output"),
        "correction must append to, not replace, the original output: {output}"
    );
    assert!(
        output.contains("corrected") && output.contains('2'),
        "correction marker must be present and mention the failed call count: {output}"
    );
    assert!(
        scheduler.take_graph_dirty(),
        "a genuine status correction must set graph_dirty"
    );
}

// ── DagScheduler::propagate_corrected_task_failure (#6396) ────────────────────

#[test]
fn propagate_corrected_task_failure_cancels_running_dependent_and_fails_graph() {
    // Two-task graph: task 1 depends on task 0. Task 0 was already corrected
    // Completed -> Failed (mirroring the spawn-path CheckToolOutcome flow); task 1 had
    // already been unblocked and dispatched (Running) before the correction landed. The
    // default failure strategy (Abort, see `make_config`) must cancel task 1 and fail the
    // graph — parity with the RunInline path's `handle_failed_outcome`.
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });
    make_running_task(&mut scheduler, TaskId(1), "h1");
    let _ = scheduler.take_graph_dirty();

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected, "test precondition: task 0 must be corrected");

    let actions = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert!(
        actions.iter().any(
            |a| matches!(a, SchedulerAction::Cancel { agent_handle_id } if agent_handle_id == "h1")
        ),
        "the already-Running dependent must be canceled: {actions:?}"
    );
    assert!(
        actions.iter().any(
            |a| matches!(a, SchedulerAction::Done { status } if *status == GraphStatus::Failed)
        ),
        "GraphStatus leaving Running must emit a Done action: {actions:?}"
    );
    assert_eq!(scheduler.graph.status, GraphStatus::Failed);
    assert!(scheduler.graph.finished_at.is_some());
    assert!(
        !scheduler.running.contains_key(&TaskId(1)),
        "the canceled dependent must be removed from the running map"
    );
}

#[test]
fn propagate_corrected_task_failure_cancels_pending_commanded_from_target() {
    // Finding 1 (code review, 2026-07-17): a Command-handoff target `try_handoff` already
    // activated (linked via `commanded_from`, not `depends_on` -- the whole point of
    // runtime-chosen routing) before a spawn-path source is corrected Completed -> Failed
    // must be cancelled too, or it runs against state its source never legitimately
    // produced. Three-task graph: 0 is the corrected source, 1 is its handoff target (no
    // depends_on edge back to 0), 2 depends on 1 -- proves both the direct target and its
    // own transitive depends_on subtree get skipped (mirrors
    // `resolve_dormant_after_terminal`'s reasoning for an un-triggered route_to fallback).
    let graph = graph_from_nodes(vec![
        make_node(0, &[]),
        make_node(1, &[]),
        make_node(2, &[1]),
    ]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "claimed done, handed off".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });
    // Simulate try_handoff's post-activation state: target 1 is Ready, commanded_from
    // links back to source 0, with no depends_on edge.
    scheduler.graph.tasks[1].status = TaskStatus::Ready;
    scheduler.graph.tasks[1].commanded_from = Some(TaskId(0));

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected, "test precondition: task 0 must be corrected");

    let _ = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert_eq!(
        scheduler.graph.tasks[1].status,
        TaskStatus::Skipped,
        "a Pending/Ready commanded_from target must be cancelled when its source is \
         corrected to Failed post-hoc"
    );
    assert_eq!(
        scheduler.graph.tasks[2].status,
        TaskStatus::Skipped,
        "a depends_on dependent of the cancelled commanded_from target must also be \
         skipped, or it would strand in Pending forever"
    );
}

#[test]
fn propagate_corrected_task_failure_leaves_running_commanded_from_target_untouched() {
    // Deliberate scope boundary (Finding 1's fix): an already-Running commanded_from
    // target is the pre-existing, accepted "already-unblocked work is not unwound"
    // limitation, same as an ordinary Running depends_on dependent -- must not be
    // cancelled/skipped by this fix. Uses a per-task Skip failure strategy on the source
    // (rather than the default Abort) so the assertion isolates this fix's own behavior
    // from Abort's unrelated "cancel every Running task in the graph" global effect.
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].failure_strategy = Some(zeph_config::FailureStrategy::Skip);
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "claimed done, handed off".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });
    scheduler.graph.tasks[1].commanded_from = Some(TaskId(0));
    make_running_task(&mut scheduler, TaskId(1), "h1");

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected, "test precondition: task 0 must be corrected");

    let actions = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert_eq!(
        scheduler.graph.tasks[1].status,
        TaskStatus::Running,
        "an already-Running commanded_from target is out of this fix's scope"
    );
    assert!(
        !actions.iter().any(
            |a| matches!(a, SchedulerAction::Cancel { agent_handle_id } if agent_handle_id == "h1")
        ),
        "a Running commanded_from target must not be cancelled by this fix: {actions:?}"
    );
}

#[test]
fn propagate_corrected_task_failure_does_not_double_count_cascade() {
    // #6396's design note: `dag::propagate_failure` never touches `cascade_detector` —
    // calling it from `propagate_corrected_task_failure` must not change RegionHealth,
    // which was already recorded (as a success) by `handle_completed_outcome` before the
    // correction landed.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let config = zeph_config::OrchestrationConfig {
        cascade_routing: true,
        topology_selection: true,
        ..make_config()
    };
    let defs = vec![make_def("worker")];
    let mut scheduler =
        DagScheduler::new(graph, &config, Box::new(FirstRouter), defs, None).unwrap();
    assert!(
        scheduler.cascade_detector.is_some(),
        "test precondition: cascade_detector must be enabled"
    );
    make_running_task(&mut scheduler, TaskId(0), "h0");

    // Simulate the spawn dispatch path: tool_trace is None at completion time, so
    // handle_completed_outcome's #6380 RunInline branch does not fire and the task
    // completes normally, recording a cascade success.
    scheduler
        .buffered_events
        .push_back(completed_event(TaskId(0), "h0", None));
    scheduler.tick();
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Completed);

    let region_health_before = scheduler
        .cascade_detector
        .as_ref()
        .unwrap()
        .region_health()
        .get(&TaskId(0))
        .expect("region health must be recorded")
        .clone();
    assert_eq!(region_health_before.total_tasks, 1);
    assert_eq!(region_health_before.failed_tasks, 0);

    // Post-hoc correction, as CheckToolOutcome would trigger.
    let trace = vec![
        ToolCallSummary {
            tool: "read".to_string(),
            args_summary: None,
            ok: true,
            is_read_only: true,
        },
        ToolCallSummary {
            tool: "write".to_string(),
            args_summary: None,
            ok: false,
            is_read_only: false,
        },
    ];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected);
    let _ = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
    let region_health_after = scheduler
        .cascade_detector
        .as_ref()
        .unwrap()
        .region_health()
        .get(&TaskId(0))
        .expect("region health must still be present")
        .clone();
    assert_eq!(
        region_health_after.total_tasks, region_health_before.total_tasks,
        "propagate_corrected_task_failure must not change RegionHealth: {region_health_after:?}"
    );
    assert_eq!(
        region_health_after.failed_tasks, region_health_before.failed_tasks,
        "propagate_corrected_task_failure must not re-record this task as a failure: \
         {region_health_after:?}"
    );
}

#[test]
fn propagate_corrected_task_failure_noop_when_no_dependents_and_graph_stays_running() {
    // Skip strategy on a task with no dependents: propagate_failure marks the task
    // Skipped-subtree (a no-op here since there are no dependents) and does not touch
    // GraphStatus, since other tasks in the graph are still non-terminal.
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].failure_strategy = Some(zeph_config::FailureStrategy::Skip);
    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected);

    let actions = scheduler.propagate_corrected_task_failure(TaskId(0));
    assert!(
        actions.is_empty(),
        "Skip strategy with no running dependents and a still-Running graph must emit no \
         actions: {actions:?}"
    );
    assert_eq!(
        scheduler.graph.status,
        GraphStatus::Running,
        "GraphStatus must remain Running while task 1 is still non-terminal"
    );
}

#[test]
fn propagate_corrected_task_failure_retry_strategy_forces_terminal_failure() {
    // Critic finding S1 (2026-07-17): under `FailureStrategy::Retry`, generic
    // `dag::propagate_failure` would resurrect the *same* corrected task back to `Ready` for
    // another attempt -- but this task was already `Completed` (cascade accounting already
    // ran, dependents already unblocked) before the correction landed, so a resurrection
    // would trigger a redundant redispatch and double-count RegionHealth on its second
    // completion. `propagate_corrected_task_failure` must instead force terminal Abort-style
    // behavior for Retry, via `dag::propagate_failure_forced_terminal`: the task stays
    // `Failed` (never resurrected), `retry_count` is untouched, and the graph fails.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].failure_strategy = Some(zeph_config::FailureStrategy::Retry);
    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });
    assert_eq!(scheduler.graph.tasks[0].retry_count, 0);

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected, "test precondition: task 0 must be corrected");
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);

    let actions = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Failed,
        "Retry must NOT resurrect an already-completed corrected task to Ready -- that would \
         trigger a redundant redispatch and double-count RegionHealth (critic finding S1)"
    );
    assert_eq!(
        scheduler.graph.tasks[0].retry_count, 0,
        "no retry attempt was made, so retry_count must not increment"
    );
    assert_eq!(
        scheduler.graph.status,
        GraphStatus::Failed,
        "Retry is forced to terminal Abort-style behavior for a post-hoc correction"
    );
    assert!(
        actions.iter().any(
            |a| matches!(a, SchedulerAction::Done { status } if *status == GraphStatus::Failed)
        ),
        "GraphStatus leaving Running must emit a Done action: {actions:?}"
    );
}

#[test]
fn propagate_corrected_task_failure_retry_then_tick_emits_no_spawn() {
    // Follow-up to the forced-terminal test above: since the task is never resurrected under
    // Retry, driving an actual `tick()` after the correction must emit zero Spawn actions for
    // it -- the graph is already terminal (Failed), so tick() returns Done immediately.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].failure_strategy = Some(zeph_config::FailureStrategy::Retry);
    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected);
    let _ = scheduler.propagate_corrected_task_failure(TaskId(0));
    assert_eq!(scheduler.graph.tasks[0].status, TaskStatus::Failed);
    assert_eq!(scheduler.graph.status, GraphStatus::Failed);

    let actions = scheduler.tick();
    let spawn_count = actions
        .iter()
        .filter(|a| matches!(a, SchedulerAction::Spawn { task_id, .. } if *task_id == TaskId(0)))
        .count();
    assert_eq!(
        spawn_count, 0,
        "a task forced to terminal Failed under Retry must never be redispatched: {actions:?}"
    );
    assert!(
        actions.iter().any(
            |a| matches!(a, SchedulerAction::Done { status } if *status == GraphStatus::Failed)
        ),
        "tick() on an already-terminal graph must return Done: {actions:?}"
    );
}

#[test]
fn propagate_corrected_task_failure_ask_strategy_forces_terminal_failure_instead_of_pause() {
    // Companion to the Retry test: `FailureStrategy::Ask` would normally pause the whole
    // graph (`GraphStatus::Paused`) via generic `dag::propagate_failure` -- but pausing an
    // already-completed-then-corrected task's plan post-hoc is equally unsafe/meaningless
    // here (critic finding S1's `Ask` variant), so it must also force terminal Failed.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].failure_strategy = Some(zeph_config::FailureStrategy::Ask);
    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected);

    let actions = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert_eq!(
        scheduler.graph.status,
        GraphStatus::Failed,
        "Ask must not pause the graph for a post-hoc correction -- it is forced to terminal \
         Failed instead (critic finding S1)"
    );
    assert!(
        actions.iter().any(
            |a| matches!(a, SchedulerAction::Done { status } if *status == GraphStatus::Failed)
        ),
        "GraphStatus leaving Running must emit a Done action: {actions:?}"
    );
}

#[test]
fn propagate_corrected_task_failure_retry_with_state_injection_does_not_recover() {
    // Regression for the developer's own design note on `propagate_failure_forced_terminal`
    // (dag.rs): "Considered and rejected including Mode-1 recovery ... `try_recover` would
    // silently flip the just-corrected `Failed` status back to `Completed`". That claim was
    // true by code inspection (the forced-terminal function never calls `try_recover`) but
    // had no test locking it in -- the *generic* `dag::propagate_failure`'s retry-exhausted
    // arm DOES invoke Mode-1 recovery when `recovery.state_injection` is configured (see
    // `test_propagate_failure_retry_exhausted_recovers_with_state_injection` in dag.rs), so
    // this is a real divergence a future refactor could silently reintroduce by routing the
    // forced-terminal case back through the generic function. Configure the task with a
    // `state_injection` fallback that *would* flip it back to Completed under the generic
    // path, and confirm the forced-terminal path leaves it Failed instead.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].failure_strategy = Some(zeph_config::FailureStrategy::Retry);
    scheduler.graph.tasks[0].max_retries = Some(3);
    scheduler.graph.tasks[0].retry_count = 3; // at max -- would be "exhausted" under the generic path
    scheduler.graph.tasks[0].recovery = Some(crate::graph::RecoveryAction {
        state_injection: Some("fallback output".to_string()),
        route_to: None,
    });
    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected);

    let _ = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Failed,
        "the forced-terminal path must NOT invoke Mode-1 recovery -- a configured \
         state_injection fallback must not silently flip the just-corrected Failed status \
         back to Completed, which no already-unblocked dependent would ever see"
    );
    assert_ne!(
        scheduler.graph.tasks[0]
            .result
            .as_ref()
            .unwrap()
            .agent_def
            .as_deref(),
        Some("__recovery__"),
        "the recovery marker must never appear -- confirms try_recover was not invoked"
    );
    assert_eq!(scheduler.graph.status, GraphStatus::Failed);
}

#[test]
fn propagate_corrected_task_failure_abort_with_state_injection_does_not_recover() {
    // Critic finding S1-residual (2026-07-17): the default `Abort` strategy's arm in
    // `dag::propagate_failure` calls `try_recover` *before* terminal-failing the graph. When
    // `recovery.state_injection` is configured, that would flip this just-corrected task
    // straight back to `Completed` with injected output -- silently undoing the correction
    // exactly like the `Retry`/`Ask` hazard the sibling test above locks in. This is the
    // default-strategy companion to `..._retry_with_state_injection_does_not_recover`: no
    // explicit `failure_strategy` override, relying on `make_config`'s `default_failure_strategy
    // = Abort`.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    assert_eq!(
        scheduler.graph.default_failure_strategy,
        zeph_config::FailureStrategy::Abort,
        "test precondition: default strategy must be Abort (unconfigured failure_strategy)"
    );
    scheduler.graph.tasks[0].recovery = Some(crate::graph::RecoveryAction {
        state_injection: Some("fallback output".to_string()),
        route_to: None,
    });
    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected);

    let actions = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Failed,
        "default Abort strategy + state_injection must NOT invoke Mode-1 recovery on a \
         post-hoc correction -- the injected fallback would never be seen by dependents that \
         already consumed the original (now-invalidated) output"
    );
    assert_ne!(
        scheduler.graph.tasks[0]
            .result
            .as_ref()
            .unwrap()
            .agent_def
            .as_deref(),
        Some("__recovery__"),
        "the recovery marker must never appear -- confirms try_recover was not invoked"
    );
    assert_eq!(scheduler.graph.status, GraphStatus::Failed);
    assert!(
        actions.iter().any(
            |a| matches!(a, SchedulerAction::Done { status } if *status == GraphStatus::Failed)
        ),
        "GraphStatus leaving Running must emit a Done action: {actions:?}"
    );
}

#[test]
fn propagate_corrected_task_failure_skip_with_state_injection_still_uses_generic_propagate() {
    // Companion negative test: `Skip`'s arm in `dag::propagate_failure` never calls
    // `try_recover` (it goes straight to `skip_subtree`), so a `Skip`-configured task with
    // `state_injection` configured is safe via the *unmodified* generic `propagate_failure`
    // path -- it must NOT be routed through `propagate_failure_forced_terminal`. Distinguishing
    // signal: `Skip`'s arm flips the failed task's own status to `Skipped` (not left `Failed`,
    // unlike the forced-terminal path), which only the generic path does.
    let graph = graph_from_nodes(vec![make_node(0, &[])]);
    let mut scheduler = make_scheduler(graph);
    scheduler.graph.tasks[0].failure_strategy = Some(zeph_config::FailureStrategy::Skip);
    scheduler.graph.tasks[0].recovery = Some(crate::graph::RecoveryAction {
        state_injection: Some("fallback output".to_string()),
        route_to: None,
    });
    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected);

    let _ = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert_eq!(
        scheduler.graph.tasks[0].status,
        TaskStatus::Skipped,
        "Skip's arm must run unmodified (flips to Skipped, not left Failed) even with \
         state_injection configured -- Skip never calls try_recover, so it needs no \
         forced-terminal special-case"
    );
}

#[test]
fn propagate_corrected_task_failure_leaves_already_completed_dependent_untouched() {
    // Documented remaining limitation on `propagate_corrected_task_failure`: a dependent
    // that already reached a terminal status (typically Completed, having consumed the
    // now-invalidated output) before the correction landed is not retroactively unwound --
    // `dag::propagate_failure`'s cancellation/skip/retry logic only affects dependents
    // still Pending/Ready/Running. This locks in that documented behavior as a regression
    // test rather than leaving it purely as a doc comment.
    let graph = graph_from_nodes(vec![make_node(0, &[]), make_node(1, &[0])]);
    let mut scheduler = make_scheduler(graph);

    scheduler.graph.tasks[0].status = TaskStatus::Completed;
    scheduler.graph.tasks[0].result = Some(TaskResult {
        output: "original output".to_string(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: Some("agent-0".to_string()),
        agent_def: Some("worker".to_string()),
    });
    scheduler.graph.tasks[1].status = TaskStatus::Completed;
    scheduler.graph.tasks[1].result = Some(TaskResult {
        output: "dependent already finished using task 0's now-invalidated output".to_string(),
        artifacts: vec![],
        duration_ms: 5,
        agent_id: Some("agent-1".to_string()),
        agent_def: Some("worker".to_string()),
    });

    let trace = vec![ToolCallSummary {
        tool: "write".to_string(),
        args_summary: None,
        ok: false,
        is_read_only: false,
    }];
    let corrected =
        scheduler.correct_completed_to_failed_if_all_tool_calls_failed(TaskId(0), Some(&trace));
    assert!(corrected);

    let actions = scheduler.propagate_corrected_task_failure(TaskId(0));

    assert_eq!(
        scheduler.graph.tasks[1].status,
        TaskStatus::Completed,
        "a dependent that already completed before the correction landed is not \
         retroactively unwound (documented limitation)"
    );
    assert!(
        actions.iter().any(
            |a| matches!(a, SchedulerAction::Done { status } if *status == GraphStatus::Failed)
        ),
        "the graph must still terminalize as Failed even though the dependent stayed \
         Completed -- task 0 itself is Failed and no non-terminal task remains: {actions:?}"
    );
}
