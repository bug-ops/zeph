// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for HL-F5 spreading activation (`hela_spreading_recall`).
//!
//! These tests require a live Qdrant instance (started via `testcontainers`). Run with:
//!
//! ```bash
//! cargo nextest run -p zeph-memory --test hela_spreading_activation -- --ignored
//! ```

use serde_json::json;
use testcontainers::ContainerAsync;
use testcontainers::GenericImage;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use zeph_llm::any::AnyProvider;
use zeph_llm::mock::MockProvider;
use zeph_memory::embedding_store::EmbeddingStore;
use zeph_memory::graph::GraphStore;
use zeph_memory::graph::activation::{HelaSpreadParams, hela_spreading_recall};
use zeph_memory::graph::types::EntityType;
use zeph_memory::store::SqliteStore;

const QDRANT_GRPC_PORT: ContainerPort = ContainerPort::Tcp(6334);
const ENTITY_COLLECTION: &str = "zeph_graph_entities";

fn qdrant_image() -> GenericImage {
    GenericImage::new("qdrant/qdrant", "v1.16.0")
        .with_wait_for(WaitFor::message_on_stdout("gRPC listening"))
        .with_wait_for(WaitFor::seconds(1))
        .with_exposed_port(QDRANT_GRPC_PORT)
}

async fn setup() -> (
    GraphStore,
    EmbeddingStore,
    SqliteStore,
    ContainerAsync<GenericImage>,
) {
    let container = qdrant_image().start().await.unwrap();
    let grpc_port = container.get_host_port_ipv4(6334).await.unwrap();
    let url = format!("http://127.0.0.1:{grpc_port}");

    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let pool = sqlite.pool().clone();
    let embeddings = EmbeddingStore::new(&url, pool.clone()).unwrap();
    let graph = GraphStore::new(pool);
    (graph, embeddings, sqlite, container)
}

/// Seed a named entity into both SQLite graph and Qdrant entity collection.
///
/// Returns `(entity_id, point_id)`. The `point_id` is set in `qdrant_point_id` so that
/// `qdrant_point_ids_for_entities` can find it.
async fn seed_entity(
    graph: &GraphStore,
    embeddings: &EmbeddingStore,
    name: &str,
    vector: Vec<f32>,
) -> (i64, String) {
    let entity_id = graph
        .upsert_entity(name, name, EntityType::Concept, None)
        .await
        .unwrap();

    let point_id = uuid::Uuid::new_v4().to_string();
    let payload = json!({
        "entity_id": entity_id,
        "entity_type": "concept",
        "name": name,
        "summary": "",
    });

    embeddings
        .upsert_to_collection(ENTITY_COLLECTION, &point_id, payload, vector)
        .await
        .unwrap();

    // Write `qdrant_point_id` so `qdrant_point_ids_for_entities` can find this entity.
    let pool = graph.pool();
    sqlx::query("UPDATE graph_entities SET qdrant_point_id = ?1 WHERE id = ?2")
        .bind(&point_id)
        .bind(entity_id)
        .execute(pool)
        .await
        .unwrap();

    (entity_id, point_id)
}

fn mock_provider(embed: Vec<f32>) -> AnyProvider {
    AnyProvider::Mock(MockProvider::default().with_embedding(embed))
}

/// `hela_spreading_recall` returns empty when the entity collection is empty.
#[ignore = "requires Qdrant container"]
#[tokio::test]
async fn hela_empty_graph_empty_result() {
    let (graph, embeddings, _sqlite, _ctr) = setup().await;
    let provider = mock_provider(vec![1.0, 0.0, 0.0]);

    // Ensure the collection exists so we don't get a Qdrant "not found" error.
    embeddings
        .ensure_named_collection(ENTITY_COLLECTION, 3)
        .await
        .unwrap();

    let results = hela_spreading_recall(
        &graph,
        &embeddings,
        &provider,
        "unknown concept",
        10,
        &HelaSpreadParams {
            spread_depth: 2,
            ..Default::default()
        },
        false,
        0.1,
    )
    .await
    .unwrap();

    assert!(results.is_empty(), "no entities → empty results");
}

