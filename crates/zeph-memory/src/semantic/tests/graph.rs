// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[allow(unused_imports)]
use zeph_db::sql;
use zeph_llm::any::AnyProvider;

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
    let facts = memory
        .recall_graph("rust", 10, 2, None, 0.0, &[])
        .await
        .unwrap();
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

    let facts = memory
        .recall_graph("rust", 10, 2, None, 0.0, &[])
        .await
        .unwrap();
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

    let facts = memory
        .recall_graph("entity_a", 10, 1, None, 0.0, &[])
        .await
        .unwrap();
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

    let result = extract_and_store(
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
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.stats.entities_upserted, 0);
    assert_eq!(result.stats.edges_inserted, 0);
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
            None,
            None,
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

    let facts = memory
        .recall_graph("root", 3, 1, None, 0.0, &[])
        .await
        .unwrap();
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

    let facts_1hop = memory
        .recall_graph("a_entity", 10, 1, None, 0.0, &[])
        .await
        .unwrap();
    assert!(!facts_1hop.is_empty(), "hop=1 must find direct edge");

    let facts_2hop = memory
        .recall_graph("a_entity", 10, 2, None, 0.0, &[])
        .await
        .unwrap();
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
    memory.spawn_graph_extraction(
        "I use Rust for systems programming".to_owned(),
        vec![],
        cfg,
        None,
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

// ── NoteLinkingConfig tests ────────────────────────────────────────────────

#[test]
fn note_linking_config_defaults() {
    let cfg = NoteLinkingConfig::default();
    assert!(!cfg.enabled);
    assert!((cfg.similarity_threshold - 0.85_f32).abs() < f32::EPSILON);
    assert_eq!(cfg.top_k, 10);
    assert_eq!(cfg.timeout_secs, 5);
}

// ── link_memory_notes tests ───────────────────────────────────────────────
//
// MockProvider returns vec![0.0; 384] for embed(). We store entities with zero vectors
// so the search query matches the stored vectors. Threshold is set to 0.0 to ensure any
// non-NaN score passes, except where we test dissimilar entities.

async fn memory_with_in_memory_vector_store() -> (
    SemanticMemory,
    std::sync::Arc<crate::embedding_store::EmbeddingStore>,
) {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use zeph_llm::mock::MockProvider;

    use crate::embedding_store::EmbeddingStore;
    use crate::in_memory_store::InMemoryVectorStore;
    use crate::store::SqliteStore;
    use crate::token_counter::TokenCounter;

    let sqlite = SqliteStore::new(":memory:").await.unwrap();
    let pool = sqlite.pool().clone();
    let mem_store = Box::new(InMemoryVectorStore::new());
    let embedding_store = Arc::new(EmbeddingStore::with_store(mem_store, pool));

    // Ensure the entity collection exists (384-dimensional to match MockProvider output).
    embedding_store
        .ensure_named_collection("zeph_graph_entities", 384)
        .await
        .unwrap();

    // MockProvider with embeddings enabled returns vec![0.0; 384].
    let mut mock = MockProvider::default();
    mock.supports_embeddings = true;
    let provider = AnyProvider::Mock(mock);

    let memory = SemanticMemory {
        sqlite,
        qdrant: Some(embedding_store.clone()),
        provider,
        embed_provider: None,
        embedding_model: "test-model".into(),
        vector_weight: 0.7,
        keyword_weight: 0.3,
        temporal_decay_enabled: false,
        temporal_decay_half_life_days: 30,
        mmr_enabled: false,
        mmr_lambda: 0.7,
        importance_enabled: false,
        importance_weight: 0.15,
        token_counter: std::sync::Arc::new(TokenCounter::new()),
        graph_store: None,
        experience: None,
        community_detection_failures: Arc::new(AtomicU64::new(0)),
        graph_extraction_count: Arc::new(AtomicU64::new(0)),
        graph_extraction_failures: Arc::new(AtomicU64::new(0)),
        last_qdrant_warn: Arc::new(AtomicU64::new(0)),
        tier_boost_semantic: 1.3,
        admission_control: None,
        quality_gate: None,
        key_facts_dedup_threshold: 0.95,
        embed_tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
        retrieval_depth: 0,
        search_prompt_template: String::new(),
        depth_below_limit_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        missing_placeholder_warned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    (memory, embedding_store)
}

/// Seed an entity into `SQLite` + the entity embedding collection with a zero vector
/// (matching `MockProvider`'s `embed()` output).
async fn seed_entity_with_zero_embedding(
    store: &GraphStore,
    embedding_store: &crate::embedding_store::EmbeddingStore,
    name: &str,
) -> i64 {
    use serde_json::json;

    let id = store
        .upsert_entity(name, name, EntityType::Concept, None)
        .await
        .unwrap();

    let point_id = uuid::Uuid::new_v4().to_string();
    let payload = json!({
        "entity_id": id,
        "entity_type": "concept",
        "name": name,
        "summary": "",
    });
    // Zero vector matches MockProvider embed output exactly, giving cosine ~0/undefined.
    // InMemoryVectorStore returns score = 1.0 for identical zero vectors (cosine of 0 vs 0 = 1.0).
    embedding_store
        .upsert_to_collection(
            "zeph_graph_entities",
            &point_id,
            payload,
            vec![0.0_f32; 384],
        )
        .await
        .unwrap();

    // Write qdrant_point_id back to graph_entities so self-exclusion works.
    let pool = store.pool();
    zeph_db::query(sql!(
        "UPDATE graph_entities SET qdrant_point_id = ?1 WHERE id = ?2"
    ))
    .bind(&point_id)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();

    id
}

/// Seed an entity with a zero embedding but WITHOUT writing `qdrant_point_id` back to `SQLite`.
///
/// Used to exercise the secondary `target_id == entity_id` guard — when `qdrant_point_id` is NULL
/// in the DB the primary point-id comparison cannot exclude the self-result, so the secondary
/// guard must catch it.
async fn seed_entity_no_db_point_id(
    store: &GraphStore,
    embedding_store: &crate::embedding_store::EmbeddingStore,
    name: &str,
) -> i64 {
    use serde_json::json;

    let id = store
        .upsert_entity(name, name, EntityType::Concept, None)
        .await
        .unwrap();

    let point_id = uuid::Uuid::new_v4().to_string();
    let payload = json!({
        "entity_id": id,
        "entity_type": "concept",
        "name": name,
        "summary": "",
    });
    embedding_store
        .upsert_to_collection(
            "zeph_graph_entities",
            &point_id,
            payload,
            vec![0.0_f32; 384],
        )
        .await
        .unwrap();

    // Intentionally NOT writing qdrant_point_id to graph_entities.
    // The DB row keeps qdrant_point_id = NULL so the primary self-exclusion guard is inactive.
    id
}

fn embedding_provider() -> AnyProvider {
    use zeph_llm::mock::MockProvider;
    let mut mock = MockProvider::default();
    mock.supports_embeddings = true;
    AnyProvider::Mock(mock)
}

#[tokio::test]
async fn link_memory_notes_skips_self() {
    let (memory, embedding_store) = memory_with_in_memory_vector_store().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    // Single entity — only self will be returned from search.
    let id = seed_entity_with_zero_embedding(&store, &embedding_store, "solo_entity").await;

    let cfg = NoteLinkingConfig {
        enabled: true,
        similarity_threshold: 0.0,
        top_k: 5,
        timeout_secs: 10,
    };
    let stats = link_memory_notes(
        &[id],
        memory.sqlite.pool().clone(),
        embedding_store,
        embedding_provider(),
        &cfg,
    )
    .await;

    // No edges should be created — only self returned from search and self is excluded.
    assert_eq!(stats.edges_created, 0, "self-link must not be created");
}

#[tokio::test]
async fn link_memory_notes_threshold_filters() {
    let (memory, embedding_store) = memory_with_in_memory_vector_store().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    // Entities A and B: zero vectors → cosine similarity 1.0 (identical vectors).
    // Threshold 0.5: both A-B and A-C will be candidates since all vectors are zero.
    // This test verifies that at least A-B edge is created (score 1.0 >= 0.5).
    let id_a = seed_entity_with_zero_embedding(&store, &embedding_store, "thr_entity_a").await;
    let id_b = seed_entity_with_zero_embedding(&store, &embedding_store, "thr_entity_b").await;

    // Threshold 0.0: all non-negative scores pass. Since zero vectors give score 0.0,
    // and 0.0 >= 0.0 is true, edges will be created.
    let cfg = NoteLinkingConfig {
        enabled: true,
        similarity_threshold: 0.0,
        top_k: 5,
        timeout_secs: 10,
    };
    link_memory_notes(
        &[id_a],
        memory.sqlite.pool().clone(),
        embedding_store,
        embedding_provider(),
        &cfg,
    )
    .await;

    // Edge between A and B must exist.
    let (src, tgt) = if id_a < id_b {
        (id_a, id_b)
    } else {
        (id_b, id_a)
    };
    let edges = store.edges_for_entity(src).await.unwrap();
    let has_ab = edges.iter().any(|e| {
        e.relation == "similar_to"
            && ((e.source_entity_id == src && e.target_entity_id == tgt)
                || (e.source_entity_id == tgt && e.target_entity_id == src))
    });
    assert!(has_ab, "A-B edge must exist above threshold");
}

#[tokio::test]
async fn link_memory_notes_unidirectional() {
    let (memory, embedding_store) = memory_with_in_memory_vector_store().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    // Two similar entities with identical zero vectors.
    let id_x = seed_entity_with_zero_embedding(&store, &embedding_store, "uni_entity_x").await;
    let id_y = seed_entity_with_zero_embedding(&store, &embedding_store, "uni_entity_y").await;

    // Threshold 0.0: zero vectors produce score 0.0, 0.0 >= 0.0 is true.
    let cfg = NoteLinkingConfig {
        enabled: true,
        similarity_threshold: 0.0,
        top_k: 5,
        timeout_secs: 10,
    };

    // Run linking for both entities — even though both link each other, only one
    // row should be created because we enforce source_id < target_id.
    link_memory_notes(
        &[id_x, id_y],
        memory.sqlite.pool().clone(),
        embedding_store,
        embedding_provider(),
        &cfg,
    )
    .await;

    // Exactly one edge between the pair (unidirectional).
    let pool = memory.sqlite.pool();
    let count: i64 = zeph_db::query_scalar(sql!(
        "SELECT COUNT(*) FROM graph_edges
         WHERE relation = 'similar_to'
           AND ((source_entity_id = ?1 AND target_entity_id = ?2)
             OR (source_entity_id = ?2 AND target_entity_id = ?1))
           AND valid_to IS NULL"
    ))
    .bind(id_x)
    .bind(id_y)
    .fetch_one(pool)
    .await
    .unwrap();

    assert_eq!(
        count, 1,
        "must have exactly one unidirectional edge per pair"
    );
}

// ── edges_created stat accuracy (fix #1792) ──────────────────────────────────
//
// When both A and B are in entity_ids, the A→B and B→A directions both produce
// the same normalised (min, max) pair. Previously both calls to insert_edge
// returned Ok (the second updated confidence on the existing row), inflating
// edges_created to 2. After the fix, seen_pairs deduplication ensures only one
// insert_edge call is made per pair, keeping edges_created == 1.

#[tokio::test]
async fn link_memory_notes_edges_created_not_inflated() {
    let (memory, embedding_store) = memory_with_in_memory_vector_store().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    let id_a = seed_entity_with_zero_embedding(&store, &embedding_store, "stat_entity_a").await;
    let id_b = seed_entity_with_zero_embedding(&store, &embedding_store, "stat_entity_b").await;

    let cfg = NoteLinkingConfig {
        enabled: true,
        similarity_threshold: 0.0,
        top_k: 5,
        timeout_secs: 10,
    };
    // Pass both A and B so each will find the other during search.
    let stats = link_memory_notes(
        &[id_a, id_b],
        memory.sqlite.pool().clone(),
        embedding_store,
        embedding_provider(),
        &cfg,
    )
    .await;

    assert_eq!(
        stats.edges_created, 1,
        "edges_created must be 1 even when both endpoints are in entity_ids"
    );
}

// ── secondary self-skip guard (test #1790) ────────────────────────────────────
//
// When qdrant_point_id is NULL in the DB the primary point-id guard cannot exclude
// the self-result from the search. The secondary guard (`target_id == entity_id`)
// must catch it so no self-edge is created.

#[tokio::test]
async fn link_memory_notes_secondary_self_skip_guard() {
    let (memory, embedding_store) = memory_with_in_memory_vector_store().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    // Entity A: qdrant_point_id NOT written to DB — primary guard is inactive.
    let id_a = seed_entity_no_db_point_id(&store, &embedding_store, "secondary_guard_a").await;
    // Entity B: normal seeding so that search returns at least one non-self result.
    let id_b = seed_entity_with_zero_embedding(&store, &embedding_store, "secondary_guard_b").await;
    let id_c = seed_entity_with_zero_embedding(&store, &embedding_store, "secondary_guard_c").await;

    let cfg = NoteLinkingConfig {
        enabled: true,
        similarity_threshold: 0.0,
        top_k: 10,
        timeout_secs: 10,
    };
    link_memory_notes(
        &[id_a],
        memory.sqlite.pool().clone(),
        embedding_store,
        embedding_provider(),
        &cfg,
    )
    .await;

    // No self-edge A→A must exist.
    let self_count: i64 = zeph_db::query_scalar(sql!(
        "SELECT COUNT(*) FROM graph_edges
         WHERE source_entity_id = ?1 AND target_entity_id = ?1"
    ))
    .bind(id_a)
    .fetch_one(memory.sqlite.pool())
    .await
    .unwrap();
    assert_eq!(
        self_count, 0,
        "self-edge must not be created via secondary guard"
    );

    // At least one edge to B or C must exist (confirming A was processed successfully).
    let other_count: i64 = zeph_db::query_scalar(sql!(
        "SELECT COUNT(*) FROM graph_edges
         WHERE (source_entity_id = ?1 OR target_entity_id = ?1)
           AND source_entity_id != target_entity_id"
    ))
    .bind(id_a)
    .fetch_one(memory.sqlite.pool())
    .await
    .unwrap();
    let _ = (id_b, id_c); // referenced for context
    assert!(other_count > 0, "A must have at least one edge to B or C");
}

// ── threshold rejection (test #1791) ─────────────────────────────────────────
//
// MockProvider returns vec![0.0; 384]; InMemoryVectorStore scores identical zero
// vectors as 1.0. Setting similarity_threshold = 2.0 (above the maximum possible
// cosine similarity) must reject all candidates, producing zero edges.

#[tokio::test]
async fn link_memory_notes_threshold_rejection() {
    let (memory, embedding_store) = memory_with_in_memory_vector_store().await;
    let store = GraphStore::new(memory.sqlite.pool().clone());

    let id_a = seed_entity_with_zero_embedding(&store, &embedding_store, "rej_entity_a").await;
    let _id_b = seed_entity_with_zero_embedding(&store, &embedding_store, "rej_entity_b").await;

    // threshold = 2.0 is above the maximum possible cosine similarity (1.0).
    let cfg = NoteLinkingConfig {
        enabled: true,
        similarity_threshold: 2.0,
        top_k: 5,
        timeout_secs: 10,
    };
    let stats = link_memory_notes(
        &[id_a],
        memory.sqlite.pool().clone(),
        embedding_store,
        embedding_provider(),
        &cfg,
    )
    .await;

    assert_eq!(
        stats.edges_created, 0,
        "no edges must be created when all scores are below threshold"
    );
}
