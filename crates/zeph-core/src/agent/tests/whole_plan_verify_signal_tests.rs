// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests for issue #6265: a visible signal when whole-plan verification judges the plan's
//! output incomplete and no successful replan resolves it. Exercises
//! `Agent::run_whole_plan_verify` directly (it is `pub(super)`).

use zeph_orchestration::{
    DagScheduler, GraphStatus, RuleBasedRouter, TaskGraph, TaskNode, TaskResult, TaskStatus,
};

use crate::agent::agent_tests::*;

fn completed_task_graph(output: &str) -> TaskGraph {
    let mut graph = TaskGraph::new("whole-plan verify signal test goal");
    let mut node = TaskNode::new(0, "task-0", "produce output");
    node.status = TaskStatus::Completed;
    node.result = Some(TaskResult {
        output: output.to_owned(),
        artifacts: vec![],
        duration_ms: 10,
        agent_id: None,
        agent_def: None,
    });
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
    // complete: false, confidence low, but gaps empty — should_replan requires
    // !gaps.is_empty(), so this is the "confidently incomplete, no actionable gaps" case.
    r#"{"complete": false, "gaps": [], "confidence": 0.2}"#.to_string()
}

fn complete_json() -> String {
    r#"{"complete": true, "gaps": [], "confidence": 0.9}"#.to_string()
}

/// B2: whole-plan verification judges the output confidently incomplete (low confidence,
/// no actionable gaps to replan against) — the signal must fire since no replan is attempted.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn confidently_incomplete_no_gaps_emits_signal() {
    let config = base_config();
    let graph = completed_task_graph("partial answer");
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();

    let provider = mock_provider(vec![incomplete_no_gaps_json()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.services.orchestration.orchestration_config = config;

    let extra_tasks = agent
        .run_whole_plan_verify(&mut scheduler, GraphStatus::Completed)
        .await;

    assert!(
        extra_tasks.is_none(),
        "no replan is attempted for the no-actionable-gaps case"
    );
    let sent = agent.channel.sent_messages();
    assert_eq!(sent.len(), 1, "exactly one signal message must be sent");
    assert!(
        sent[0].contains("may be incomplete"),
        "message must surface the incompleteness: {sent:?}"
    );
}

/// Regression (B2 ordering): a plan verification judges the output complete — no signal, no
/// replan attempt.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn complete_plan_emits_no_signal() {
    let config = base_config();
    let graph = completed_task_graph("full answer");
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();

    let provider = mock_provider(vec![complete_json()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.services.orchestration.orchestration_config = config;

    let extra_tasks = agent
        .run_whole_plan_verify(&mut scheduler, GraphStatus::Completed)
        .await;

    assert!(extra_tasks.is_none());
    assert!(
        agent.channel.sent_messages().is_empty(),
        "a complete plan must never emit an incompleteness signal"
    );
}

/// Default-off regression: `verify_completeness = false` never invokes the verifier or the
/// signal path at all.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn verify_completeness_disabled_emits_no_signal() {
    let config = crate::config::OrchestrationConfig {
        enabled: true,
        verify_completeness: false,
        ..crate::config::OrchestrationConfig::default()
    };
    let graph = completed_task_graph("full answer");
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();

    // No provider responses queued — if the verifier were invoked, MockProvider would panic
    // or error on an empty response queue, proving this path is never reached.
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor);
    agent.services.orchestration.orchestration_config = config;

    let extra_tasks = agent
        .run_whole_plan_verify(&mut scheduler, GraphStatus::Completed)
        .await;

    assert!(extra_tasks.is_none());
    assert!(agent.channel.sent_messages().is_empty());
}
