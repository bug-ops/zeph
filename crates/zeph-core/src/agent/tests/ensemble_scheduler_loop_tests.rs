// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the ensemble branch of `SchedulerAction::Verify` in
//! `scheduler_loop.rs` (spec `073-orch-ensemble-merge`, tasks.md T4.5-T4.7).
//!
//! The `EnsembleVerifier`-level tests in `zeph-orchestration::ensemble::verifier` cover the
//! same logical scenarios (full-quorum merge, quorum-fallback) at the merge/mock-provider
//! layer. These tests exercise the layer above: the `use_ensemble` gating condition itself,
//! the `ensemble_degraded_total`/`ensemble_last_agreement_ratio`/`ensemble_member_stats`
//! metrics wiring, and the default-off regression at the `Agent::run_scheduler_loop` seam.

use crate::agent::agent_tests::*;
use zeph_config::EnsembleConfig;
use zeph_llm::LlmError;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_orchestration::{
    DagScheduler, GraphStatus, RuleBasedRouter, TaskEvent, TaskGraph, TaskNode, TaskOutcome,
    TaskStatus,
};

/// Build a graph with a single task already `Running` with `assigned_agent` set, so
/// `DagScheduler::resume_from` reconstructs the `running` map and a `TaskEvent` for that
/// handle is accepted by `process_event` on the very first `tick()`.
fn running_task_graph(handle_id: &str) -> TaskGraph {
    let mut graph = TaskGraph::new("ensemble verify test goal");
    let mut node = TaskNode::new(0, "task-0", "produce output");
    node.status = TaskStatus::Running;
    node.assigned_agent = Some(handle_id.to_owned());
    graph.tasks.push(node);
    graph.status = GraphStatus::Running;
    graph
}

fn base_orchestration_config() -> crate::config::OrchestrationConfig {
    crate::config::OrchestrationConfig {
        enabled: true,
        verify_completeness: true,
        ..crate::config::OrchestrationConfig::default()
    }
}

fn complete_json() -> String {
    r#"{"complete": true, "gaps": [], "confidence": 0.9}"#.to_string()
}

/// T4.7 (blocking acceptance criterion): with `[orchestration.ensemble].enabled = false` (the
/// default), the `SchedulerAction::Verify` handler must take the pre-existing single-provider
/// path — `ensemble_degraded_total` and `ensemble_last_agreement_ratio` must never be touched.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn default_off_regression_never_touches_ensemble_metrics() {
    let config = base_orchestration_config(); // ensemble.enabled = false (default)
    let graph = running_task_graph("handle-1");
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();
    scheduler
        .event_sender()
        .try_send(TaskEvent {
            task_id: zeph_orchestration::TaskId(0),
            agent_handle_id: "handle-1".to_owned(),
            outcome: TaskOutcome::Completed {
                output: "task output".into(),
                artifacts: vec![],
            },
        })
        .unwrap();

    let provider = mock_provider(vec![complete_json()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (metrics_tx, metrics_rx) = watch::channel(MetricsSnapshot::default());
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_metrics(metrics_tx);
    agent.services.orchestration.orchestration_config = config;

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(status, GraphStatus::Completed);
    let snapshot = metrics_rx.borrow().clone();
    assert_eq!(
        snapshot.orchestration.ensemble_degraded_total, 0,
        "default-off path must never increment ensemble_degraded_total"
    );
    assert!(
        snapshot
            .orchestration
            .ensemble_last_agreement_ratio
            .is_none(),
        "default-off path must never populate ensemble_last_agreement_ratio"
    );
    assert!(snapshot.orchestration.ensemble_member_stats.is_empty());
}

/// T4.5: full quorum (3 of 3 members agree `complete: true`) merges through the ensemble path
/// end-to-end and updates `ensemble_last_agreement_ratio` without incrementing
/// `ensemble_degraded_total`.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn full_quorum_ensemble_path_updates_agreement_ratio() {
    let config = crate::config::OrchestrationConfig {
        ensemble: EnsembleConfig {
            enabled: true,
            verify: true,
            members: vec!["m1".into(), "m2".into(), "m3".into()],
            ..EnsembleConfig::default()
        },
        ..base_orchestration_config()
    };
    let graph = running_task_graph("handle-1");
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();
    scheduler
        .event_sender()
        .try_send(TaskEvent {
            task_id: zeph_orchestration::TaskId(0),
            agent_handle_id: "handle-1".to_owned(),
            outcome: TaskOutcome::Completed {
                output: "task output".into(),
                artifacts: vec![],
            },
        })
        .unwrap();

    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (metrics_tx, metrics_rx) = watch::channel(MetricsSnapshot::default());
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_metrics(metrics_tx)
        .with_ensemble_members(vec![
            (
                "m1".to_owned(),
                AnyProvider::Mock(MockProvider::with_responses(vec![complete_json()])),
            ),
            (
                "m2".to_owned(),
                AnyProvider::Mock(MockProvider::with_responses(vec![complete_json()])),
            ),
            (
                "m3".to_owned(),
                AnyProvider::Mock(MockProvider::with_responses(vec![complete_json()])),
            ),
        ]);
    agent.services.orchestration.orchestration_config = config;

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(status, GraphStatus::Completed);
    let snapshot = metrics_rx.borrow().clone();
    assert_eq!(
        snapshot.orchestration.ensemble_degraded_total, 0,
        "full-quorum merge must not be counted as degraded"
    );
    assert_eq!(
        snapshot.orchestration.ensemble_last_agreement_ratio,
        Some(1.0),
        "unanimous 3-of-3 agreement must be recorded"
    );
    assert_eq!(snapshot.orchestration.ensemble_member_stats.len(), 3);
}

