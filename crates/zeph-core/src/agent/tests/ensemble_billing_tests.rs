// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Acceptance test for #7090 (1): ensemble member billing at the `SchedulerAction::Verify`
//! site records the serving model, member cache tokens and a priced cost in `usage_records`.
//!
//! Registered from `tests/mod.rs` with `mod ensemble_billing_tests;`.

use std::sync::Arc;

use crate::agent::agent_tests::*;
use crate::cost::{CostTracker, DailyBudget};
use zeph_common::money::Cents;
use zeph_config::EnsembleConfig;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_llm::provider::{CacheUsage, ResponseUsage};
use zeph_memory::semantic::SemanticMemory;
use zeph_orchestration::{
    DagScheduler, GraphStatus, RuleBasedRouter, TaskEvent, TaskGraph, TaskNode, TaskOutcome,
    TaskStatus,
};

/// Model id present in the `cost.rs` default price table.
const PRICED_MODEL: &str = "claude-sonnet-4-6";
const CACHE_READ: u64 = 300;
const CACHE_WRITE: u64 = 7;

fn member(name: &str) -> (String, AnyProvider) {
    let usage = ResponseUsage {
        input: 100,
        output: 20,
        cache: Some(CacheUsage {
            creation: CACHE_WRITE,
            read: CACHE_READ,
        }),
        reasoning: None,
        auxiliary: None,
        speed: zeph_config::ServiceSpeed::default(),
    };
    let mock = MockProvider::with_responses(vec![
        r#"{"complete": true, "gaps": [], "confidence": 0.9}"#.to_owned(),
    ])
    .with_model_identifier(PRICED_MODEL)
    .with_response_usage(usage);
    (name.to_owned(), AnyProvider::Mock(mock))
}

fn running_task_graph() -> TaskGraph {
    let mut graph = TaskGraph::new("ensemble billing goal");
    let mut node = TaskNode::new(0, "task-0", "produce output");
    node.status = TaskStatus::Running;
    node.assigned_agent = Some("handle-1".to_owned());
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;
    graph
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn run_scheduler_loop_full_quorum_records_serving_model_cache_tokens_and_cost() {
    let config = crate::config::OrchestrationConfig {
        enabled: true,
        verify_completeness: true,
        ensemble: EnsembleConfig {
            enabled: true,
            verify: true,
            members: vec!["m1".into(), "m2".into(), "m3".into()],
            ..EnsembleConfig::default()
        },
        ..crate::config::OrchestrationConfig::default()
    };
    let mut scheduler = DagScheduler::resume_from(
        running_task_graph(),
        &config,
        Box::new(RuleBasedRouter),
        vec![],
        None,
    )
    .unwrap();
    scheduler
        .event_sender()
        .try_send(TaskEvent {
            task_id: zeph_orchestration::TaskId(0),
            agent_handle_id: "handle-1".to_owned(),
            outcome: TaskOutcome::Completed {
                output: "task output".into(),
                artifacts: vec![],
                tool_trace: None,
            },
        })
        .unwrap();

    let memory = Arc::new(
        SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            AnyProvider::Mock(MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap(),
    );
    let cid = memory.sqlite().create_conversation().await.unwrap();

    let mut agent = crate::agent::Agent::new(
        mock_provider(vec![]),
        MockChannel::new(vec![]),
        create_test_registry(),
        None,
        5,
        MockToolExecutor::no_tools(),
    )
    .with_memory(Arc::clone(&memory), cid, 100, 5, 1000)
    .with_cost_tracker(CostTracker::new(DailyBudget::Limited(Cents::new(
        100_000.0,
    ))))
    .with_ensemble_members(vec![member("m1"), member("m2"), member("m3")]);
    agent.services.orchestration.orchestration_config = config;

    let status = agent
        .run_scheduler_loop(
            &mut scheduler,
            1,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(status, GraphStatus::Completed);

    let source = zeph_memory::UsageSource::EnsembleMember.as_str();
    let rows: Vec<(String, String, i64, i64, f64)> =
        zeph_db::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT provider_name, model_name, cache_read_tokens, cache_write_tokens, cost_cents \
         FROM usage_records WHERE source = '{source}'"
        )))
        .fetch_all(memory.sqlite().pool())
        .await
        .unwrap();

    assert_eq!(rows.len(), 3, "one EnsembleMember row per member: {rows:?}");
    for (provider_name, model_name, cache_read, cache_write, cost_cents) in rows {
        assert!(["m1", "m2", "m3"].contains(&provider_name.as_str()));
        assert_eq!(
            model_name, PRICED_MODEL,
            "serving model, not the member name"
        );
        assert_eq!(cache_read, i64::try_from(CACHE_READ).unwrap());
        assert_eq!(cache_write, i64::try_from(CACHE_WRITE).unwrap());
        assert!(
            cost_cents > 0.0,
            "priced model must bill > 0, got {cost_cents}"
        );
    }
}