/// Isolated anchor (no edges) falls back to a single synthetic fact.
#[ignore = "requires Qdrant container"]
#[tokio::test]
async fn hela_anchor_isolated_fallback() {
    let (graph, embeddings, _sqlite, _ctr) = setup().await;
    let embed_vec = vec![1.0_f32, 0.0, 0.0];
    let provider = mock_provider(embed_vec.clone());

    embeddings
        .ensure_named_collection(ENTITY_COLLECTION, 3)
        .await
        .unwrap();

    let (_entity_id, _point_id) = seed_entity(&graph, &embeddings, "Isolation", embed_vec).await;

    let results = hela_spreading_recall(
        &graph,
        &embeddings,
        &provider,
        "isolation",
        5,
        &HelaSpreadParams {
            spread_depth: 2,
            ..Default::default()
        },
        false,
        0.1,
    )
    .await
    .unwrap();

    assert_eq!(
        results.len(),
        1,
        "isolated anchor → exactly one synthetic fact"
    );
    assert_eq!(results[0].edge.id, 0, "synthetic anchor edge id must be 0");
    assert!(results[0].score > 0.0, "anchor cosine must be positive");
}

/// `spread_depth=1` does not return 2-hop neighbours.
#[ignore = "requires Qdrant container"]
#[tokio::test]
async fn hela_depth_one_excludes_two_hop() {
    let (graph, embeddings, _sqlite, _ctr) = setup().await;
    let embed_vec = vec![1.0_f32, 0.0, 0.0];
    let provider = mock_provider(embed_vec.clone());

    embeddings
        .ensure_named_collection(ENTITY_COLLECTION, 3)
        .await
        .unwrap();

    let (anchor_id, _) = seed_entity(&graph, &embeddings, "A", embed_vec.clone()).await;
    let (hop1_id, _) = seed_entity(&graph, &embeddings, "B", embed_vec.clone()).await;
    let (hop2_id, _) = seed_entity(&graph, &embeddings, "C", embed_vec.clone()).await;

    graph
        .insert_edge(anchor_id, hop1_id, "relates_to", "test edge", 0.9, None)
        .await
        .unwrap();
    graph
        .insert_edge(hop1_id, hop2_id, "relates_to", "test edge", 0.9, None)
        .await
        .unwrap();

    let params = HelaSpreadParams {
        spread_depth: 1,
        ..Default::default()
    };
    let results =
        hela_spreading_recall(&graph, &embeddings, &provider, "a", 20, &params, false, 0.1)
            .await
            .unwrap();

    // At depth 1 only hop1 (B) is reachable. C must not appear.
    for fact in &results {
        let src = fact.edge.source_entity_id;
        let tgt = fact.edge.target_entity_id;
        assert_ne!(
            (src == hop2_id) || (tgt == hop2_id),
            true,
            "C must not appear in depth-1 results"
        );
    }
}

/// `max_visited` cap is respected — results never exceed the cap.
#[ignore = "requires Qdrant container"]
#[tokio::test]
async fn hela_respects_max_visited() {
    let (graph, embeddings, _sqlite, _ctr) = setup().await;
    let embed_vec = vec![1.0_f32, 0.0, 0.0];
    let provider = mock_provider(embed_vec.clone());

    embeddings
        .ensure_named_collection(ENTITY_COLLECTION, 3)
        .await
        .unwrap();

    let (hub_id, _) = seed_entity(&graph, &embeddings, "Hub", embed_vec.clone()).await;
    for i in 0..15i64 {
        let (nid, _) = seed_entity(&graph, &embeddings, &format!("N{i}"), embed_vec.clone()).await;
        graph
            .insert_edge(hub_id, nid, "relates_to", "test edge", 0.8, None)
            .await
            .unwrap();
    }

    let cap = 5;
    let params = HelaSpreadParams {
        spread_depth: 2,
        max_visited: cap,
        ..Default::default()
    };
    let results = hela_spreading_recall(
        &graph,
        &embeddings,
        &provider,
        "hub",
        100,
        &params,
        false,
        0.1,
    )
    .await
    .unwrap();

    assert!(
        results.len() <= cap,
        "max_visited={cap} must cap results, got {}",
        results.len()
    );
}
