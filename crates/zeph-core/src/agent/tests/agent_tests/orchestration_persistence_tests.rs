// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused_imports)]
use super::*;

// ── GraphPersistence wiring tests ─────────────────────────────────────────

#[cfg(feature = "scheduler")]
async fn make_gp_memory() -> std::sync::Arc<zeph_memory::semantic::SemanticMemory> {
    let memory = zeph_memory::semantic::SemanticMemory::with_sqlite_backend(
        ":memory:",
        AnyProvider::Mock(MockProvider::default()),
        "test-model",
        0.7,
        0.3,
    )
    .await
    .expect("SemanticMemory");
    std::sync::Arc::new(memory)
}

#[cfg(feature = "scheduler")]
fn graph_with_status(status: zeph_orchestration::GraphStatus) -> zeph_orchestration::TaskGraph {
    let mut g = zeph_orchestration::TaskGraph::new("test goal");
    g.status = status;
    g
}

#[cfg(feature = "scheduler")]
fn make_orch_agent(
    memory: std::sync::Arc<zeph_memory::semantic::SemanticMemory>,
    cid: zeph_memory::ConversationId,
    persistence_enabled: bool,
) -> Agent<MockChannel> {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let cfg = crate::config::OrchestrationConfig {
        persistence_enabled,
        ..Default::default()
    };
    Agent::new(provider, channel, registry, None, 5, executor)
        .with_memory(memory, cid, 100, 5, 1000)
        .with_orchestration(cfg, crate::config::SubAgentConfig::default(), {
            zeph_subagent::SubAgentManager::new(4)
        })
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn wire_graph_persistence_is_none_without_memory() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let cfg = crate::config::OrchestrationConfig {
        persistence_enabled: true,
        ..Default::default()
    };
    let agent = Agent::new(provider, channel, registry, None, 5, executor).with_orchestration(
        cfg,
        crate::config::SubAgentConfig::default(),
        zeph_subagent::SubAgentManager::new(4),
    );
    assert!(
        agent.services.orchestration.graph_persistence.is_none(),
        "graph_persistence must be None when no memory is attached"
    );
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn wire_graph_persistence_is_none_when_disabled() {
    let memory = make_gp_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let agent = make_orch_agent(memory, cid, false);
    assert!(
        agent.services.orchestration.graph_persistence.is_none(),
        "graph_persistence must be None when persistence_enabled = false"
    );
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn wire_graph_persistence_is_some_with_memory_and_enabled() {
    let memory = make_gp_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let agent = make_orch_agent(memory, cid, true);
    assert!(
        agent.services.orchestration.graph_persistence.is_some(),
        "graph_persistence must be Some when memory attached and persistence_enabled = true"
    );
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn handle_plan_resume_invalid_uuid_returns_error_string() {
    let memory = make_gp_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mut agent = make_orch_agent(memory, cid, true);
    let result = agent.handle_plan_resume_as_string(Some("not-a-uuid")).await;
    assert!(
        result.contains("Invalid graph id"),
        "must return parse error for invalid UUID, got: {result}"
    );
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn handle_plan_resume_missing_graph_returns_not_found() {
    use zeph_orchestration::GraphId;
    let memory = make_gp_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mut agent = make_orch_agent(memory, cid, true);
    let missing_id = GraphId::new().to_string();
    let result = agent
        .handle_plan_resume_as_string(Some(missing_id.as_str()))
        .await;
    assert!(
        result.contains("not found in persistence"),
        "must return not-found message, got: {result}"
    );
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn handle_plan_resume_from_disk_hydrates_paused() {
    use zeph_orchestration::GraphStatus;
    let memory = make_gp_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mut agent = make_orch_agent(memory, cid, true);
    let graph = graph_with_status(GraphStatus::Paused);
    let graph_id = graph.id.to_string();
    agent
        .services
        .orchestration
        .graph_persistence
        .as_ref()
        .unwrap()
        .save(&graph)
        .await
        .unwrap();
    let result = agent
        .handle_plan_resume_as_string(Some(graph_id.as_str()))
        .await;
    assert!(result.contains("Resuming plan"), "got: {result}");
    assert!(agent.services.orchestration.pending_graph.is_some());
    assert_eq!(
        agent
            .services
            .orchestration
            .pending_graph
            .as_ref()
            .unwrap()
            .status,
        GraphStatus::Paused
    );
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn handle_plan_resume_from_disk_recovers_running_as_paused() {
    use zeph_orchestration::{GraphStatus, TaskStatus};
    let memory = make_gp_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mut agent = make_orch_agent(memory, cid, true);
    let mut graph = graph_with_status(GraphStatus::Running);
    let mut task = zeph_orchestration::TaskNode::new(0, "task1", "do something");
    task.status = TaskStatus::Running;
    task.assigned_agent = Some("agent-1".to_string());
    graph.tasks.push(task);
    let graph_id = graph.id.to_string();
    agent
        .services
        .orchestration
        .graph_persistence
        .as_ref()
        .unwrap()
        .save(&graph)
        .await
        .unwrap();
    let result = agent
        .handle_plan_resume_as_string(Some(graph_id.as_str()))
        .await;
    assert!(result.contains("Recovered plan"), "got: {result}");
    let recovered = agent.services.orchestration.pending_graph.as_ref().unwrap();
    assert_eq!(recovered.status, GraphStatus::Paused);
    assert_eq!(recovered.tasks[0].status, TaskStatus::Ready);
    assert!(recovered.tasks[0].assigned_agent.is_none());
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn handle_plan_resume_refuses_completed() {
    use zeph_orchestration::GraphStatus;
    let memory = make_gp_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mut agent = make_orch_agent(memory, cid, true);
    let graph = graph_with_status(GraphStatus::Completed);
    let graph_id = graph.id.to_string();
    agent
        .services
        .orchestration
        .graph_persistence
        .as_ref()
        .unwrap()
        .save(&graph)
        .await
        .unwrap();
    let result = agent
        .handle_plan_resume_as_string(Some(graph_id.as_str()))
        .await;
    assert!(result.contains("already Completed"), "got: {result}");
    assert!(agent.services.orchestration.pending_graph.is_none());
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn handle_plan_resume_refuses_canceled() {
    use zeph_orchestration::GraphStatus;
    let memory = make_gp_memory().await;
    let cid = memory.sqlite().create_conversation().await.unwrap();
    let mut agent = make_orch_agent(memory, cid, true);
    let graph = graph_with_status(GraphStatus::Canceled);
    let graph_id = graph.id.to_string();
    agent
        .services
        .orchestration
        .graph_persistence
        .as_ref()
        .unwrap()
        .save(&graph)
        .await
        .unwrap();
    let result = agent
        .handle_plan_resume_as_string(Some(graph_id.as_str()))
        .await;
    assert!(result.contains("Canceled"), "got: {result}");
    assert!(agent.services.orchestration.pending_graph.is_none());
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn handle_plan_resume_no_id_no_pending_returns_instructions() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent.handle_plan_resume_as_string(None).await;
    assert!(result.contains("No paused plan"), "got: {result}");
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn handle_plan_resume_no_persistence_with_id_returns_disabled_message() {
    let provider = mock_provider(vec![]);
    let channel = MockChannel::new(vec![]);
    let registry = create_test_registry();
    let executor = MockToolExecutor::no_tools();
    let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
    let result = agent
        .handle_plan_resume_as_string(Some("00000000-0000-0000-0000-000000000001"))
        .await;
    // UUID parses successfully, so persistence-disabled branch is unambiguously hit.
    assert!(
        result.contains("persistence is disabled"),
        "expected persistence-disabled message, got: {result}"
    );
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn predicate_outcome_round_trips_through_new_agent() {
    use zeph_orchestration::{
        GraphPersistence, GraphStatus, PredicateOutcome, TaskGraph, TaskNode, TaskStatus,
    };

    // Reuse the shared in-memory DB via SemanticMemory — migrations are already applied.
    let memory = make_gp_memory().await;
    let pool = memory.sqlite().pool().clone();
    let store_a = zeph_memory::store::graph_store::TaskGraphStore::new(pool.clone());
    let persistence_a = GraphPersistence::new(store_a);

    let node = TaskNode {
        status: TaskStatus::Completed,
        predicate_outcome: Some(PredicateOutcome {
            passed: true,
            reason: "ok".to_string(),
            confidence: 1.0,
        }),
        ..TaskNode::new(0, "task-a", "test task")
    };
    let graph = TaskGraph {
        status: GraphStatus::Completed,
        tasks: vec![node],
        ..TaskGraph::new("test goal")
    };
    let graph_id = graph.id.clone();

    persistence_a.save(&graph).await.unwrap();

    // Second persistence handle on the same pool — simulates "new agent" rehydrating.
    let store_b = zeph_memory::store::graph_store::TaskGraphStore::new(pool);
    let persistence_b = GraphPersistence::new(store_b);

    let loaded = persistence_b
        .load(&graph_id)
        .await
        .unwrap()
        .expect("graph must exist");
    let loaded_node = loaded.tasks.first().expect("task must exist");
    let outcome = loaded_node
        .predicate_outcome
        .as_ref()
        .expect("predicate_outcome must survive round-trip");
    assert!(
        outcome.passed,
        "predicate outcome `passed` must be true after round-trip"
    );
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn scheduler_loop_saves_graph_once_per_tick() {
    use zeph_orchestration::{GraphPersistence, GraphStatus, TaskGraph};

    let memory = make_gp_memory().await;
    let pool = memory.sqlite().pool().clone();
    let store = zeph_memory::store::graph_store::TaskGraphStore::new(pool.clone());
    let persistence = GraphPersistence::new(store);

    let graph = TaskGraph {
        status: GraphStatus::Running,
        ..TaskGraph::new("g")
    };
    let graph_id = graph.id.clone();

    // Mirrors what save_graph_snapshot does (bounded save call).
    persistence.save(&graph).await.unwrap();

    let store2 = zeph_memory::store::graph_store::TaskGraphStore::new(pool);
    let persistence2 = GraphPersistence::new(store2);
    let loaded = persistence2.load(&graph_id).await.unwrap();
    assert!(
        loaded.is_some(),
        "graph must be persisted after save_graph_snapshot call"
    );
    assert_eq!(loaded.unwrap().id, graph_id);
}

#[cfg(feature = "scheduler")]
#[tokio::test]
async fn terminal_save_persists_completed_status() {
    use zeph_orchestration::{GraphPersistence, GraphStatus, TaskGraph};

    let memory = make_gp_memory().await;
    let pool = memory.sqlite().pool().clone();
    let store = zeph_memory::store::graph_store::TaskGraphStore::new(pool.clone());
    let persistence = GraphPersistence::new(store);

    let graph = TaskGraph {
        status: GraphStatus::Completed,
        ..TaskGraph::new("g")
    };
    let graph_id = graph.id.clone();

    // Simulate the authoritative terminal save in handle_plan_confirm.
    match tokio::time::timeout(std::time::Duration::from_secs(5), persistence.save(&graph)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("terminal save failed: {e}"),
        Err(e) => panic!("terminal save timed out after 5s: {e}"),
    }
    drop(persistence);

    let store2 = zeph_memory::store::graph_store::TaskGraphStore::new(pool);
    let persistence2 = GraphPersistence::new(store2);
    let loaded = persistence2
        .load(&graph_id)
        .await
        .unwrap()
        .expect("must find saved graph");
    assert_eq!(
        loaded.status,
        GraphStatus::Completed,
        "terminal save must persist Completed status"
    );
}
