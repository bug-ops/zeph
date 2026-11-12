// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use crate::graph::retrieval::find_seed_entities;
use crate::graph::{EntityType, GraphStore};

use super::super::*;
use super::test_semantic_memory;

async fn graph_memory() -> (SemanticMemory, Arc<GraphStore>) {
    let mem = test_semantic_memory(false).await;
    let store = Arc::new(GraphStore::new(mem.sqlite.pool().clone()));
    (mem.with_graph_store(Arc::clone(&store)), store)
}

#[tokio::test]
async fn resolve_returns_none_when_graph_disabled() {
    let mem = test_semantic_memory(false).await;
    assert_eq!(mem.resolve_goal_anchor("ship invoice").await.unwrap(), None);
}

#[tokio::test]
async fn resolve_returns_none_without_matching_entity() {
    let (mem, _) = graph_memory().await;
    assert_eq!(mem.resolve_goal_anchor("ship invoice").await.unwrap(), None);
}

#[tokio::test]
async fn resolve_prefers_named_entity_over_high_degree_hub() {
    let (mem, store) = graph_memory().await;
    let goal = store
        .upsert_entity("invoice", "invoice", EntityType::Concept, None, None)
        .await
        .unwrap();
    let hub = store
        .upsert_entity(
            "invoice ledger",
            "invoice ledger",
            EntityType::Concept,
            None,
            None,
        )
        .await
        .unwrap();
    for i in 0..30 {
        let leaf = store
            .upsert_entity(
                &format!("leaf{i}"),
                &format!("leaf{i}"),
                EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap();
        store
            .insert_edge(hub, leaf, "links", "hub links leaf", 1.0, None, None)
            .await
            .unwrap();
    }

    // Fixture validity: with the default structural weight the hub would win the same query.
    let biased = find_seed_entities(
        &store,
        None,
        &mem.provider,
        "invoice",
        5,
        0.4,
        1,
        mem.embed_timeout,
    )
    .await
    .unwrap();
    let biased_top = biased
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id);
    assert_eq!(
        biased_top,
        Some(hub),
        "fixture must make the hub win at 0.4"
    );

    assert_eq!(
        mem.resolve_goal_anchor("invoice").await.unwrap(),
        Some(goal)
    );
}

#[tokio::test]
async fn episode_active_index_backs_the_production_lookup_query() {
    let (_, store) = graph_memory().await;
    let sql = format!(
        "EXPLAIN QUERY PLAN {}",
        crate::graph::store::entity_ids_for_messages_sql(2)
    );
    let plan: Vec<(i64, i64, i64, String)> = zeph_db::query_as(sqlx::AssertSqlSafe(sql))
        .bind(1_i64)
        .bind(2_i64)
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert!(
        plan.iter()
            .any(|row| row.3.contains("idx_graph_edges_episode_active")),
        "episode lookup must use the partial index, plan: {plan:?}"
    );
}
