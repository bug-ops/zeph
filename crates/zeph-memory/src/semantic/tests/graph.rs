// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::graph::{EntityType, GraphStore};

use super::super::*;
use super::test_provider;
use super::test_semantic_memory;

async fn graph_memory() -> SemanticMemory {
    let mem = test_semantic_memory(false).await;
    let store = std::sync::Arc::new(GraphStore::new(mem.sqlite.pool().clone()));
    mem.with_graph_store(store)
}

#[tokio::test]
async fn recall_graph_returns_empty_when_no_entities() {
    let memory = graph_memory().await;
    let facts = memory.recall_graph("rust", 10, 2).await.unwrap();
    assert!(facts.is_empty(), "empty graph must return empty vec");
}

#[tokio::test]
async fn recall_graph_returns_facts_for_known_entity() {
    let memory = graph_memory().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    let rust_id = store
        .upsert_entity("rust", "rust", EntityType::Language, Some("a language"))
        .await
        .unwrap();
    let tokio_id = store
        .upsert_entity("tokio", "tokio", EntityType::Tool, Some("async runtime"))
        .await
        .unwrap();
    store
        .insert_edge(
            rust_id,
            tokio_id,
            "uses",
            "Rust uses tokio for async",
            0.9,
            None,
        )
        .await
        .unwrap();

    let facts = memory.recall_graph("rust", 10, 2).await.unwrap();
    assert!(!facts.is_empty(), "should return at least one fact");
    assert_eq!(facts[0].entity_name, "rust");
    assert_eq!(facts[0].relation, "uses");
}

#[tokio::test]
async fn recall_graph_sorted_by_composite_score() {
    let memory = graph_memory().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    let a_id = store
        .upsert_entity("entity_a", "entity_a", EntityType::Concept, None)
        .await
        .unwrap();
    let b_id = store
        .upsert_entity("entity_b", "entity_b", EntityType::Concept, None)
        .await
        .unwrap();
    let c_id = store
        .upsert_entity("entity_c", "entity_c", EntityType::Concept, None)
        .await
        .unwrap();
    store
        .insert_edge(a_id, b_id, "relates", "a relates b", 0.9, None)
        .await
        .unwrap();
    store
        .insert_edge(a_id, c_id, "relates", "a relates c", 0.5, None)
        .await
        .unwrap();

    let facts = memory.recall_graph("entity_a", 10, 1).await.unwrap();
    if facts.len() >= 2 {
        assert!(
            facts[0].composite_score() >= facts[1].composite_score(),
            "facts must be sorted descending by composite score"
        );
    }
}

#[tokio::test]
async fn extract_and_store_returns_zero_stats_for_empty_content() {
    let memory = graph_memory().await;
    let pool = memory.sqlite.pool().clone();
    let provider = test_provider();

    let stats = extract_and_store(
        String::new(),
        vec![],
        provider,
        pool,
        GraphExtractionConfig {
            max_entities: 10,
            max_edges: 10,
            extraction_timeout_secs: 5,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(stats.entities_upserted, 0);
    assert_eq!(stats.edges_inserted, 0);
}

#[tokio::test]
async fn extraction_count_increments_atomically() {
    let memory = graph_memory().await;
    let pool = memory.sqlite.pool().clone();
    let provider = test_provider();

    for _ in 0..2 {
        let _ = extract_and_store(
            "I use Rust for systems programming".to_owned(),
            vec![],
            provider.clone(),
            pool.clone(),
            GraphExtractionConfig {
                max_entities: 5,
                max_edges: 5,
                extraction_timeout_secs: 5,
                ..Default::default()
            },
        )
        .await;
    }

    let store = GraphStore::new(pool);
    let count = store.get_metadata("extraction_count").await.unwrap();
    assert_eq!(
        count.as_deref(),
        Some("2"),
        "extraction_count must be exactly 2 after two extraction attempts"
    );
}

#[tokio::test]
async fn recall_graph_truncates_to_limit() {
    let memory = graph_memory().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    let root_id = store
        .upsert_entity("root", "root", EntityType::Concept, None)
        .await
        .unwrap();
    for i in 0..5 {
        let name = format!("target_{i}");
        let tid = store
            .upsert_entity(&name, &name, EntityType::Concept, None)
            .await
            .unwrap();
        store
            .insert_edge(
                root_id,
                tid,
                "links",
                &format!("root links {name}"),
                0.7,
                None,
            )
            .await
            .unwrap();
    }

    let facts = memory.recall_graph("root", 3, 1).await.unwrap();
    assert!(facts.len() <= 3, "recall_graph must respect limit");
}

#[tokio::test]
async fn recall_graph_multi_hop_traverses_two_hops() {
    let memory = graph_memory().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    let a_id = store
        .upsert_entity("a_entity", "a_entity", EntityType::Person, None)
        .await
        .unwrap();
    let b_id = store
        .upsert_entity("b_entity", "b_entity", EntityType::Person, None)
        .await
        .unwrap();
    let c_id = store
        .upsert_entity("c_entity", "c_entity", EntityType::Concept, None)
        .await
        .unwrap();

    store
        .insert_edge(a_id, b_id, "knows", "a knows b", 0.9, None)
        .await
        .unwrap();
    store
        .insert_edge(b_id, c_id, "uses", "b uses c", 0.8, None)
        .await
        .unwrap();

    let facts_1hop = memory.recall_graph("a_entity", 10, 1).await.unwrap();
    assert!(!facts_1hop.is_empty(), "hop=1 must find direct edge");

    let facts_2hop = memory.recall_graph("a_entity", 10, 2).await.unwrap();
    assert!(
        facts_2hop.len() >= facts_1hop.len(),
        "hop=2 must find at least as many facts as hop=1"
    );
    let has_bc = facts_2hop.iter().any(|f| {
        (f.entity_name.contains("b_entity") || f.target_name.contains("b_entity"))
            && (f.entity_name.contains("c_entity") || f.target_name.contains("c_entity"))
    });
    assert!(has_bc, "hop=2 BFS must traverse to c_entity via b_entity");
}

#[tokio::test]
async fn spawn_graph_extraction_zero_timeout_returns_without_panic() {
    let memory = graph_memory().await;
    let cfg = GraphExtractionConfig {
        max_entities: 5,
        max_edges: 5,
        extraction_timeout_secs: 0,
        ..Default::default()
    };
    memory.spawn_graph_extraction("I use Rust for systems programming".to_owned(), vec![], cfg);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}