/// T4.6 / T5.6: below-quorum (2 of 3 members error) falls back to the single-provider path
/// and increments `ensemble_degraded_total` exactly once.
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn quorum_fallback_increments_degraded_counter_and_uses_single_provider_result() {
    let config = crate::config::OrchestrationConfig {
        ensemble: EnsembleConfig {
            enabled: true,
            verify: true,
            members: vec!["m1".into(), "m2".into(), "m3".into()],
            ..EnsembleConfig::default()
        },
        ..base_orchestration_config()
    };
    let graph = running_task_graph("handle-1");
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();
    scheduler
        .event_sender()
        .try_send(TaskEvent {
            task_id: zeph_orchestration::TaskId(0),
            agent_handle_id: "handle-1".to_owned(),
            outcome: TaskOutcome::Completed {
                output: "task output".into(),
                artifacts: vec![],
            },
        })
        .unwrap();

    // Primary provider is the single-provider fallback target (verify_provider unset).
    let provider = mock_provider(vec![complete_json()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (metrics_tx, metrics_rx) = watch::channel(MetricsSnapshot::default());
    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_metrics(metrics_tx)
        .with_ensemble_members(vec![
            (
                "m1".to_owned(),
                AnyProvider::Mock(MockProvider::with_responses(vec![complete_json()])),
            ),
            (
                "m2".to_owned(),
                AnyProvider::Mock(MockProvider::default().with_errors(vec![LlmError::Unavailable])),
            ),
            (
                "m3".to_owned(),
                AnyProvider::Mock(MockProvider::default().with_errors(vec![LlmError::Unavailable])),
            ),
        ]);
    agent.services.orchestration.orchestration_config = config;

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(status, GraphStatus::Completed);
    let snapshot = metrics_rx.borrow().clone();
    assert_eq!(
        snapshot.orchestration.ensemble_degraded_total, 1,
        "below-quorum (1 of 3 responders) must increment the degraded counter exactly once"
    );
    assert!(
        snapshot
            .orchestration
            .ensemble_last_agreement_ratio
            .is_none(),
        "a quorum-fallback round must not populate ensemble_last_agreement_ratio \
         (that field is only set on a successful merge)"
    );
}

/// S1 fix regression: a bootstrap-resolved `ensemble_members` set that is even-length or `< 3`
/// (simulating a partial resolution failure that shrank the *configured* odd/>=3 list down to
/// a structurally invalid *effective* count) must be rejected by the `effective_ensemble_valid`
/// gate in `scheduler_loop.rs` **before** `EnsembleVerifier::verify()` is ever constructed or
/// called — not merely fall back after dispatching to the shrunk set and losing quorum.
///
/// This is distinct from `quorum_fallback_increments_degraded_counter_and_uses_single_provider_result`,
/// which exercises the pre-existing runtime `EnsembleAttempt::QuorumNotMet` branch (all resolved
/// members are dispatched to, but too few respond). Both branches increment the same
/// `ensemble_degraded_total` counter, so the counter alone cannot distinguish them — proving
/// this test exercises the *new* S1 gate specifically requires proving the shrunk-set members
/// were never dispatched to at all, via `MockProvider::with_concurrency_tracking`'s peak-call
/// counter (monotonic: once incremented by a `chat()` call, it never resets to 0).
#[cfg(feature = "scheduler")]
#[tokio::test]
async fn bootstrap_shrunk_ensemble_falls_back_before_verify_dispatch() {
    use std::sync::atomic::Ordering;

    let config = crate::config::OrchestrationConfig {
        ensemble: EnsembleConfig {
            enabled: true,
            verify: true,
            // Configured list is odd/>=3 (would pass load-time validation); the *resolved*
            // set below is deliberately shrunk to 2 (even), simulating one member failing to
            // resolve at bootstrap (critic S1).
            members: vec!["m1".into(), "m2".into(), "m3".into()],
            ..EnsembleConfig::default()
        },
        ..base_orchestration_config()
    };
    let graph = running_task_graph("handle-1");
    let mut scheduler =
        DagScheduler::resume_from(graph, &config, Box::new(RuleBasedRouter), vec![], None).unwrap();
    scheduler
        .event_sender()
        .try_send(TaskEvent {
            task_id: zeph_orchestration::TaskId(0),
            agent_handle_id: "handle-1".to_owned(),
            outcome: TaskOutcome::Completed {
                output: "task output".into(),
                artifacts: vec![],
            },
        })
        .unwrap();

    // Primary provider is the single-provider fallback target (verify_provider unset).
    let provider = mock_provider(vec![complete_json()]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let (metrics_tx, metrics_rx) = watch::channel(MetricsSnapshot::default());

    let (m1_provider, m1_peak) =
        MockProvider::with_responses(vec![complete_json()]).with_concurrency_tracking();
    let (m2_provider, m2_peak) =
        MockProvider::with_responses(vec![complete_json()]).with_concurrency_tracking();

    let mut agent = crate::agent::Agent::new(provider, channel, registry, None, 5, executor)
        .with_metrics(metrics_tx)
        // Only 2 members resolved (even, < the configured 3) — the bootstrap-shrunk effective
        // ensemble the S1 fix must reject before any dispatch.
        .with_ensemble_members(vec![
            ("m1".to_owned(), AnyProvider::Mock(m1_provider)),
            ("m2".to_owned(), AnyProvider::Mock(m2_provider)),
        ]);
    agent.services.orchestration.orchestration_config = config;

    let token = tokio_util::sync::CancellationToken::new();
    let status = agent
        .run_scheduler_loop(&mut scheduler, 1, token)
        .await
        .unwrap();

    assert_eq!(status, GraphStatus::Completed);
    let snapshot = metrics_rx.borrow().clone();
    assert_eq!(
        snapshot.orchestration.ensemble_degraded_total, 1,
        "an even-length (2) effective ensemble must trigger the S1 shrinkage-guard fallback"
    );
    assert!(
        snapshot
            .orchestration
            .ensemble_last_agreement_ratio
            .is_none()
    );
    assert_eq!(
        m1_peak.load(Ordering::SeqCst),
        0,
        "member m1 must never be dispatched to — the S1 gate rejects the shrunk effective \
         ensemble before EnsembleVerifier::verify() is ever called"
    );
    assert_eq!(
        m2_peak.load(Ordering::SeqCst),
        0,
        "member m2 must never be dispatched to — the S1 gate rejects the shrunk effective \
         ensemble before EnsembleVerifier::verify() is ever called"
    );
}
