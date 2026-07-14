// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for issue #6265 (B1): a visible signal when per-task verification (the
//! `SchedulerAction::Verify` arm in `scheduler_loop.rs`) judges a task's output incomplete
//! and no successful replan resolves it. Exercises `Agent::run_scheduler_loop` directly, since
//! per-task verify fires from inside the scheduler tick loop.

use zeph_orchestration::{
    DagScheduler, GraphStatus, RuleBasedRouter, TaskEvent, TaskGraph, TaskNode, TaskOutcome,
    TaskStatus,
};

use crate::agent::agent_tests::*;

fn running_task_graph(handle_id: &str, title: &str) -> TaskGraph {
    let mut graph = TaskGraph::new("per-task verify signal test goal");
    let mut node = TaskNode::new(0, title, "produce output");
    node.status = TaskStatus::Running;
    node.assigned_agent = Some(handle_id.to_owned());
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;
    graph
}

fn base_config() -> crate::config::OrchestrationConfig {
    crate::config::OrchestrationConfig {
        enabled: true,
        verify_completeness: true,
        ..crate::config::OrchestrationConfig::default()
    }
}

fn incomplete_no_gaps_json() -> String {
    // complete: false, no gaps to replan against — should_replan is false, so no replan is
    // attempted and the per-task signal must fire.
    r#"{"complete": false, "gaps": [], "confidence": 0.2}"#.to_string()
}

/// B1: per-task verification judges a completed task's output incomplete, with no gaps to
/// replan against — the per-task signal must fire, worded locally to that task.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn incomplete_task_with_no_repair_emits_per_task_signal() {
    let config = base_config();
    let graph = running_task_graph("handle-1", "fetch the report");
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();
    scheduler
        .event_sender()
        .try_send(TaskEvent {
            task_id: zeph_orchestration::TaskId(0),
            agent_handle_id: "handle-1".to_owned(),
            outcome: TaskOutcome::Completed {
                output: "partial answer".into(),
                artifacts: vec![],
                tool_trace: None,
            },
        })
        .unwrap();

    let provider = mock_provider(vec![incomplete_no_gaps_json()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.services.orchestration.orchestration_config = config;

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(status, GraphStatus::Completed);
    let sent = agent.channel.sent_messages();
    assert_eq!(sent.len(), 1, "exactly one per-task signal must be sent");
    assert!(
        sent[0].contains("fetch the report"),
        "message must name the task: {sent:?}"
    );
    assert!(
        sent[0].contains("unresolved gap"),
        "message must surface the incompleteness: {sent:?}"
    );
    // M1 (critic finding): wording must stay strictly local to this task, never imply
    // whole-plan incompleteness — the plan-level signal is a separate mechanism (B2).
    assert!(
        !sent[0].to_lowercase().contains("the plan"),
        "per-task message must not overclaim about the whole plan: {sent:?}"
    );
}

/// Regression: a task verified complete emits no signal.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn complete_task_emits_no_signal() {
    let config = base_config();
    let graph = running_task_graph("handle-1", "fetch the report");
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();
    scheduler
        .event_sender()
        .try_send(TaskEvent {
            task_id: zeph_orchestration::TaskId(0),
            agent_handle_id: "handle-1".to_owned(),
            outcome: TaskOutcome::Completed {
                output: "full answer".into(),
                artifacts: vec![],
                tool_trace: None,
            },
        })
        .unwrap();

    let provider = mock_provider(vec![
        r#"{"complete": true, "gaps": [], "confidence": 0.9}"#.into(),
    ]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.services.orchestration.orchestration_config = config;

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(status, GraphStatus::Completed);
    assert!(
        agent.channel.sent_messages().is_empty(),
        "a complete task must never emit a per-task incompleteness signal"
    );
}
