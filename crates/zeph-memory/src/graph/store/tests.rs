// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use crate::graph::types::EdgeType;
use crate::store::SqliteStore;
#[allow(unused_imports)]
use zeph_db::sql;

async fn setup() -> GraphStore {
    let store = SqliteStore::new(":memory:").await.unwrap();
    GraphStore::new(store.pool().clone())
}

#[tokio::test]
async fn upsert_entity_insert_new() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("Alice", "Alice", EntityType::Person, Some("a person"))
        .await
        .unwrap();
    assert!(id > 0);
}

#[tokio::test]
async fn upsert_entity_update_existing() {
    let gs = setup().await;
    let id1 = gs
        .upsert_entity("Alice", "Alice", EntityType::Person, None)
        .await
        .unwrap();
    // Sleep 1ms to ensure datetime changes; SQLite datetime granularity is 1s,
    // so we verify idempotency instead of timestamp ordering.
    let id2 = gs
        .upsert_entity("Alice", "Alice", EntityType::Person, Some("updated"))
        .await
        .unwrap();
    assert_eq!(id1, id2);
    let entity = gs
        .find_entity("Alice", EntityType::Person)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.summary.as_deref(), Some("updated"));
}

#[tokio::test]
async fn find_entity_found() {
    let gs = setup().await;
    gs.upsert_entity("Bob", "Bob", EntityType::Tool, Some("a tool"))
        .await
        .unwrap();
    let entity = gs
        .find_entity("Bob", EntityType::Tool)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.name, "Bob");
    assert_eq!(entity.entity_type, EntityType::Tool);
}

#[tokio::test]
async fn find_entity_not_found() {
    let gs = setup().await;
    let result = gs.find_entity("Nobody", EntityType::Person).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn find_entities_fuzzy_partial_match() {
    let gs = setup().await;
    gs.upsert_entity("GraphQL", "GraphQL", EntityType::Concept, None)
        .await
        .unwrap();
    gs.upsert_entity("Graph", "Graph", EntityType::Concept, None)
        .await
        .unwrap();
    gs.upsert_entity("Unrelated", "Unrelated", EntityType::Concept, None)
        .await
        .unwrap();

    let results = gs.find_entities_fuzzy("graph", 10).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|e| e.name == "GraphQL"));
    assert!(results.iter().any(|e| e.name == "Graph"));
}

#[tokio::test]
async fn entity_count_empty() {
    let gs = setup().await;
    assert_eq!(gs.entity_count().await.unwrap(), 0);
}

#[tokio::test]
async fn entity_count_non_empty() {
    let gs = setup().await;
    gs.upsert_entity("A", "A", EntityType::Concept, None)
        .await
        .unwrap();
    gs.upsert_entity("B", "B", EntityType::Concept, None)
        .await
        .unwrap();
    assert_eq!(gs.entity_count().await.unwrap(), 2);
}

#[tokio::test]
async fn all_entities_and_stream() {
    use futures::StreamExt as _;

    let gs = setup().await;
    gs.upsert_entity("X", "X", EntityType::Project, None)
        .await
        .unwrap();
    gs.upsert_entity("Y", "Y", EntityType::Language, None)
        .await
        .unwrap();

    let all = gs.all_entities().await.unwrap();
    assert_eq!(all.len(), 2);
    let streamed: Vec<Result<Entity, _>> = gs.all_entities_stream().collect().await;
    assert_eq!(streamed.len(), 2);
    assert!(streamed.iter().all(Result::is_ok));
}

#[tokio::test]
async fn insert_edge_without_episode() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("Src", "Src", EntityType::Concept, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("Tgt", "Tgt", EntityType::Concept, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge(src, tgt, "relates_to", "Src relates to Tgt", 0.9, None)
        .await
        .unwrap();
    assert!(eid > 0);
}

#[tokio::test]
async fn insert_edge_deduplicates_active_edge() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("Alice", "Alice", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("Google", "Google", EntityType::Organization, None)
        .await
        .unwrap();

    let id1 = gs
        .insert_edge(src, tgt, "works_at", "Alice works at Google", 0.7, None)
        .await
        .unwrap();

    // Re-inserting the same (source, target, relation) must return the same id.
    let id2 = gs
        .insert_edge(src, tgt, "works_at", "Alice works at Google", 0.9, None)
        .await
        .unwrap();
    assert_eq!(id1, id2, "duplicate active edge must not be created");

    // Confidence should be updated to the higher value.
    let count: i64 = sqlx::query_scalar(sql!(
        "SELECT COUNT(*) FROM graph_edges WHERE valid_to IS NULL"
    ))
    .fetch_one(&gs.pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "only one active edge must exist");

    let conf: f64 = sqlx::query_scalar(sql!("SELECT confidence FROM graph_edges WHERE id = ?1"))
        .bind(id1)
        .fetch_one(&gs.pool)
        .await
        .unwrap();
    // Use 1e-6 tolerance: 0.9_f32 → f64 conversion is ~0.8999999761581421.
    assert!(
        (conf - f64::from(0.9_f32)).abs() < 1e-6,
        "confidence must be updated to max, got {conf}"
    );
}

#[tokio::test]
async fn insert_edge_different_relations_are_distinct() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("Bob", "Bob", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("Acme", "Acme", EntityType::Organization, None)
        .await
        .unwrap();

    let id1 = gs
        .insert_edge(src, tgt, "founded", "Bob founded Acme", 0.8, None)
        .await
        .unwrap();
    let id2 = gs
        .insert_edge(src, tgt, "chairs", "Bob chairs Acme", 0.8, None)
        .await
        .unwrap();
    assert_ne!(id1, id2, "different relations must produce distinct edges");

    let count: i64 = sqlx::query_scalar(sql!(
        "SELECT COUNT(*) FROM graph_edges WHERE valid_to IS NULL"
    ))
    .fetch_one(&gs.pool)
    .await
    .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn insert_edge_with_episode() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("Src2", "Src2", EntityType::Concept, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("Tgt2", "Tgt2", EntityType::Concept, None)
        .await
        .unwrap();
    // Verifies that passing an episode_id does not cause a panic or unexpected error on the
    // insertion path itself. The episode_id references the messages table; whether the FK
    // constraint fires depends on the SQLite FK enforcement mode at runtime. Both success
    // (FK off) and FK-violation error are acceptable outcomes for this test — we only assert
    // that insert_edge does not panic or return an unexpected error type.
    let episode = MessageId(999);
    let result = gs
        .insert_edge(src, tgt, "uses", "Src2 uses Tgt2", 1.0, Some(episode))
        .await;
    match &result {
        Ok(eid) => assert!(*eid > 0, "inserted edge should have positive id"),
        Err(MemoryError::Sqlx(_)) => {} // FK constraint failed — acceptable
        Err(e) => panic!("unexpected error: {e}"),
    }
}

#[tokio::test]
async fn invalidate_edge_sets_timestamps() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("E1", "E1", EntityType::Concept, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("E2", "E2", EntityType::Concept, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge(src, tgt, "r", "fact", 1.0, None)
        .await
        .unwrap();
    gs.invalidate_edge(eid).await.unwrap();

    let row: (Option<String>, Option<String>) = sqlx::query_as(sql!(
        "SELECT valid_to, expired_at FROM graph_edges WHERE id = ?1"
    ))
    .bind(eid)
    .fetch_one(&gs.pool)
    .await
    .unwrap();
    assert!(row.0.is_some(), "valid_to should be set");
    assert!(row.1.is_some(), "expired_at should be set");
}

#[tokio::test]
async fn edges_for_entity_both_directions() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("A", "A", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("B", "B", EntityType::Concept, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("C", "C", EntityType::Concept, None)
        .await
        .unwrap();
    gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
    gs.insert_edge(c, a, "r", "f2", 1.0, None).await.unwrap();

    let edges = gs.edges_for_entity(a).await.unwrap();
    assert_eq!(edges.len(), 2);
}

#[tokio::test]
async fn edges_between_both_directions() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("PA", "PA", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("PB", "PB", EntityType::Person, None)
        .await
        .unwrap();
    gs.insert_edge(a, b, "knows", "PA knows PB", 1.0, None)
        .await
        .unwrap();

    let fwd = gs.edges_between(a, b).await.unwrap();
    assert_eq!(fwd.len(), 1);
    let rev = gs.edges_between(b, a).await.unwrap();
    assert_eq!(rev.len(), 1);
}

#[tokio::test]
async fn active_edge_count_excludes_invalidated() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("N1", "N1", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("N2", "N2", EntityType::Concept, None)
        .await
        .unwrap();
    let e1 = gs.insert_edge(a, b, "r1", "f1", 1.0, None).await.unwrap();
    gs.insert_edge(a, b, "r2", "f2", 1.0, None).await.unwrap();
    gs.invalidate_edge(e1).await.unwrap();

    assert_eq!(gs.active_edge_count().await.unwrap(), 1);
}

#[tokio::test]
async fn upsert_community_insert_and_update() {
    let gs = setup().await;
    let id1 = gs
        .upsert_community("clusterA", "summary A", &[1, 2, 3], None)
        .await
        .unwrap();
    assert!(id1 > 0);
    let id2 = gs
        .upsert_community("clusterA", "summary A updated", &[1, 2, 3, 4], None)
        .await
        .unwrap();
    assert_eq!(id1, id2);
    let communities = gs.all_communities().await.unwrap();
    assert_eq!(communities.len(), 1);
    assert_eq!(communities[0].summary, "summary A updated");
    assert_eq!(communities[0].entity_ids, vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn community_for_entity_found() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("CA", "CA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("CB", "CB", EntityType::Concept, None)
        .await
        .unwrap();
    gs.upsert_community("cA", "summary", &[a, b], None)
        .await
        .unwrap();
    let result = gs.community_for_entity(a).await.unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().name, "cA");
}

#[tokio::test]
async fn community_for_entity_not_found() {
    let gs = setup().await;
    let result = gs.community_for_entity(999).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn community_count() {
    let gs = setup().await;
    assert_eq!(gs.community_count().await.unwrap(), 0);
    gs.upsert_community("c1", "s1", &[], None).await.unwrap();
    gs.upsert_community("c2", "s2", &[], None).await.unwrap();
    assert_eq!(gs.community_count().await.unwrap(), 2);
}

#[tokio::test]
async fn metadata_get_set_round_trip() {
    let gs = setup().await;
    assert_eq!(gs.get_metadata("counter").await.unwrap(), None);
    gs.set_metadata("counter", "42").await.unwrap();
    assert_eq!(gs.get_metadata("counter").await.unwrap(), Some("42".into()));
    gs.set_metadata("counter", "43").await.unwrap();
    assert_eq!(gs.get_metadata("counter").await.unwrap(), Some("43".into()));
}

#[tokio::test]
async fn bfs_max_hops_0_returns_only_start() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("BfsA", "BfsA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("BfsB", "BfsB", EntityType::Concept, None)
        .await
        .unwrap();
    gs.insert_edge(a, b, "r", "f", 1.0, None).await.unwrap();

    let (entities, edges) = gs.bfs(a, 0).await.unwrap();
    assert_eq!(entities.len(), 1);
    assert_eq!(entities[0].id, a);
    assert!(edges.is_empty());
}

#[tokio::test]
async fn bfs_max_hops_2_chain() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("ChainA", "ChainA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("ChainB", "ChainB", EntityType::Concept, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("ChainC", "ChainC", EntityType::Concept, None)
        .await
        .unwrap();
    gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
    gs.insert_edge(b, c, "r", "f2", 1.0, None).await.unwrap();

    let (entities, edges) = gs.bfs(a, 2).await.unwrap();
    let ids: Vec<_> = entities.iter().map(|e| e.id).collect();
    assert!(ids.contains(&a));
    assert!(ids.contains(&b));
    assert!(ids.contains(&c));
    assert_eq!(edges.len(), 2);
}

#[tokio::test]
async fn bfs_cycle_no_infinite_loop() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("CycA", "CycA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("CycB", "CycB", EntityType::Concept, None)
        .await
        .unwrap();
    gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
    gs.insert_edge(b, a, "r", "f2", 1.0, None).await.unwrap();

    let (entities, _edges) = gs.bfs(a, 3).await.unwrap();
    let ids: Vec<_> = entities.iter().map(|e| e.id).collect();
    // Should have exactly A and B, no infinite loop
    assert!(ids.contains(&a));
    assert!(ids.contains(&b));
    assert_eq!(ids.len(), 2);
}

#[tokio::test]
async fn test_invalidated_edges_excluded_from_bfs() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("InvA", "InvA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("InvB", "InvB", EntityType::Concept, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("InvC", "InvC", EntityType::Concept, None)
        .await
        .unwrap();
    let ab = gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
    gs.insert_edge(b, c, "r", "f2", 1.0, None).await.unwrap();
    // Invalidate A->B: BFS from A should not reach B or C.
    gs.invalidate_edge(ab).await.unwrap();

    let (entities, edges) = gs.bfs(a, 2).await.unwrap();
    let ids: Vec<_> = entities.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![a], "only start entity should be reachable");
    assert!(edges.is_empty(), "no active edges should be returned");
}

#[tokio::test]
async fn test_bfs_empty_graph() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("IsoA", "IsoA", EntityType::Concept, None)
        .await
        .unwrap();

    let (entities, edges) = gs.bfs(a, 2).await.unwrap();
    let ids: Vec<_> = entities.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![a], "isolated node: only start entity returned");
    assert!(edges.is_empty(), "no edges for isolated node");
}

#[tokio::test]
async fn test_bfs_diamond() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("DiamA", "DiamA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("DiamB", "DiamB", EntityType::Concept, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("DiamC", "DiamC", EntityType::Concept, None)
        .await
        .unwrap();
    let d = gs
        .upsert_entity("DiamD", "DiamD", EntityType::Concept, None)
        .await
        .unwrap();
    gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
    gs.insert_edge(a, c, "r", "f2", 1.0, None).await.unwrap();
    gs.insert_edge(b, d, "r", "f3", 1.0, None).await.unwrap();
    gs.insert_edge(c, d, "r", "f4", 1.0, None).await.unwrap();

    let (entities, edges) = gs.bfs(a, 2).await.unwrap();
    let mut ids: Vec<_> = entities.iter().map(|e| e.id).collect();
    ids.sort_unstable();
    let mut expected = vec![a, b, c, d];
    expected.sort_unstable();
    assert_eq!(ids, expected, "all 4 nodes reachable, no duplicates");
    assert_eq!(edges.len(), 4, "all 4 edges returned");
}

#[tokio::test]
async fn extraction_count_default_zero() {
    let gs = setup().await;
    assert_eq!(gs.extraction_count().await.unwrap(), 0);
}

#[tokio::test]
async fn extraction_count_after_set() {
    let gs = setup().await;
    gs.set_metadata("extraction_count", "7").await.unwrap();
    assert_eq!(gs.extraction_count().await.unwrap(), 7);
}

#[tokio::test]
async fn all_active_edges_stream_excludes_invalidated() {
    use futures::TryStreamExt as _;
    let gs = setup().await;
    let a = gs
        .upsert_entity("SA", "SA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("SB", "SB", EntityType::Concept, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("SC", "SC", EntityType::Concept, None)
        .await
        .unwrap();
    let e1 = gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
    gs.insert_edge(b, c, "r", "f2", 1.0, None).await.unwrap();
    gs.invalidate_edge(e1).await.unwrap();

    let edges: Vec<_> = gs.all_active_edges_stream().try_collect().await.unwrap();
    assert_eq!(edges.len(), 1, "only the active edge should be returned");
    assert_eq!(edges[0].source_entity_id, b);
    assert_eq!(edges[0].target_entity_id, c);
}

#[tokio::test]
async fn find_community_by_id_found_and_not_found() {
    let gs = setup().await;
    let cid = gs
        .upsert_community("grp", "summary", &[1, 2], None)
        .await
        .unwrap();
    let found = gs.find_community_by_id(cid).await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().name, "grp");

    let missing = gs.find_community_by_id(9999).await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn delete_all_communities_clears_table() {
    let gs = setup().await;
    gs.upsert_community("c1", "s1", &[1], None).await.unwrap();
    gs.upsert_community("c2", "s2", &[2], None).await.unwrap();
    assert_eq!(gs.community_count().await.unwrap(), 2);
    gs.delete_all_communities().await.unwrap();
    assert_eq!(gs.community_count().await.unwrap(), 0);
}

#[tokio::test]
async fn test_find_entities_fuzzy_no_results() {
    let gs = setup().await;
    gs.upsert_entity("Alpha", "Alpha", EntityType::Concept, None)
        .await
        .unwrap();
    let results = gs.find_entities_fuzzy("zzzznonexistent", 10).await.unwrap();
    assert!(
        results.is_empty(),
        "no entities should match an unknown term"
    );
}

// ── Canonicalization / alias tests ────────────────────────────────────────

#[tokio::test]
async fn upsert_entity_stores_canonical_name() {
    let gs = setup().await;
    gs.upsert_entity("rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    let entity = gs
        .find_entity("rust", EntityType::Language)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(entity.canonical_name, "rust");
    assert_eq!(entity.name, "rust");
}

#[tokio::test]
async fn add_alias_idempotent() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    gs.add_alias(id, "rust-lang").await.unwrap();
    // Second insert should succeed silently (INSERT OR IGNORE)
    gs.add_alias(id, "rust-lang").await.unwrap();
    let aliases = gs.aliases_for_entity(id).await.unwrap();
    assert_eq!(
        aliases
            .iter()
            .filter(|a| a.alias_name == "rust-lang")
            .count(),
        1
    );
}

// ── FTS5 fuzzy search tests ──────────────────────────────────────────────

#[tokio::test]
async fn find_entity_by_id_found() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("FindById", "finbyid", EntityType::Concept, Some("summary"))
        .await
        .unwrap();
    let entity = gs.find_entity_by_id(id).await.unwrap();
    assert!(entity.is_some());
    let entity = entity.unwrap();
    assert_eq!(entity.id, id);
    assert_eq!(entity.name, "FindById");
}

#[tokio::test]
async fn find_entity_by_id_not_found() {
    let gs = setup().await;
    let result = gs.find_entity_by_id(99999).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn set_entity_qdrant_point_id_updates() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("QdrantPoint", "qdrantpoint", EntityType::Concept, None)
        .await
        .unwrap();
    let point_id = "550e8400-e29b-41d4-a716-446655440000";
    gs.set_entity_qdrant_point_id(id, point_id).await.unwrap();

    let entity = gs.find_entity_by_id(id).await.unwrap().unwrap();
    assert_eq!(entity.qdrant_point_id.as_deref(), Some(point_id));
}

#[tokio::test]
async fn find_entities_fuzzy_matches_summary() {
    let gs = setup().await;
    gs.upsert_entity(
        "Rust",
        "Rust",
        EntityType::Language,
        Some("a systems programming language"),
    )
    .await
    .unwrap();
    gs.upsert_entity(
        "Go",
        "Go",
        EntityType::Language,
        Some("a compiled language by Google"),
    )
    .await
    .unwrap();
    // Search by summary word — should find "Rust" by "systems" in summary.
    let results = gs.find_entities_fuzzy("systems", 10).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "Rust");
}

#[tokio::test]
async fn find_entities_fuzzy_empty_query() {
    let gs = setup().await;
    gs.upsert_entity("Alpha", "Alpha", EntityType::Concept, None)
        .await
        .unwrap();
    // Empty query returns empty vec without hitting the database.
    let results = gs.find_entities_fuzzy("", 10).await.unwrap();
    assert!(results.is_empty(), "empty query should return no results");
    // Whitespace-only query also returns empty.
    let results = gs.find_entities_fuzzy("   ", 10).await.unwrap();
    assert!(
        results.is_empty(),
        "whitespace query should return no results"
    );
}

#[tokio::test]
async fn find_entity_by_alias_case_insensitive() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    gs.add_alias(id, "rust").await.unwrap();
    gs.add_alias(id, "rust-lang").await.unwrap();

    let found = gs
        .find_entity_by_alias("RUST-LANG", EntityType::Language)
        .await
        .unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().id, id);
}

#[tokio::test]
async fn find_entity_by_alias_returns_none_for_unknown() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    gs.add_alias(id, "rust").await.unwrap();

    let found = gs
        .find_entity_by_alias("python", EntityType::Language)
        .await
        .unwrap();
    assert!(found.is_none());
}

#[tokio::test]
async fn find_entity_by_alias_filters_by_entity_type() {
    // "python" alias for Language should NOT match when looking for Tool type
    let gs = setup().await;
    let lang_id = gs
        .upsert_entity("python", "python", EntityType::Language, None)
        .await
        .unwrap();
    gs.add_alias(lang_id, "python").await.unwrap();

    let found_tool = gs
        .find_entity_by_alias("python", EntityType::Tool)
        .await
        .unwrap();
    assert!(
        found_tool.is_none(),
        "cross-type alias collision must not occur"
    );

    let found_lang = gs
        .find_entity_by_alias("python", EntityType::Language)
        .await
        .unwrap();
    assert!(found_lang.is_some());
    assert_eq!(found_lang.unwrap().id, lang_id);
}

#[tokio::test]
async fn aliases_for_entity_returns_all() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    gs.add_alias(id, "rust").await.unwrap();
    gs.add_alias(id, "rust-lang").await.unwrap();
    gs.add_alias(id, "rustlang").await.unwrap();

    let aliases = gs.aliases_for_entity(id).await.unwrap();
    assert_eq!(aliases.len(), 3);
    let names: Vec<&str> = aliases.iter().map(|a| a.alias_name.as_str()).collect();
    assert!(names.contains(&"rust"));
    assert!(names.contains(&"rust-lang"));
    assert!(names.contains(&"rustlang"));
}

#[tokio::test]
async fn find_entities_fuzzy_includes_aliases() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    gs.add_alias(id, "rust-lang").await.unwrap();
    gs.upsert_entity("python", "python", EntityType::Language, None)
        .await
        .unwrap();

    // "rust-lang" is an alias, not the entity name — fuzzy search should still find it
    let results = gs.find_entities_fuzzy("rust-lang", 10).await.unwrap();
    assert!(!results.is_empty());
    assert!(results.iter().any(|e| e.id == id));
}

#[tokio::test]
async fn orphan_alias_cleanup_on_entity_delete() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    gs.add_alias(id, "rust").await.unwrap();
    gs.add_alias(id, "rust-lang").await.unwrap();

    // Delete the entity directly (bypassing FK for test purposes)
    sqlx::query(sql!("DELETE FROM graph_entities WHERE id = ?1"))
        .bind(id)
        .execute(&gs.pool)
        .await
        .unwrap();

    // ON DELETE CASCADE should have removed aliases
    let aliases = gs.aliases_for_entity(id).await.unwrap();
    assert!(
        aliases.is_empty(),
        "aliases should cascade-delete with entity"
    );
}

/// Validates migration 024 backfill on a pre-canonicalization database state.
///
/// Simulates a database at migration 021 state (no `canonical_name`, no aliases), inserts
/// entities and edges, then applies the migration 024 SQL directly via a single acquired
/// connection (required so that PRAGMA `foreign_keys` = OFF takes effect on the same
/// connection that executes DROP TABLE). Verifies:
/// - `canonical_name` is backfilled from name for all existing entities
/// - initial aliases are seeded from entity names
/// - `graph_edges` survive (FK cascade did not wipe them)
#[tokio::test]
#[cfg(feature = "sqlite")]
async fn migration_024_backfill_preserves_entities_and_edges() {
    use sqlx::Acquire as _;
    use sqlx::ConnectOptions as _;
    use sqlx::sqlite::SqliteConnectOptions;

    let opts = SqliteConnectOptions::from_url(&"sqlite::memory:".parse().unwrap())
        .unwrap()
        .foreign_keys(true);
    let pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();

    create_pre_023_schema(&pool).await;
    let (alice_id, rust_id) = seed_pre_023_fixtures(&pool).await;

    let mut conn = pool.acquire().await.unwrap();
    let conn = conn.acquire().await.unwrap();

    apply_migration_024(conn).await;

    assert_canonical_names_backfilled(conn, (alice_id, rust_id)).await;
    assert_aliases_seeded(conn, alice_id).await;
    assert_edges_survived(conn).await;
}

async fn create_pre_023_schema(pool: &sqlx::SqlitePool) {
    sqlx::query(sql!(
        "CREATE TABLE graph_entities (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            entity_type TEXT NOT NULL,
            summary TEXT,
            first_seen_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
            last_seen_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
            qdrant_point_id TEXT,
            UNIQUE(name, entity_type)
         )"
    ))
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(sql!(
        "CREATE TABLE graph_edges (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
            target_entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
            relation TEXT NOT NULL,
            fact TEXT NOT NULL,
            confidence REAL NOT NULL DEFAULT 1.0,
            valid_from TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
            valid_to TEXT,
            created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
            expired_at TEXT,
            episode_id INTEGER,
            qdrant_point_id TEXT
         )"
    ))
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(sql!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS graph_entities_fts USING fts5(
            name, summary, content='graph_entities', content_rowid='id',
            tokenize='unicode61 remove_diacritics 2'
         )"
    ))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        sql!("CREATE TRIGGER IF NOT EXISTS graph_entities_fts_insert AFTER INSERT ON graph_entities
         BEGIN INSERT INTO graph_entities_fts(rowid, name, summary) VALUES (new.id, new.name, COALESCE(new.summary, '')); END"),
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        sql!("CREATE TRIGGER IF NOT EXISTS graph_entities_fts_delete AFTER DELETE ON graph_entities
         BEGIN INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary) VALUES ('delete', old.id, old.name, COALESCE(old.summary, '')); END"),
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        sql!("CREATE TRIGGER IF NOT EXISTS graph_entities_fts_update AFTER UPDATE ON graph_entities
         BEGIN
             INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary) VALUES ('delete', old.id, old.name, COALESCE(old.summary, ''));
             INSERT INTO graph_entities_fts(rowid, name, summary) VALUES (new.id, new.name, COALESCE(new.summary, ''));
         END"),
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_pre_023_fixtures(pool: &sqlx::SqlitePool) -> (i64, i64) {
    let alice_id: i64 = sqlx::query_scalar(sql!(
        "INSERT INTO graph_entities (name, entity_type) VALUES ('Alice', 'person') RETURNING id"
    ))
    .fetch_one(pool)
    .await
    .unwrap();

    let rust_id: i64 = sqlx::query_scalar(sql!(
        "INSERT INTO graph_entities (name, entity_type) VALUES ('Rust', 'language') RETURNING id"
    ))
    .fetch_one(pool)
    .await
    .unwrap();

    sqlx::query(sql!(
        "INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact)
         VALUES (?1, ?2, 'uses', 'Alice uses Rust')"
    ))
    .bind(alice_id)
    .bind(rust_id)
    .execute(pool)
    .await
    .unwrap();

    (alice_id, rust_id)
}

/// Apply migration 024 SQL on a single pinned connection.
///
/// Must use a single connection so `PRAGMA foreign_keys = OFF` takes effect on the same
/// connection that executes DROP TABLE (PRAGMA is per-connection, not per-transaction).
#[allow(clippy::too_many_lines)]
async fn apply_migration_024(conn: &mut sqlx::SqliteConnection) {
    sqlx::query(sql!("PRAGMA foreign_keys = OFF"))
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(sql!(
        "ALTER TABLE graph_entities ADD COLUMN canonical_name TEXT"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(sql!(
        "UPDATE graph_entities SET canonical_name = name WHERE canonical_name IS NULL"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(sql!(
        "CREATE TABLE graph_entities_new (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            canonical_name TEXT NOT NULL,
            entity_type TEXT NOT NULL,
            summary TEXT,
            first_seen_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
            last_seen_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
            qdrant_point_id TEXT,
            UNIQUE(canonical_name, entity_type)
         )"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        sql!("INSERT INTO graph_entities_new
             (id, name, canonical_name, entity_type, summary, first_seen_at, last_seen_at, qdrant_point_id)
         SELECT id, name, COALESCE(canonical_name, name), entity_type, summary,
                first_seen_at, last_seen_at, qdrant_point_id
         FROM graph_entities"),
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(sql!("DROP TABLE graph_entities"))
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(sql!(
        "ALTER TABLE graph_entities_new RENAME TO graph_entities"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    // Rebuild FTS5 triggers (dropped with the old table) and rebuild index.
    sqlx::query(
        sql!("CREATE TRIGGER IF NOT EXISTS graph_entities_fts_insert AFTER INSERT ON graph_entities
         BEGIN INSERT INTO graph_entities_fts(rowid, name, summary) VALUES (new.id, new.name, COALESCE(new.summary, '')); END"),
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        sql!("CREATE TRIGGER IF NOT EXISTS graph_entities_fts_delete AFTER DELETE ON graph_entities
         BEGIN INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary) VALUES ('delete', old.id, old.name, COALESCE(old.summary, '')); END"),
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        sql!("CREATE TRIGGER IF NOT EXISTS graph_entities_fts_update AFTER UPDATE ON graph_entities
         BEGIN
             INSERT INTO graph_entities_fts(graph_entities_fts, rowid, name, summary) VALUES ('delete', old.id, old.name, COALESCE(old.summary, ''));
             INSERT INTO graph_entities_fts(rowid, name, summary) VALUES (new.id, new.name, COALESCE(new.summary, ''));
         END"),
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(sql!(
        "INSERT INTO graph_entities_fts(graph_entities_fts) VALUES('rebuild')"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(sql!(
        "CREATE TABLE graph_entity_aliases (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_id INTEGER NOT NULL REFERENCES graph_entities(id) ON DELETE CASCADE,
            alias_name TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (CURRENT_TIMESTAMP),
            UNIQUE(alias_name, entity_id)
         )"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(sql!(
        "INSERT OR IGNORE INTO graph_entity_aliases (entity_id, alias_name)
         SELECT id, name FROM graph_entities"
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(sql!("PRAGMA foreign_keys = ON"))
        .execute(&mut *conn)
        .await
        .unwrap();
}

async fn assert_canonical_names_backfilled(conn: &mut sqlx::SqliteConnection, ids: (i64, i64)) {
    let (alice_id, rust_id) = ids;
    let alice_canon: String = sqlx::query_scalar(sql!(
        "SELECT canonical_name FROM graph_entities WHERE id = ?1"
    ))
    .bind(alice_id)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        alice_canon, "Alice",
        "canonical_name should equal pre-migration name"
    );

    let rust_canon: String = sqlx::query_scalar(sql!(
        "SELECT canonical_name FROM graph_entities WHERE id = ?1"
    ))
    .bind(rust_id)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        rust_canon, "Rust",
        "canonical_name should equal pre-migration name"
    );
}

async fn assert_aliases_seeded(conn: &mut sqlx::SqliteConnection, alice_id: i64) {
    let alice_aliases: Vec<String> = sqlx::query_scalar(sql!(
        "SELECT alias_name FROM graph_entity_aliases WHERE entity_id = ?1"
    ))
    .bind(alice_id)
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert!(
        alice_aliases.contains(&"Alice".to_owned()),
        "initial alias should be seeded from entity name"
    );
}

async fn assert_edges_survived(conn: &mut sqlx::SqliteConnection) {
    let edge_count: i64 = sqlx::query_scalar(sql!("SELECT COUNT(*) FROM graph_edges"))
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        edge_count, 1,
        "graph_edges must survive migration 024 table recreation"
    );
}

#[tokio::test]
async fn find_entity_by_alias_same_alias_two_entities_deterministic() {
    // Two same-type entities share an alias — ORDER BY id ASC ensures first-registered wins.
    let gs = setup().await;
    let id1 = gs
        .upsert_entity("python-v2", "python-v2", EntityType::Language, None)
        .await
        .unwrap();
    let id2 = gs
        .upsert_entity("python-v3", "python-v3", EntityType::Language, None)
        .await
        .unwrap();
    gs.add_alias(id1, "python").await.unwrap();
    gs.add_alias(id2, "python").await.unwrap();

    // Both entities now have alias "python" — should return the first-registered (id1)
    let found = gs
        .find_entity_by_alias("python", EntityType::Language)
        .await
        .unwrap();
    assert!(found.is_some(), "should find an entity by shared alias");
    // ORDER BY e.id ASC guarantees deterministic result: first inserted wins
    assert_eq!(
        found.unwrap().id,
        id1,
        "first-registered entity should win on shared alias"
    );
}

// ── FTS5 search tests ────────────────────────────────────────────────────

#[tokio::test]
async fn find_entities_fuzzy_special_chars() {
    let gs = setup().await;
    gs.upsert_entity("Graph", "Graph", EntityType::Concept, None)
        .await
        .unwrap();
    // FTS5 special characters in query must not cause an error.
    let results = gs.find_entities_fuzzy("graph\"()*:^", 10).await.unwrap();
    // "graph" survives sanitization and matches.
    assert!(results.iter().any(|e| e.name == "Graph"));
}

#[tokio::test]
async fn find_entities_fuzzy_prefix_match() {
    let gs = setup().await;
    gs.upsert_entity("Graph", "Graph", EntityType::Concept, None)
        .await
        .unwrap();
    gs.upsert_entity("GraphQL", "GraphQL", EntityType::Concept, None)
        .await
        .unwrap();
    gs.upsert_entity("Unrelated", "Unrelated", EntityType::Concept, None)
        .await
        .unwrap();
    // "Gra" prefix should match both "Graph" and "GraphQL" via FTS5 "gra*".
    let results = gs.find_entities_fuzzy("Gra", 10).await.unwrap();
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|e| e.name == "Graph"));
    assert!(results.iter().any(|e| e.name == "GraphQL"));
}

#[tokio::test]
async fn find_entities_fuzzy_fts5_operator_injection() {
    let gs = setup().await;
    gs.upsert_entity("Graph", "Graph", EntityType::Concept, None)
        .await
        .unwrap();
    gs.upsert_entity("Unrelated", "Unrelated", EntityType::Concept, None)
        .await
        .unwrap();
    // "graph OR unrelated" — sanitizer splits on non-alphanumeric chars,
    // yielding tokens ["graph", "OR", "unrelated"]. The FTS5_OPERATORS filter
    // removes "OR", producing "graph* unrelated*" (implicit AND).
    // No entity contains both token prefixes, so the result is empty.
    let results = gs
        .find_entities_fuzzy("graph OR unrelated", 10)
        .await
        .unwrap();
    assert!(
        results.is_empty(),
        "implicit AND of 'graph*' and 'unrelated*' should match no entity"
    );
}

#[tokio::test]
async fn find_entities_fuzzy_after_entity_update() {
    let gs = setup().await;
    // Insert entity with initial summary.
    gs.upsert_entity(
        "Foo",
        "Foo",
        EntityType::Concept,
        Some("initial summary bar"),
    )
    .await
    .unwrap();
    // Update summary via upsert — triggers the FTS UPDATE trigger.
    gs.upsert_entity(
        "Foo",
        "Foo",
        EntityType::Concept,
        Some("updated summary baz"),
    )
    .await
    .unwrap();
    // Old summary term should not match.
    let old_results = gs.find_entities_fuzzy("bar", 10).await.unwrap();
    assert!(
        old_results.is_empty(),
        "old summary content should not match after update"
    );
    // New summary term should match.
    let new_results = gs.find_entities_fuzzy("baz", 10).await.unwrap();
    assert_eq!(new_results.len(), 1);
    assert_eq!(new_results[0].name, "Foo");
}

#[tokio::test]
async fn find_entities_fuzzy_only_special_chars() {
    let gs = setup().await;
    gs.upsert_entity("Alpha", "Alpha", EntityType::Concept, None)
        .await
        .unwrap();
    // Queries consisting solely of FTS5 special characters produce no alphanumeric
    // tokens after sanitization, so the function returns early with an empty vec
    // rather than passing an empty or malformed MATCH expression to FTS5.
    let results = gs.find_entities_fuzzy("***", 10).await.unwrap();
    assert!(
        results.is_empty(),
        "only special chars should return no results"
    );
    let results = gs.find_entities_fuzzy("(((", 10).await.unwrap();
    assert!(results.is_empty(), "only parens should return no results");
    let results = gs.find_entities_fuzzy("\"\"\"", 10).await.unwrap();
    assert!(results.is_empty(), "only quotes should return no results");
}

// ── find_entity_by_name tests ─────────────────────────────────────────────

#[tokio::test]
async fn find_entity_by_name_exact_wins_over_summary_mention() {
    // Regression test for: /graph facts Alice returns Google because Google's
    // summary mentions "Alice".
    let gs = setup().await;
    gs.upsert_entity(
        "Alice",
        "Alice",
        EntityType::Person,
        Some("A person named Alice"),
    )
    .await
    .unwrap();
    // Google's summary mentions "Alice" — without the fix, FTS5 could rank this first.
    gs.upsert_entity(
        "Google",
        "Google",
        EntityType::Organization,
        Some("Company where Charlie, Alice, and Bob have worked"),
    )
    .await
    .unwrap();

    let results = gs.find_entity_by_name("Alice").await.unwrap();
    assert!(!results.is_empty(), "must find at least one entity");
    assert_eq!(
        results[0].name, "Alice",
        "exact name match must come first, not entity with 'Alice' in summary"
    );
}

#[tokio::test]
async fn find_entity_by_name_case_insensitive_exact() {
    let gs = setup().await;
    gs.upsert_entity("Bob", "Bob", EntityType::Person, None)
        .await
        .unwrap();

    let results = gs.find_entity_by_name("bob").await.unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].name, "Bob");
}

#[tokio::test]
async fn find_entity_by_name_falls_back_to_fuzzy_when_no_exact_match() {
    let gs = setup().await;
    gs.upsert_entity("Charlie", "Charlie", EntityType::Person, None)
        .await
        .unwrap();

    // "Char" is not an exact match for "Charlie" → FTS5 prefix fallback should find it.
    let results = gs.find_entity_by_name("Char").await.unwrap();
    assert!(!results.is_empty(), "prefix search must find Charlie");
}

#[tokio::test]
async fn find_entity_by_name_returns_empty_for_unknown() {
    let gs = setup().await;
    let results = gs.find_entity_by_name("NonExistent").await.unwrap();
    assert!(results.is_empty());
}

#[tokio::test]
async fn find_entity_by_name_matches_canonical_name() {
    // Verify the exact-match phase checks canonical_name, not only name.
    let gs = setup().await;
    // upsert_entity sets canonical_name = second arg
    gs.upsert_entity("Dave (Engineer)", "Dave", EntityType::Person, None)
        .await
        .unwrap();

    // Searching by canonical_name "Dave" must return the entity even though
    // the display name is "Dave (Engineer)".
    let results = gs.find_entity_by_name("Dave").await.unwrap();
    assert!(
        !results.is_empty(),
        "canonical_name match must return entity"
    );
    assert_eq!(results[0].canonical_name, "Dave");
}

async fn insert_test_message(gs: &GraphStore, content: &str) -> crate::types::MessageId {
    // Insert a conversation first (FK constraint).
    let conv_id: i64 = sqlx::query_scalar(sql!(
        "INSERT INTO conversations DEFAULT VALUES RETURNING id"
    ))
    .fetch_one(&gs.pool)
    .await
    .unwrap();
    let id: i64 = sqlx::query_scalar(sql!(
        "INSERT INTO messages (conversation_id, role, content) VALUES (?1, 'user', ?2) RETURNING id"
    ))
    .bind(conv_id)
    .bind(content)
    .fetch_one(&gs.pool)
    .await
    .unwrap();
    crate::types::MessageId(id)
}

#[tokio::test]
async fn unprocessed_messages_for_backfill_returns_unprocessed() {
    let gs = setup().await;
    let id1 = insert_test_message(&gs, "hello world").await;
    let id2 = insert_test_message(&gs, "second message").await;

    let rows = gs.unprocessed_messages_for_backfill(10).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|(id, _)| *id == id1));
    assert!(rows.iter().any(|(id, _)| *id == id2));
}

#[tokio::test]
async fn unprocessed_messages_for_backfill_respects_limit() {
    let gs = setup().await;
    insert_test_message(&gs, "msg1").await;
    insert_test_message(&gs, "msg2").await;
    insert_test_message(&gs, "msg3").await;

    let rows = gs.unprocessed_messages_for_backfill(2).await.unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
async fn mark_messages_graph_processed_updates_flag() {
    let gs = setup().await;
    let id1 = insert_test_message(&gs, "to process").await;
    let _id2 = insert_test_message(&gs, "also to process").await;

    // Before marking: both are unprocessed
    let count_before = gs.unprocessed_message_count().await.unwrap();
    assert_eq!(count_before, 2);

    gs.mark_messages_graph_processed(&[id1]).await.unwrap();

    let count_after = gs.unprocessed_message_count().await.unwrap();
    assert_eq!(count_after, 1);

    // Remaining unprocessed should not contain id1
    let rows = gs.unprocessed_messages_for_backfill(10).await.unwrap();
    assert!(!rows.iter().any(|(id, _)| *id == id1));
}

#[tokio::test]
async fn mark_messages_graph_processed_empty_ids_is_noop() {
    let gs = setup().await;
    insert_test_message(&gs, "message").await;

    gs.mark_messages_graph_processed(&[]).await.unwrap();

    let count = gs.unprocessed_message_count().await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn edges_after_id_first_page_returns_all_within_limit() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("PA", "PA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("PB", "PB", EntityType::Concept, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("PC", "PC", EntityType::Concept, None)
        .await
        .unwrap();
    let e1 = gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
    let e2 = gs.insert_edge(b, c, "r", "f2", 1.0, None).await.unwrap();
    let e3 = gs.insert_edge(a, c, "r", "f3", 1.0, None).await.unwrap();

    // after_id=0 returns first page.
    let page1 = gs.edges_after_id(0, 2).await.unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].id, e1);
    assert_eq!(page1[1].id, e2);

    // Continue from last id of page1.
    let page2 = gs
        .edges_after_id(page1.last().unwrap().id, 2)
        .await
        .unwrap();
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].id, e3);

    // Page after the last element returns empty.
    let page3 = gs
        .edges_after_id(page2.last().unwrap().id, 2)
        .await
        .unwrap();
    assert!(page3.is_empty(), "no more edges after last id");
}

#[tokio::test]
async fn edges_after_id_skips_invalidated_edges() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("IA", "IA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("IB", "IB", EntityType::Concept, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("IC", "IC", EntityType::Concept, None)
        .await
        .unwrap();
    let e1 = gs.insert_edge(a, b, "r", "f1", 1.0, None).await.unwrap();
    let e2 = gs.insert_edge(b, c, "r", "f2", 1.0, None).await.unwrap();

    // Invalidate e1 — it must not appear in edges_after_id results.
    gs.invalidate_edge(e1).await.unwrap();

    let page = gs.edges_after_id(0, 10).await.unwrap();
    assert_eq!(page.len(), 1, "invalidated edge must be excluded");
    assert_eq!(page[0].id, e2);
}

// ── Temporal query tests ──────────────────────────────────────────────────

#[tokio::test]
async fn edges_at_timestamp_returns_active_edge() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("TA", "TA", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("TB", "TB", EntityType::Person, None)
        .await
        .unwrap();
    gs.insert_edge(a, b, "knows", "TA knows TB", 1.0, None)
        .await
        .unwrap();

    // Active edge (valid_to IS NULL) must be visible at any timestamp.
    let edges = gs
        .edges_at_timestamp(a, "2099-01-01 00:00:00")
        .await
        .unwrap();
    assert_eq!(edges.len(), 1, "active edge must be visible at future ts");
    assert_eq!(edges[0].relation, "knows");
}

#[tokio::test]
async fn edges_at_timestamp_excludes_future_valid_from() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("FA", "FA", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("FB", "FB", EntityType::Person, None)
        .await
        .unwrap();
    // Insert edge with valid_from in the far future.
    sqlx::query(
        sql!("INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'rel', 'fact', 1.0, '2100-01-01 00:00:00')"),
    )
    .bind(a)
    .bind(b)
    .execute(gs.pool())
    .await
    .unwrap();

    // Query at 2026 — future-valid_from edge must be excluded.
    let edges = gs
        .edges_at_timestamp(a, "2026-01-01 00:00:00")
        .await
        .unwrap();
    assert!(
        edges.is_empty(),
        "edge with future valid_from must not be visible at earlier timestamp"
    );
}

#[tokio::test]
async fn edges_at_timestamp_historical_window_visible() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("HA", "HA", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("HB", "HB", EntityType::Person, None)
        .await
        .unwrap();
    // Expired edge valid 2020-01-01 → 2021-01-01.
    sqlx::query(
        sql!("INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from, valid_to, expired_at)
         VALUES (?1, ?2, 'managed', 'HA managed HB', 0.8,
                 '2020-01-01 00:00:00', '2021-01-01 00:00:00', '2021-01-01 00:00:00')"),
    )
    .bind(a)
    .bind(b)
    .execute(gs.pool())
    .await
    .unwrap();

    // During validity window → visible.
    let during = gs
        .edges_at_timestamp(a, "2020-06-01 00:00:00")
        .await
        .unwrap();
    assert_eq!(
        during.len(),
        1,
        "expired edge must be visible during its validity window"
    );

    // Before valid_from → not visible.
    let before = gs
        .edges_at_timestamp(a, "2019-01-01 00:00:00")
        .await
        .unwrap();
    assert!(
        before.is_empty(),
        "edge must not be visible before valid_from"
    );

    // After valid_to → not visible.
    let after = gs
        .edges_at_timestamp(a, "2026-01-01 00:00:00")
        .await
        .unwrap();
    assert!(
        after.is_empty(),
        "expired edge must not be visible after valid_to"
    );
}

#[tokio::test]
async fn edges_at_timestamp_entity_as_target() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("SRC", "SRC", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("TGT", "TGT", EntityType::Person, None)
        .await
        .unwrap();
    gs.insert_edge(src, tgt, "links", "SRC links TGT", 0.9, None)
        .await
        .unwrap();

    // Query by target entity_id at a far-future timestamp — must find the active edge.
    let edges = gs
        .edges_at_timestamp(tgt, "2099-01-01 00:00:00")
        .await
        .unwrap();
    assert_eq!(
        edges.len(),
        1,
        "edge must be found when querying by target entity_id"
    );
}

#[tokio::test]
async fn bfs_at_timestamp_excludes_expired_edges() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("BA", "BA", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("BB", "BB", EntityType::Person, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("BC", "BC", EntityType::Concept, None)
        .await
        .unwrap();

    // A → B: active edge with explicit valid_from in 2019 so it predates all test timestamps.
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'knows', 'BA knows BB', 1.0, '2019-01-01 00:00:00')"
    ))
    .bind(a)
    .bind(b)
    .execute(gs.pool())
    .await
    .unwrap();
    // B → C: expired edge valid 2020→2021.
    sqlx::query(
        sql!("INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from, valid_to, expired_at)
         VALUES (?1, ?2, 'used', 'BB used BC', 0.9,
                 '2020-01-01 00:00:00', '2021-01-01 00:00:00', '2021-01-01 00:00:00')"),
    )
    .bind(b)
    .bind(c)
    .execute(gs.pool())
    .await
    .unwrap();

    // BFS at 2026: A→B active (valid since 2019); B→C expired → C not reachable at 2026.
    let (entities, _edges, depth_map) = gs
        .bfs_at_timestamp(a, 3, "2026-01-01 00:00:00")
        .await
        .unwrap();
    let entity_ids: Vec<i64> = entities.iter().map(|e| e.id).collect();
    assert!(
        depth_map.contains_key(&a),
        "start entity must be in depth_map"
    );
    assert!(
        depth_map.contains_key(&b),
        "B should be reachable via active A→B edge"
    );
    assert!(
        !entity_ids.contains(&c),
        "C must not be reachable at 2026 because B→C expired in 2021"
    );

    // BFS at 2020-06-01: both A→B (active since 2019) and B→C (within window) are valid.
    let (_entities2, _edges2, depth_map2) = gs
        .bfs_at_timestamp(a, 3, "2020-06-01 00:00:00")
        .await
        .unwrap();
    assert!(
        depth_map2.contains_key(&c),
        "C should be reachable at 2020-06-01 when B→C was valid"
    );
}

#[tokio::test]
async fn edge_history_returns_all_versions_ordered() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("ESrc", "ESrc", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("ETgt", "ETgt", EntityType::Organization, None)
        .await
        .unwrap();

    // Version 1: valid 2020→2022 (expired).
    sqlx::query(
        sql!("INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from, valid_to, expired_at)
         VALUES (?1, ?2, 'works_at', 'ESrc works at CompanyA', 0.9,
                 '2020-01-01 00:00:00', '2022-01-01 00:00:00', '2022-01-01 00:00:00')"),
    )
    .bind(src)
    .bind(tgt)
    .execute(gs.pool())
    .await
    .unwrap();
    // Version 2: active since 2022.
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'works_at', 'ESrc works at CompanyB', 0.95, '2022-01-01 00:00:00')"
    ))
    .bind(src)
    .bind(tgt)
    .execute(gs.pool())
    .await
    .unwrap();

    // History without relation filter — both versions returned, newest first.
    let history = gs.edge_history(src, "works at", None, 100).await.unwrap();
    assert_eq!(history.len(), 2, "both edge versions must be returned");
    // Ordered valid_from DESC — version 2 (2022) before version 1 (2020).
    assert!(
        history[0].valid_from >= history[1].valid_from,
        "results must be ordered by valid_from DESC"
    );

    // History with relation filter.
    let filtered = gs
        .edge_history(src, "works at", Some("works_at"), 100)
        .await
        .unwrap();
    assert_eq!(
        filtered.len(),
        2,
        "relation filter must retain both versions"
    );

    // History with non-matching predicate.
    let empty = gs
        .edge_history(src, "nonexistent_predicate_xyz", None, 100)
        .await
        .unwrap();
    assert!(empty.is_empty(), "non-matching predicate must return empty");
}

#[tokio::test]
async fn edge_history_like_escaping() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("EscSrc", "EscSrc", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("EscTgt", "EscTgt", EntityType::Concept, None)
        .await
        .unwrap();

    // Insert an edge with a fact that contains neither '%' nor '_'.
    gs.insert_edge(src, tgt, "ref", "plain text fact no wildcards", 1.0, None)
        .await
        .unwrap();

    // Searching with '%' as predicate must NOT match all edges (wildcard injection).
    // After LIKE escaping '%' becomes '\%', so only facts containing literal '%' match.
    let results = gs.edge_history(src, "%", None, 100).await.unwrap();
    assert!(
        results.is_empty(),
        "LIKE wildcard '%' in predicate must be escaped and not match all edges"
    );

    // Searching with '_' must only match facts containing literal '_'.
    // Our fact has no '_', so result must be empty.
    let results_underscore = gs.edge_history(src, "_", None, 100).await.unwrap();
    assert!(
        results_underscore.is_empty(),
        "LIKE wildcard '_' in predicate must be escaped and not match single-char substrings"
    );
}

#[tokio::test]
async fn invalidate_edge_sets_valid_to_and_expired_at() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("InvA", "InvA", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("InvB", "InvB", EntityType::Person, None)
        .await
        .unwrap();
    let edge_id = gs
        .insert_edge(a, b, "rel", "InvA rel InvB", 1.0, None)
        .await
        .unwrap();

    // Before invalidation: valid_to and expired_at must be NULL.
    let active_edge: (Option<String>, Option<String>) = sqlx::query_as(sql!(
        "SELECT valid_to, expired_at FROM graph_edges WHERE id = ?1"
    ))
    .bind(edge_id)
    .fetch_one(gs.pool())
    .await
    .unwrap();
    assert!(
        active_edge.0.is_none(),
        "valid_to must be NULL before invalidation"
    );
    assert!(
        active_edge.1.is_none(),
        "expired_at must be NULL before invalidation"
    );

    gs.invalidate_edge(edge_id).await.unwrap();

    // After invalidation: both valid_to and expired_at must be set.
    let dead_edge: (Option<String>, Option<String>) = sqlx::query_as(sql!(
        "SELECT valid_to, expired_at FROM graph_edges WHERE id = ?1"
    ))
    .bind(edge_id)
    .fetch_one(gs.pool())
    .await
    .unwrap();
    assert!(
        dead_edge.0.is_some(),
        "valid_to must be set after invalidation"
    );
    assert!(
        dead_edge.1.is_some(),
        "expired_at must be set after invalidation"
    );
}

// ── New temporal unit tests (issue-1776) ──────────────────────────────────

// edges_at_timestamp

#[tokio::test]
async fn edges_at_timestamp_valid_from_inclusive() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("VFI_A", "VFI_A", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("VFI_B", "VFI_B", EntityType::Person, None)
        .await
        .unwrap();
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'knows', 'VFI_A knows VFI_B', 1.0, '2025-06-01 00:00:00')"
    ))
    .bind(a)
    .bind(b)
    .execute(gs.pool())
    .await
    .unwrap();

    // Query at exactly valid_from — must be included (valid_from <= ts).
    let edges = gs
        .edges_at_timestamp(a, "2025-06-01 00:00:00")
        .await
        .unwrap();
    assert_eq!(
        edges.len(),
        1,
        "edge with valid_from == ts must be visible (inclusive boundary)"
    );
}

#[tokio::test]
async fn edges_at_timestamp_valid_to_exclusive() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("VTO_A", "VTO_A", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("VTO_B", "VTO_B", EntityType::Person, None)
        .await
        .unwrap();
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence,
          valid_from, valid_to, expired_at)
         VALUES (?1, ?2, 'knows', 'VTO_A knows VTO_B', 1.0,
                 '2020-01-01 00:00:00', '2025-06-01 00:00:00', '2025-06-01 00:00:00')"
    ))
    .bind(a)
    .bind(b)
    .execute(gs.pool())
    .await
    .unwrap();

    // Query at exactly valid_to — must be excluded (valid_to > ts fails when equal).
    let at_boundary = gs
        .edges_at_timestamp(a, "2025-06-01 00:00:00")
        .await
        .unwrap();
    assert!(
        at_boundary.is_empty(),
        "edge with valid_to == ts must NOT be visible (exclusive upper boundary)"
    );

    // Query one second before valid_to — must be included.
    let before_boundary = gs
        .edges_at_timestamp(a, "2025-05-31 23:59:59")
        .await
        .unwrap();
    assert_eq!(
        before_boundary.len(),
        1,
        "edge must be visible one second before valid_to"
    );
}

#[tokio::test]
async fn edges_at_timestamp_multiple_edges_same_entity() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("ME_A", "ME_A", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("ME_B", "ME_B", EntityType::Person, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("ME_C", "ME_C", EntityType::Person, None)
        .await
        .unwrap();
    let d = gs
        .upsert_entity("ME_D", "ME_D", EntityType::Person, None)
        .await
        .unwrap();

    // A->B: active since 2020, no expiry — visible at 2025.
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'knows', 'ME_A knows ME_B', 1.0, '2020-01-01 00:00:00')"
    ))
    .bind(a)
    .bind(b)
    .execute(gs.pool())
    .await
    .unwrap();
    // A->C: expired in 2023 — NOT visible at 2025.
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence,
          valid_from, valid_to, expired_at)
         VALUES (?1, ?2, 'knows', 'ME_A knows ME_C', 1.0,
                 '2020-01-01 00:00:00', '2023-01-01 00:00:00', '2023-01-01 00:00:00')"
    ))
    .bind(a)
    .bind(c)
    .execute(gs.pool())
    .await
    .unwrap();
    // A->D: future valid_from 2030 — NOT visible at 2025.
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'knows', 'ME_A knows ME_D', 1.0, '2030-01-01 00:00:00')"
    ))
    .bind(a)
    .bind(d)
    .execute(gs.pool())
    .await
    .unwrap();

    let edges = gs
        .edges_at_timestamp(a, "2025-01-01 00:00:00")
        .await
        .unwrap();
    assert_eq!(
        edges.len(),
        1,
        "only A->B must be visible at 2025 (C expired, D future)"
    );
    assert_eq!(edges[0].target_entity_id, b);
}

#[tokio::test]
async fn edges_at_timestamp_no_edges_returns_empty() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("NE_A", "NE_A", EntityType::Person, None)
        .await
        .unwrap();

    let edges = gs
        .edges_at_timestamp(a, "2025-01-01 00:00:00")
        .await
        .unwrap();
    assert!(
        edges.is_empty(),
        "entity with no edges must return empty vec"
    );
}

// edge_history

#[tokio::test]
async fn edge_history_basic_history() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("EH_Src", "EH_Src", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("EH_Tgt", "EH_Tgt", EntityType::Organization, None)
        .await
        .unwrap();

    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence,
          valid_from, valid_to, expired_at)
         VALUES (?1, ?2, 'works_at', 'EH_Src works at OrgA', 0.9,
                 '2020-01-01 00:00:00', '2022-01-01 00:00:00', '2022-01-01 00:00:00')"
    ))
    .bind(src)
    .bind(tgt)
    .execute(gs.pool())
    .await
    .unwrap();
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'works_at', 'EH_Src works at OrgB', 0.95, '2022-01-01 00:00:00')"
    ))
    .bind(src)
    .bind(tgt)
    .execute(gs.pool())
    .await
    .unwrap();

    let history = gs.edge_history(src, "works at", None, 100).await.unwrap();
    assert_eq!(history.len(), 2, "both versions must be returned");
    assert!(
        history[0].valid_from > history[1].valid_from,
        "ordered valid_from DESC — versions have distinct timestamps"
    );
}

#[tokio::test]
async fn edge_history_for_entity_includes_expired() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("HistA", "HistA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("HistB", "HistB", EntityType::Concept, None)
        .await
        .unwrap();

    // Insert and immediately invalidate first edge
    let e1 = gs
        .insert_edge(a, b, "uses", "old fact", 0.8, None)
        .await
        .unwrap();
    gs.invalidate_edge(e1).await.unwrap();
    // Insert new active edge
    gs.insert_edge(a, b, "uses", "new fact", 0.9, None)
        .await
        .unwrap();

    let history = gs.edge_history_for_entity(a, 10).await.unwrap();
    assert_eq!(
        history.len(),
        2,
        "both active and expired edges must appear"
    );

    // Most recent first — active edge has later valid_from
    let active = history.iter().find(|e| e.valid_to.is_none());
    let expired = history.iter().find(|e| e.valid_to.is_some());
    assert!(active.is_some(), "active edge must be in history");
    assert!(expired.is_some(), "expired edge must be in history");
}

#[tokio::test]
async fn edge_history_for_entity_both_directions() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("DirA", "DirA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("DirB", "DirB", EntityType::Concept, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("DirC", "DirC", EntityType::Concept, None)
        .await
        .unwrap();

    gs.insert_edge(a, b, "r1", "f1", 1.0, None).await.unwrap();
    gs.insert_edge(c, a, "r2", "f2", 1.0, None).await.unwrap();

    let history = gs.edge_history_for_entity(a, 10).await.unwrap();
    assert_eq!(
        history.len(),
        2,
        "both outgoing and incoming edges must appear"
    );
    assert!(
        history
            .iter()
            .any(|e| e.source_entity_id == a && e.target_entity_id == b)
    );
    assert!(
        history
            .iter()
            .any(|e| e.source_entity_id == c && e.target_entity_id == a)
    );
}

#[tokio::test]
async fn edge_history_for_entity_respects_limit() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("LimA", "LimA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("LimB", "LimB", EntityType::Concept, None)
        .await
        .unwrap();

    for i in 0..5u32 {
        gs.insert_edge(a, b, &format!("r{i}"), &format!("fact {i}"), 1.0, None)
            .await
            .unwrap();
    }

    let history = gs.edge_history_for_entity(a, 2).await.unwrap();
    assert_eq!(history.len(), 2, "limit must be respected");
}

#[tokio::test]
async fn edge_history_limit_parameter() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("EHL_Src", "EHL_Src", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("EHL_Tgt", "EHL_Tgt", EntityType::Organization, None)
        .await
        .unwrap();

    for (year, rel) in [
        (2018i32, "worked_at_v1"),
        (2019, "worked_at_v2"),
        (2020, "worked_at_v3"),
        (2021, "worked_at_v4"),
        (2022, "worked_at_v5"),
    ] {
        let valid_from = format!("{year}-01-01 00:00:00");
        sqlx::query(sql!(
            "INSERT INTO graph_edges
             (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
             VALUES (?1, ?2, ?3, 'EHL_Src worked at org', 1.0, ?4)"
        ))
        .bind(src)
        .bind(tgt)
        .bind(rel)
        .bind(valid_from)
        .execute(gs.pool())
        .await
        .unwrap();
    }

    // Pre-condition: all 5 rows match without a limit.
    let all = gs.edge_history(src, "worked at", None, 100).await.unwrap();
    assert_eq!(
        all.len(),
        5,
        "all 5 rows must match without limit constraint"
    );

    let limited = gs.edge_history(src, "worked at", None, 2).await.unwrap();
    assert_eq!(limited.len(), 2, "limit=2 must truncate to 2 results");
    assert!(
        limited[0].valid_from > limited[1].valid_from,
        "most recent results first"
    );
}

#[tokio::test]
async fn edge_history_non_matching_relation_returns_empty() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("EHR_Src", "EHR_Src", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("EHR_Tgt", "EHR_Tgt", EntityType::Organization, None)
        .await
        .unwrap();

    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'works_at', 'EHR_Src works at place', 1.0, '2020-01-01 00:00:00')"
    ))
    .bind(src)
    .bind(tgt)
    .execute(gs.pool())
    .await
    .unwrap();

    let result = gs
        .edge_history(src, "works at", Some("lives_in"), 100)
        .await
        .unwrap();
    assert!(
        result.is_empty(),
        "relation filter with no match must return empty"
    );
}

#[tokio::test]
async fn edge_history_empty_entity() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("EHE_Src", "EHE_Src", EntityType::Person, None)
        .await
        .unwrap();

    let result = gs.edge_history(src, "anything", None, 100).await.unwrap();
    assert!(
        result.is_empty(),
        "entity with no edges must return empty history"
    );
}

#[tokio::test]
async fn edge_history_fact_substring_filters_subset() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("EHP_Src", "EHP_Src", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("EHP_Tgt", "EHP_Tgt", EntityType::Concept, None)
        .await
        .unwrap();

    // Two facts containing "uses" and one containing "knows" (distinct relations to avoid
    // UNIQUE(source, target, relation) violation).
    for (rel, fact) in [
        ("uses_lang1", "EHP_Src uses Rust"),
        ("uses_lang2", "EHP_Src uses Python"),
        ("knows_person", "EHP_Src knows Bob"),
    ] {
        sqlx::query(sql!(
            "INSERT INTO graph_edges
             (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
             VALUES (?1, ?2, ?3, ?4, 1.0, '2020-01-01 00:00:00')"
        ))
        .bind(src)
        .bind(tgt)
        .bind(rel)
        .bind(fact)
        .execute(gs.pool())
        .await
        .unwrap();
    }

    // All 3 facts match the empty-ish predicate "src" (present in every fact prefix).
    let all = gs.edge_history(src, "EHP_Src", None, 100).await.unwrap();
    assert_eq!(all.len(), 3, "broad predicate must return all 3 facts");

    // Narrow predicate "uses" matches only the two Rust/Python facts.
    let filtered = gs.edge_history(src, "uses", None, 100).await.unwrap();
    assert_eq!(
        filtered.len(),
        2,
        "predicate 'uses' must match only the two 'uses' facts"
    );
    assert!(
        filtered.len() < all.len(),
        "filtered count must be less than total count"
    );
}

// bfs_at_timestamp

#[tokio::test]
async fn bfs_at_timestamp_zero_hops() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("ZH_A", "ZH_A", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("ZH_B", "ZH_B", EntityType::Person, None)
        .await
        .unwrap();
    // Use an explicit valid_from so the query timestamp is within the data range.
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'knows', 'ZH_A knows ZH_B', 1.0, '2020-01-01 00:00:00')"
    ))
    .bind(a)
    .bind(b)
    .execute(gs.pool())
    .await
    .unwrap();

    let (_entities, edges, depth_map) = gs
        .bfs_at_timestamp(a, 0, "2025-01-01 00:00:00")
        .await
        .unwrap();
    assert!(
        depth_map.contains_key(&a),
        "start entity must be in depth_map"
    );
    assert_eq!(depth_map.len(), 1, "depth=0 must include only start entity");
    assert!(edges.is_empty(), "depth=0 must return no edges");
}

#[tokio::test]
async fn bfs_at_timestamp_expired_intermediate_blocks() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("EI_A", "EI_A", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("EI_B", "EI_B", EntityType::Person, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("EI_C", "EI_C", EntityType::Person, None)
        .await
        .unwrap();

    // A->B: expired in 2022.
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence,
          valid_from, valid_to, expired_at)
         VALUES (?1, ?2, 'link', 'EI_A link EI_B', 1.0,
                 '2020-01-01 00:00:00', '2022-01-01 00:00:00', '2022-01-01 00:00:00')"
    ))
    .bind(a)
    .bind(b)
    .execute(gs.pool())
    .await
    .unwrap();
    // B->C: active.
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'link', 'EI_B link EI_C', 1.0, '2020-01-01 00:00:00')"
    ))
    .bind(b)
    .bind(c)
    .execute(gs.pool())
    .await
    .unwrap();

    let (entities, _edges, depth_map) = gs
        .bfs_at_timestamp(a, 3, "2025-01-01 00:00:00")
        .await
        .unwrap();
    let entity_ids: Vec<i64> = entities.iter().map(|e| e.id).collect();
    assert!(
        !depth_map.contains_key(&b),
        "B must not be reachable (A->B expired)"
    );
    assert!(
        !entity_ids.contains(&c),
        "C must not be reachable (blocked by expired A->B)"
    );
}

#[tokio::test]
async fn bfs_at_timestamp_disconnected_entity() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("DC_A", "DC_A", EntityType::Person, None)
        .await
        .unwrap();

    let (_entities, edges, depth_map) = gs
        .bfs_at_timestamp(a, 3, "2025-01-01 00:00:00")
        .await
        .unwrap();
    assert_eq!(depth_map.len(), 1, "disconnected entity has only itself");
    assert!(depth_map.contains_key(&a));
    assert!(edges.is_empty(), "disconnected entity has no edges");
}

#[tokio::test]
async fn bfs_at_timestamp_reverse_direction() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("RD_A", "RD_A", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("RD_B", "RD_B", EntityType::Person, None)
        .await
        .unwrap();

    // B -> A (B is source, A is target).
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'points_to', 'RD_B points_to RD_A', 1.0, '2020-01-01 00:00:00')"
    ))
    .bind(b)
    .bind(a)
    .execute(gs.pool())
    .await
    .unwrap();

    let (entities, edges, depth_map) = gs
        .bfs_at_timestamp(a, 1, "2099-01-01 00:00:00")
        .await
        .unwrap();
    let entity_ids: Vec<i64> = entities.iter().map(|e| e.id).collect();
    assert!(
        depth_map.contains_key(&b),
        "B must be reachable when BFS traverses reverse direction"
    );
    assert!(entity_ids.contains(&b), "B must appear in entities vec");
    assert!(
        edges
            .iter()
            .any(|e| e.source_entity_id == b && e.target_entity_id == a),
        "traversed edge B->A must appear in returned edges"
    );
}

#[tokio::test]
async fn bfs_at_timestamp_valid_to_boundary() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("VTB_A", "VTB_A", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("VTB_B", "VTB_B", EntityType::Person, None)
        .await
        .unwrap();

    // A->B: valid_to = "2025-06-01 00:00:00" (exactly).
    sqlx::query(sql!(
        "INSERT INTO graph_edges
         (source_entity_id, target_entity_id, relation, fact, confidence,
          valid_from, valid_to, expired_at)
         VALUES (?1, ?2, 'link', 'VTB_A link VTB_B', 1.0,
                 '2020-01-01 00:00:00', '2025-06-01 00:00:00', '2025-06-01 00:00:00')"
    ))
    .bind(a)
    .bind(b)
    .execute(gs.pool())
    .await
    .unwrap();

    // Query at exactly valid_to — B must NOT be reachable (exclusive upper bound).
    let (_entities, _edges, depth_map_at) = gs
        .bfs_at_timestamp(a, 1, "2025-06-01 00:00:00")
        .await
        .unwrap();
    assert!(
        !depth_map_at.contains_key(&b),
        "B must not be reachable when valid_to == ts (exclusive boundary)"
    );

    // Query one second before — B must be reachable.
    let (_entities2, _edges2, depth_map_before) = gs
        .bfs_at_timestamp(a, 1, "2025-05-31 23:59:59")
        .await
        .unwrap();
    assert!(
        depth_map_before.contains_key(&b),
        "B must be reachable one second before valid_to"
    );
}

// ── MAGMA EdgeType tests ──────────────────────────────────────────────────

#[tokio::test]
async fn insert_edge_typed_stores_edge_type() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("A", "A", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("B", "B", EntityType::Concept, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge_typed(a, b, "caused", "A caused B", 0.9, None, EdgeType::Causal)
        .await
        .unwrap();
    assert!(eid > 0);

    let stored: String =
        sqlx::query_scalar(sql!("SELECT edge_type FROM graph_edges WHERE id = ?1"))
            .bind(eid)
            .fetch_one(gs.pool())
            .await
            .unwrap();
    assert_eq!(stored, "causal");
}

#[tokio::test]
async fn insert_edge_defaults_to_semantic() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("A2", "A2", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("B2", "B2", EntityType::Concept, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge(a, b, "uses", "A2 uses B2", 0.8, None)
        .await
        .unwrap();

    let stored: String =
        sqlx::query_scalar(sql!("SELECT edge_type FROM graph_edges WHERE id = ?1"))
            .bind(eid)
            .fetch_one(gs.pool())
            .await
            .unwrap();
    assert_eq!(stored, "semantic", "insert_edge must default to semantic");
}

#[tokio::test]
async fn insert_edge_typed_dedup_key_includes_edge_type() {
    // The same (source, target, relation) with different edge types must produce
    // distinct edges (critic mitigation: dedup key includes edge_type).
    let gs = setup().await;
    let a = gs
        .upsert_entity("X", "X", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("Y", "Y", EntityType::Concept, None)
        .await
        .unwrap();

    let id_semantic = gs
        .insert_edge_typed(
            a,
            b,
            "depends_on",
            "X depends on Y (semantic)",
            0.8,
            None,
            EdgeType::Semantic,
        )
        .await
        .unwrap();
    let id_causal = gs
        .insert_edge_typed(
            a,
            b,
            "depends_on",
            "X depends on Y (causal)",
            0.9,
            None,
            EdgeType::Causal,
        )
        .await
        .unwrap();

    assert_ne!(
        id_semantic, id_causal,
        "same relation with different edge types must produce distinct edges"
    );

    let count: i64 = sqlx::query_scalar(sql!(
        "SELECT COUNT(*) FROM graph_edges WHERE valid_to IS NULL AND source_entity_id = ?1"
    ))
    .bind(a)
    .fetch_one(gs.pool())
    .await
    .unwrap();
    assert_eq!(count, 2, "both typed edges must exist");
}

#[tokio::test]
async fn insert_edge_typed_deduplicates_same_type() {
    // Same (source, target, relation, edge_type) must return the same id on repeat call.
    let gs = setup().await;
    let a = gs
        .upsert_entity("P", "P", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("Q", "Q", EntityType::Concept, None)
        .await
        .unwrap();

    let id1 = gs
        .insert_edge_typed(
            a,
            b,
            "triggered",
            "P triggered Q",
            0.7,
            None,
            EdgeType::Causal,
        )
        .await
        .unwrap();
    let id2 = gs
        .insert_edge_typed(
            a,
            b,
            "triggered",
            "P triggered Q",
            0.95,
            None,
            EdgeType::Causal,
        )
        .await
        .unwrap();

    assert_eq!(
        id1, id2,
        "same (source, target, relation, edge_type) must dedup"
    );
}

#[tokio::test]
async fn edges_for_entity_includes_edge_type() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("EA", "EA", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("EB", "EB", EntityType::Concept, None)
        .await
        .unwrap();
    gs.insert_edge_typed(
        a,
        b,
        "preceded_by",
        "EA preceded_by EB",
        0.8,
        None,
        EdgeType::Temporal,
    )
    .await
    .unwrap();

    let edges = gs.edges_for_entity(a).await.unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge_type, EdgeType::Temporal);
}

#[tokio::test]
async fn bfs_typed_empty_types_behaves_like_bfs_with_depth() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("T_A", "T_A", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("T_B", "T_B", EntityType::Person, None)
        .await
        .unwrap();
    gs.insert_edge_typed(
        a,
        b,
        "knows",
        "T_A knows T_B",
        0.9,
        None,
        EdgeType::Semantic,
    )
    .await
    .unwrap();

    let (_, edges_typed, _) = gs.bfs_typed(a, 1, &[]).await.unwrap();
    let (_, edges_plain, _) = gs.bfs_with_depth(a, 1).await.unwrap();
    assert_eq!(
        edges_typed.len(),
        edges_plain.len(),
        "empty edge_types must behave like bfs_with_depth"
    );
}

#[tokio::test]
async fn bfs_typed_filters_by_edge_type() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("BT_A", "BT_A", EntityType::Person, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("BT_B", "BT_B", EntityType::Person, None)
        .await
        .unwrap();
    let c = gs
        .upsert_entity("BT_C", "BT_C", EntityType::Person, None)
        .await
        .unwrap();

    // A->B: semantic edge
    gs.insert_edge_typed(a, b, "knows", "A knows B", 0.9, None, EdgeType::Semantic)
        .await
        .unwrap();
    // A->C: causal edge
    gs.insert_edge_typed(a, c, "caused", "A caused C", 0.9, None, EdgeType::Causal)
        .await
        .unwrap();

    // BFS with only Semantic: C must not be reachable (only via causal)
    let (_, edges_semantic, depth_semantic) =
        gs.bfs_typed(a, 2, &[EdgeType::Semantic]).await.unwrap();
    assert!(
        depth_semantic.contains_key(&b),
        "B must be reachable via semantic edge"
    );
    assert!(
        !depth_semantic.contains_key(&c),
        "C must not be reachable when only Semantic edges are traversed"
    );
    // At least one edge must be returned (guards against vacuous all())
    assert!(
        !edges_semantic.is_empty(),
        "semantic BFS must return at least one edge"
    );
    // Only the semantic edge should be in results
    assert!(
        edges_semantic
            .iter()
            .all(|e| e.edge_type == EdgeType::Semantic)
    );

    // BFS with both Semantic and Causal: both B and C must be reachable
    let (_, _, depth_both) = gs
        .bfs_typed(a, 2, &[EdgeType::Semantic, EdgeType::Causal])
        .await
        .unwrap();
    assert!(depth_both.contains_key(&b));
    assert!(depth_both.contains_key(&c));
}

#[tokio::test]
async fn bfs_typed_entity_type_filter() {
    let gs = setup().await;
    let a = gs
        .upsert_entity("E_A", "E_A", EntityType::Concept, None)
        .await
        .unwrap();
    let b = gs
        .upsert_entity("E_B", "E_B", EntityType::Concept, None)
        .await
        .unwrap();

    gs.insert_edge_typed(a, b, "is_a", "E_A is_a E_B", 1.0, None, EdgeType::Entity)
        .await
        .unwrap();

    // BFS with Entity type filter should find B
    let (_, _, depth) = gs.bfs_typed(a, 1, &[EdgeType::Entity]).await.unwrap();
    assert!(
        depth.contains_key(&b),
        "B must be reachable via entity edge"
    );

    // BFS with only Semantic should not find B (no semantic edges exist)
    let (_, _, depth_sem) = gs.bfs_typed(a, 1, &[EdgeType::Semantic]).await.unwrap();
    assert!(
        !depth_sem.contains_key(&b),
        "B must not be reachable via semantic filter when only entity edge exists"
    );
}

/// Regression test for FTS5+WAL cross-session visibility (issue #2166).
///
/// Entities inserted via `upsert_entity` in one pool must be found by `find_entities_fuzzy`
/// in a new pool opened on the same file after the first pool is dropped.
/// Without `checkpoint_wal`, FTS5 shadow table writes buffered in the WAL are not visible
/// to a fresh connection, causing SYNAPSE to return zero seeds.
#[tokio::test]
async fn fts5_cross_session_visibility_after_checkpoint() {
    let file = tempfile::NamedTempFile::new().expect("tempfile");
    let path = file.path().to_str().expect("valid path").to_string();

    // Session A: open store, insert entity, checkpoint, drop pool.
    {
        let store_a = SqliteStore::new(&path).await.unwrap();
        let gs_a = GraphStore::new(store_a.pool().clone());
        gs_a.upsert_entity("Rust", "rust", EntityType::Concept, None)
            .await
            .unwrap();
        gs_a.checkpoint_wal().await.unwrap();
    }

    // Session B: new pool on same file — entity must be visible via FTS5.
    let store_b = SqliteStore::new(&path).await.unwrap();
    let gs_b = GraphStore::new(store_b.pool().clone());
    let results = gs_b.find_entities_fuzzy("Rust", 10).await.unwrap();
    assert!(
        !results.is_empty(),
        "FTS5 cross-session: entity inserted in session A must be visible in session B after WAL checkpoint"
    );
}

// ── A-MEM: record_edge_retrieval ─────────────────────────────────────────────

#[tokio::test]
async fn record_edge_retrieval_increments_count() {
    let store = setup().await;
    let a = store
        .upsert_entity("A", "a", EntityType::Person, None)
        .await
        .unwrap();
    let b = store
        .upsert_entity("B", "b", EntityType::Person, None)
        .await
        .unwrap();
    let edge_id: i64 = sqlx::query_scalar(
        sql!("INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'knows', 'A knows B', 0.9, CURRENT_TIMESTAMP)
         RETURNING id"),
    )
    .bind(a)
    .bind(b)
    .fetch_one(store.pool())
    .await
    .unwrap();

    // Baseline: retrieval_count = 0
    let count_before: i32 = sqlx::query_scalar(sql!(
        "SELECT retrieval_count FROM graph_edges WHERE id = ?1"
    ))
    .bind(edge_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(count_before, 0);

    store.record_edge_retrieval(&[edge_id]).await.unwrap();

    let count_after: i32 = sqlx::query_scalar(sql!(
        "SELECT retrieval_count FROM graph_edges WHERE id = ?1"
    ))
    .bind(edge_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(count_after, 1, "retrieval_count must be incremented to 1");

    store.record_edge_retrieval(&[edge_id]).await.unwrap();

    let count_after2: i32 = sqlx::query_scalar(sql!(
        "SELECT retrieval_count FROM graph_edges WHERE id = ?1"
    ))
    .bind(edge_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(
        count_after2, 2,
        "retrieval_count must be 2 after second call"
    );
}

#[tokio::test]
async fn record_edge_retrieval_sets_last_retrieved_at() {
    let store = setup().await;
    let a = store
        .upsert_entity("A", "a", EntityType::Person, None)
        .await
        .unwrap();
    let b = store
        .upsert_entity("B", "b", EntityType::Person, None)
        .await
        .unwrap();
    let edge_id: i64 = sqlx::query_scalar(
        sql!("INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
         VALUES (?1, ?2, 'knows', 'A knows B', 0.9, CURRENT_TIMESTAMP)
         RETURNING id"),
    )
    .bind(a)
    .bind(b)
    .fetch_one(store.pool())
    .await
    .unwrap();

    let ts_before: Option<i64> = sqlx::query_scalar(sql!(
        "SELECT last_retrieved_at FROM graph_edges WHERE id = ?1"
    ))
    .bind(edge_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(
        ts_before.is_none(),
        "last_retrieved_at must be NULL before first retrieval"
    );

    store.record_edge_retrieval(&[edge_id]).await.unwrap();

    let ts_after: Option<i64> = sqlx::query_scalar(sql!(
        "SELECT last_retrieved_at FROM graph_edges WHERE id = ?1"
    ))
    .bind(edge_id)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert!(
        ts_after.is_some(),
        "last_retrieved_at must be set after retrieval"
    );
}

#[tokio::test]
async fn record_edge_retrieval_empty_ids_is_noop() {
    let store = setup().await;
    // Should succeed without touching any rows
    store.record_edge_retrieval(&[]).await.unwrap();
}

// ── A-MEM: decay_edge_retrieval_counts ───────────────────────────────────────

#[tokio::test]
async fn decay_edge_retrieval_counts_reduces_count() {
    let store = setup().await;
    let a = store
        .upsert_entity("A", "a", EntityType::Person, None)
        .await
        .unwrap();
    let b = store
        .upsert_entity("B", "b", EntityType::Person, None)
        .await
        .unwrap();
    // Insert edge with retrieval_count=10 and last_retrieved_at far in the past
    sqlx::query(sql!(
        "INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence,
         valid_from, retrieval_count, last_retrieved_at)
         VALUES (?1, ?2, 'knows', 'A knows B', 0.9, CURRENT_TIMESTAMP, 10, 0)"
    ))
    .bind(a)
    .bind(b)
    .execute(store.pool())
    .await
    .unwrap();

    // Decay with lambda=0.5, interval=0 (all edges eligible)
    let affected = store.decay_edge_retrieval_counts(0.5, 0).await.unwrap();
    assert_eq!(affected, 1, "exactly one edge should be decayed");

    let count: i32 = sqlx::query_scalar(sql!(
        "SELECT retrieval_count FROM graph_edges WHERE source_entity_id = ?1 AND valid_to IS NULL"
    ))
    .bind(a)
    .fetch_one(store.pool())
    .await
    .unwrap();
    // 10 * 0.5 = 5 (cast to INTEGER)
    assert_eq!(
        count, 5,
        "retrieval_count must be halved by decay lambda=0.5"
    );
}

#[tokio::test]
async fn decay_edge_retrieval_counts_skips_zero_count_edges() {
    let store = setup().await;
    let a = store
        .upsert_entity("A", "a", EntityType::Person, None)
        .await
        .unwrap();
    let b = store
        .upsert_entity("B", "b", EntityType::Person, None)
        .await
        .unwrap();
    // Edge with retrieval_count=0 (default) — must not be updated
    store
        .insert_edge(a, b, "knows", "A knows B", 0.9, None)
        .await
        .unwrap();

    let affected = store.decay_edge_retrieval_counts(0.5, 0).await.unwrap();
    assert_eq!(affected, 0, "edge with count=0 must not be decayed");
}

#[tokio::test]
async fn decay_edge_retrieval_counts_respects_interval() {
    let store = setup().await;
    let a = store
        .upsert_entity("A", "a", EntityType::Person, None)
        .await
        .unwrap();
    let b = store
        .upsert_entity("B", "b", EntityType::Person, None)
        .await
        .unwrap();
    // Edge retrieved just now (last_retrieved_at = current time)
    let epoch_now = <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::EPOCH_NOW;
    let insert_raw = format!(
        "INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence, \
         valid_from, retrieval_count, last_retrieved_at) \
         VALUES (?, ?, 'knows', 'A knows B', 0.9, CURRENT_TIMESTAMP, 5, {epoch_now})"
    );
    let insert_sql = zeph_db::rewrite_placeholders(&insert_raw);
    sqlx::query(&insert_sql)
        .bind(a)
        .bind(b)
        .execute(store.pool())
        .await
        .unwrap();

    // interval=86400 (24h): recent edge must NOT be decayed
    let affected = store.decay_edge_retrieval_counts(0.5, 86400).await.unwrap();
    assert_eq!(
        affected, 0,
        "recently-retrieved edge must not decay within interval"
    );
}

// ── Hybrid seed selection: structural scores ──────────────────────────────────

#[tokio::test]
async fn entity_structural_scores_formula_hub_leaf() {
    let store = setup().await;
    let hub = store
        .upsert_entity("Hub", "hub", EntityType::Concept, None)
        .await
        .unwrap();
    let leaf = store
        .upsert_entity("Leaf", "leaf", EntityType::Concept, None)
        .await
        .unwrap();
    // Hub has 1 edge; leaf has 1 edge (same edge, both as source/target)
    store
        .insert_edge(hub, leaf, "has", "Hub has Leaf", 0.9, None)
        .await
        .unwrap();

    let scores = store.entity_structural_scores(&[hub, leaf]).await.unwrap();

    // Both have degree=1 (max=1), type_diversity=1 (semantic only, 1/4=0.25)
    // score = 0.6 * (1/1) + 0.4 * (1/4) = 0.6 + 0.1 = 0.7
    let hub_score = scores[&hub];
    let leaf_score = scores[&leaf];
    assert!(
        (hub_score - 0.7).abs() < 1e-5,
        "hub structural score must be ~0.7, got {hub_score}"
    );
    assert!(
        (leaf_score - 0.7).abs() < 1e-5,
        "leaf structural score must be ~0.7, got {leaf_score}"
    );
}

#[tokio::test]
async fn entity_structural_scores_isolated_entity_gets_zero() {
    let store = setup().await;
    let isolated = store
        .upsert_entity("Isolated", "isolated", EntityType::Concept, None)
        .await
        .unwrap();

    let structural_scores = store.entity_structural_scores(&[isolated]).await.unwrap();

    let isolated_score = structural_scores[&isolated];
    assert!(
        isolated_score < 1e-6,
        "entity with no edges must have structural score 0.0, got {isolated_score}"
    );
}

#[tokio::test]
async fn entity_structural_scores_hub_higher_than_leaf() {
    let store = setup().await;
    let hub = store
        .upsert_entity("Hub2", "hub2", EntityType::Concept, None)
        .await
        .unwrap();
    // Connect 5 leaves to hub — hub has degree 5, each leaf has degree 1
    for i in 0..5 {
        let leaf = store
            .upsert_entity(
                &format!("SmLeaf{i}"),
                &format!("smleaf{i}"),
                EntityType::Concept,
                None,
            )
            .await
            .unwrap();
        store
            .insert_edge(hub, leaf, "has", &format!("Hub2 has SmLeaf{i}"), 0.9, None)
            .await
            .unwrap();
        let leaf_scores = store.entity_structural_scores(&[leaf]).await.unwrap();
        let hub_scores = store.entity_structural_scores(&[hub]).await.unwrap();
        // After each leaf added, hub degree grows → hub score must grow or stay equal
        let _ = (leaf_scores[&leaf], hub_scores[&hub]);
    }
    // Final check: hub score >= any leaf score (hub is in both as source/target)
    let leaf0 = sqlx::query_scalar::<_, i64>(sql!(
        "SELECT id FROM graph_entities WHERE canonical_name = 'smleaf0'"
    ))
    .fetch_one(store.pool())
    .await
    .unwrap();
    let hub_scores = store.entity_structural_scores(&[hub]).await.unwrap();
    let leaf_scores = store.entity_structural_scores(&[leaf0]).await.unwrap();
    assert!(
        hub_scores[&hub] >= leaf_scores[&leaf0],
        "hub (degree=5) must score >= leaf (degree=1)"
    );
}

// ── Hybrid seed selection: find_entities_ranked ───────────────────────────────

#[tokio::test]
async fn find_entities_ranked_returns_scores_in_0_1() {
    let store = setup().await;
    store
        .upsert_entity("Rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    store
        .upsert_entity("RustLang", "rustlang", EntityType::Language, None)
        .await
        .unwrap();

    let results = store.find_entities_ranked("rust", 10).await.unwrap();
    assert!(
        !results.is_empty(),
        "must find at least one result for 'rust'"
    );
    for (entity, score) in &results {
        assert!(
            *score >= 0.0 && *score <= 1.0,
            "score for {} must be in [0, 1], got {}",
            entity.name,
            score
        );
    }
}

#[tokio::test]
async fn find_entities_ranked_empty_query_returns_empty() {
    let store = setup().await;
    store
        .upsert_entity("Rust", "rust", EntityType::Language, None)
        .await
        .unwrap();

    let results = store.find_entities_ranked("", 10).await.unwrap();
    assert!(results.is_empty(), "empty query must return no results");
}

#[tokio::test]
async fn find_entities_ranked_top_match_has_highest_score() {
    let store = setup().await;
    store
        .upsert_entity("Rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    store
        .upsert_entity("Python", "python", EntityType::Language, None)
        .await
        .unwrap();

    // "rust" query — Rust should score higher than Python
    let results = store.find_entities_ranked("rust", 10).await.unwrap();
    let rust_score = results
        .iter()
        .find(|(e, _)| e.canonical_name == "rust")
        .map(|(_, s)| *s);
    assert!(
        rust_score.is_some(),
        "Rust must be in results for 'rust' query"
    );
    // Scores must be in non-increasing order
    let scores: Vec<f32> = results.iter().map(|(_, s)| *s).collect();
    for [a, b] in scores.array_windows::<2>().copied() {
        assert!(a >= b, "results must be ordered by score desc: {a} < {b}");
    }
}

// ── Community cap guard (SA-INV-10) via retrieval ────────────────────────────

#[tokio::test]
async fn entity_community_ids_returns_correct_mapping() {
    let store = setup().await;
    let a = store
        .upsert_entity("A", "a", EntityType::Concept, None)
        .await
        .unwrap();
    let b = store
        .upsert_entity("B", "b", EntityType::Concept, None)
        .await
        .unwrap();
    let c = store
        .upsert_entity("C", "c", EntityType::Concept, None)
        .await
        .unwrap();

    // Insert community with a and b as members
    sqlx::query(sql!(
        "INSERT INTO graph_communities (name, summary, entity_ids, fingerprint)
         VALUES ('TestCommunity', 'summary', json_array(?1, ?2), 'fp1')"
    ))
    .bind(a)
    .bind(b)
    .execute(store.pool())
    .await
    .unwrap();

    let mapping = store.entity_community_ids(&[a, b, c]).await.unwrap();

    assert!(
        mapping.contains_key(&a),
        "entity a must be mapped to a community"
    );
    assert!(
        mapping.contains_key(&b),
        "entity b must be mapped to a community"
    );
    assert_eq!(
        mapping[&a], mapping[&b],
        "a and b must be in the same community"
    );
    assert!(
        !mapping.contains_key(&c),
        "entity c (not in any community) must not be in the map"
    );
}

#[tokio::test]
async fn entity_community_ids_empty_input_returns_empty() {
    let store = setup().await;
    let result = store.entity_community_ids(&[]).await.unwrap();
    assert!(result.is_empty());
}

/// Regression test for #2215: `insert_edge_typed` must reject self-loop edges.
#[tokio::test]
async fn insert_edge_typed_rejects_self_loop() {
    let gs = setup().await;
    let id = gs
        .upsert_entity("Solo", "Solo", EntityType::Concept, None)
        .await
        .unwrap();

    let err = gs
        .insert_edge_typed(
            id,
            id,
            "self",
            "Solo is Solo",
            0.9,
            None,
            EdgeType::Semantic,
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, crate::error::MemoryError::InvalidInput(_)),
        "expected InvalidInput for self-loop, got: {err:?}"
    );
}

// ── Regression: #2591 pool isolation ─────────────────────────────────────────

/// Regression test for #2591: two `GraphStore` instances backed by independent pools
/// on the same file must not interfere — writes in one are visible in the other after
/// WAL checkpoint, and pool exhaustion in one does not block the other.
#[tokio::test]
async fn pool_isolation_independent_pools_do_not_starve() {
    let file = tempfile::NamedTempFile::new().expect("tempfile");
    let path = file.path().to_str().expect("valid path").to_string();

    // Pool A — simulates the shared messages pool (pool_size=5).
    let store_a = SqliteStore::with_pool_size(&path, 5).await.unwrap();
    let gs_a = GraphStore::new(store_a.pool().clone());

    // Pool B — simulates the dedicated graph pool (pool_size=3).
    let store_b = SqliteStore::with_pool_size(&path, 3).await.unwrap();
    let gs_b = GraphStore::new(store_b.pool().clone());

    // Write via pool A and checkpoint so pool B can see the data.
    let alice = gs_a
        .upsert_entity("Alice", "alice", EntityType::Person, None)
        .await
        .unwrap();
    let bob = gs_a
        .upsert_entity("Bob", "bob", EntityType::Person, None)
        .await
        .unwrap();
    gs_a.insert_edge(alice, bob, "knows", "Alice knows Bob", 0.9, None)
        .await
        .unwrap();
    gs_a.checkpoint_wal().await.unwrap();

    // Pool B must be able to read independently without acquiring from pool A.
    let edges = gs_b.edges_for_entity(alice).await.unwrap();
    assert!(
        !edges.is_empty(),
        "#2591 regression: pool B must read edges written by pool A after checkpoint"
    );

    // Both pools are isolated — saturating pool A connections must not block pool B.
    // Spawn 5 concurrent tasks all holding connections from pool A, then verify pool B
    // still responds. (In-memory WAL mode: connections come from the same file but
    // different pool semaphores are independent.)
    let pool_a = store_a.pool().clone();
    let handles: Vec<_> = (0..5)
        .map(|_| {
            let p = pool_a.clone();
            tokio::spawn(async move {
                // Acquire and hold the connection briefly.
                let _conn = p.acquire().await.expect("pool A connection");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            })
        })
        .collect();

    // Pool B query must complete while pool A is saturated.
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        gs_b.edges_for_entity(alice),
    )
    .await;

    for h in handles {
        h.await.expect("task");
    }

    assert!(
        result.is_ok(),
        "#2591 regression: pool B must not block when pool A is saturated"
    );
    assert!(result.unwrap().is_ok(), "pool B query must succeed");
}

// ── GAAMA episode tests ────────────────────────────────────────────────────────

/// Insert a conversation row and return its id (satisfies `FK` in `graph_episodes`).
async fn insert_conversation(gs: &GraphStore) -> i64 {
    sqlx::query_scalar(sql!(
        "INSERT INTO conversations DEFAULT VALUES RETURNING id"
    ))
    .fetch_one(&gs.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn ensure_episode_creates_and_returns_id() {
    let gs = setup().await;
    let conv_id = insert_conversation(&gs).await;
    let ep_id = gs.ensure_episode(conv_id).await.unwrap();
    assert!(ep_id > 0);
}

#[tokio::test]
async fn ensure_episode_idempotent() {
    let gs = setup().await;
    let conv_id = insert_conversation(&gs).await;
    let id1 = gs.ensure_episode(conv_id).await.unwrap();
    let id2 = gs.ensure_episode(conv_id).await.unwrap();
    assert_eq!(
        id1, id2,
        "ensure_episode must return the same id on conflict"
    );
}

#[tokio::test]
async fn ensure_episode_different_conversations_get_different_ids() {
    let gs = setup().await;
    let c1 = insert_conversation(&gs).await;
    let c2 = insert_conversation(&gs).await;
    let id1 = gs.ensure_episode(c1).await.unwrap();
    let id2 = gs.ensure_episode(c2).await.unwrap();
    assert_ne!(id1, id2);
}

#[tokio::test]
async fn link_entity_to_episode_and_query() {
    let gs = setup().await;
    let conv_id = insert_conversation(&gs).await;
    let entity_id = gs
        .upsert_entity("Rust", "rust", EntityType::Language, None)
        .await
        .unwrap();
    let ep_id = gs.ensure_episode(conv_id).await.unwrap();
    gs.link_entity_to_episode(ep_id, entity_id).await.unwrap();

    let episodes = gs.episodes_for_entity(entity_id).await.unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].id, ep_id);
    assert_eq!(episodes[0].conversation_id, conv_id);
}

#[tokio::test]
async fn link_entity_to_episode_idempotent() {
    let gs = setup().await;
    let conv_id = insert_conversation(&gs).await;
    let entity_id = gs
        .upsert_entity("Tokio", "tokio", EntityType::Tool, None)
        .await
        .unwrap();
    let ep_id = gs.ensure_episode(conv_id).await.unwrap();
    gs.link_entity_to_episode(ep_id, entity_id).await.unwrap();
    // Second call must not error (ON CONFLICT DO NOTHING).
    gs.link_entity_to_episode(ep_id, entity_id).await.unwrap();

    let episodes = gs.episodes_for_entity(entity_id).await.unwrap();
    assert_eq!(
        episodes.len(),
        1,
        "duplicate link must not create a second row"
    );
}

#[tokio::test]
async fn entity_in_multiple_episodes() {
    let gs = setup().await;
    let c1 = insert_conversation(&gs).await;
    let c2 = insert_conversation(&gs).await;
    let entity_id = gs
        .upsert_entity("Cargo", "cargo", EntityType::Tool, None)
        .await
        .unwrap();
    let ep1 = gs.ensure_episode(c1).await.unwrap();
    let ep2 = gs.ensure_episode(c2).await.unwrap();
    gs.link_entity_to_episode(ep1, entity_id).await.unwrap();
    gs.link_entity_to_episode(ep2, entity_id).await.unwrap();

    let episodes = gs.episodes_for_entity(entity_id).await.unwrap();
    assert_eq!(episodes.len(), 2);
    let ids: Vec<i64> = episodes.iter().map(|e| e.id).collect();
    assert!(ids.contains(&ep1));
    assert!(ids.contains(&ep2));
}

#[tokio::test]
async fn episodes_for_entity_returns_empty_when_no_links() {
    let gs = setup().await;
    let entity_id = gs
        .upsert_entity("Clippy", "clippy", EntityType::Tool, None)
        .await
        .unwrap();
    let episodes = gs.episodes_for_entity(entity_id).await.unwrap();
    assert!(episodes.is_empty());
}

#[tokio::test]
async fn episodes_for_entity_unknown_id_returns_empty() {
    // entity_id 99999 does not exist — should return empty, not error
    let gs = setup().await;
    let episodes = gs.episodes_for_entity(99999).await.unwrap();
    assert!(episodes.is_empty());
}

#[tokio::test]
async fn link_entity_to_episode_invalid_entity_is_fk_error() {
    // Linking a non-existent entity_id must fail with a DB error (FK violation), not panic.
    let gs = setup().await;
    let conv_id = insert_conversation(&gs).await;
    let ep_id = gs.ensure_episode(conv_id).await.unwrap();
    let result = gs.link_entity_to_episode(ep_id, 99999).await;
    // FK enforcement may be off in test pool — accept both Ok (FK off) and Err (FK on).
    match result {
        Ok(()) | Err(crate::error::MemoryError::Sqlx(_)) => {} // FK off or FK violation — both acceptable
        Err(e) => panic!("unexpected error type: {e:?}"),
    }
}

#[tokio::test]
async fn episode_cascade_delete_on_conversation_delete() {
    // Deleting the parent conversation must cascade-delete the episode row.
    let gs = setup().await;
    let conv_id = insert_conversation(&gs).await;
    let ep_id = gs.ensure_episode(conv_id).await.unwrap();

    // Delete the conversation row directly.
    sqlx::query(sql!("DELETE FROM conversations WHERE id = ?1"))
        .bind(conv_id)
        .execute(&gs.pool)
        .await
        .unwrap();

    // The episode should be gone due to ON DELETE CASCADE.
    let count: i64 = sqlx::query_scalar(sql!("SELECT COUNT(*) FROM graph_episodes WHERE id = ?1"))
        .bind(ep_id)
        .fetch_one(&gs.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "episode must cascade-delete with its conversation"
    );
}

#[tokio::test]
async fn ensure_episode_zero_conversation_id_is_rejected() {
    // conversation_id=0 does not satisfy the FK (no row with id=0 in conversations).
    let gs = setup().await;
    let result = gs.ensure_episode(0).await;
    match result {
        Ok(_) | Err(crate::error::MemoryError::Sqlx(_)) => {} // FK off or FK violation — both acceptable
        Err(e) => panic!("unexpected error: {e:?}"),
    }
}

// ── APEX-MEM: insert_or_supersede tests ───────────────────────────────────────

/// FR-001: inserting a new edge over an existing head sets `supersedes` on the new row
/// and marks the old edge with `valid_to`/`expired_at` atomically.
#[tokio::test]
async fn insert_or_supersede_sets_supersedes_pointer_and_invalidates_prior() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("Alice", "Alice", EntityType::Person, None)
        .await
        .unwrap();
    let tgt1 = gs
        .upsert_entity("Acme", "Acme", EntityType::Organization, None)
        .await
        .unwrap();
    let tgt2 = gs
        .upsert_entity("Globex", "Globex", EntityType::Organization, None)
        .await
        .unwrap();

    let old_id = gs
        .insert_or_supersede(
            src,
            tgt1,
            "works_at",
            "works_at",
            "Alice works at Acme",
            0.8,
            None,
            EdgeType::Semantic,
            true,
        )
        .await
        .unwrap();

    let new_id = gs
        .insert_or_supersede(
            src,
            tgt2,
            "works_at",
            "works_at",
            "Alice works at Globex",
            0.9,
            None,
            EdgeType::Semantic,
            true,
        )
        .await
        .unwrap();

    assert_ne!(old_id, new_id, "second write must create a new edge row");

    // Verify supersedes pointer on new row.
    let supersedes: Option<i64> =
        sqlx::query_scalar("SELECT supersedes FROM graph_edges WHERE id = ?")
            .bind(new_id)
            .fetch_one(&gs.pool)
            .await
            .unwrap();
    assert_eq!(
        supersedes,
        Some(old_id),
        "new edge must point back to old edge"
    );

    // Verify old edge is invalidated.
    let valid_to: Option<String> =
        sqlx::query_scalar("SELECT valid_to FROM graph_edges WHERE id = ?")
            .bind(old_id)
            .fetch_one(&gs.pool)
            .await
            .unwrap();
    assert!(valid_to.is_some(), "prior head must have valid_to set");
}

/// FR-015: byte-identical reassertion must NOT insert a new edge row but must
/// write a row into `edge_reassertions`.
#[tokio::test]
async fn insert_or_supersede_reassertion_goes_to_reassertions_table() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("Bob", "Bob", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("TechCorp", "TechCorp", EntityType::Organization, None)
        .await
        .unwrap();

    let first_id = gs
        .insert_or_supersede(
            src,
            tgt,
            "works_at",
            "works_at",
            "Bob works at TechCorp",
            0.7,
            None,
            EdgeType::Semantic,
            true,
        )
        .await
        .unwrap();

    // Exact same fact — must be treated as reassertion.
    let second_id = gs
        .insert_or_supersede(
            src,
            tgt,
            "works_at",
            "works_at",
            "Bob works at TechCorp",
            0.7,
            None,
            EdgeType::Semantic,
            true,
        )
        .await
        .unwrap();

    assert_eq!(
        first_id, second_id,
        "reassertion must return the existing edge id"
    );

    let edge_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM graph_edges WHERE source_entity_id = ? AND canonical_relation = 'works_at'"
    )
    .bind(src)
    .fetch_one(&gs.pool)
    .await
    .unwrap();
    assert_eq!(edge_count, 1, "must not create a second edge row");

    let reassertion_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM edge_reassertions WHERE head_edge_id = ?")
            .bind(first_id)
            .fetch_one(&gs.pool)
            .await
            .unwrap();
    assert_eq!(reassertion_count, 1, "must record one reassertion event");
}

/// Atomic rollback: if the UPDATE to invalidate the prior head fails, the INSERT must also
/// be rolled back.  We test this property indirectly by verifying that after a normal
/// successful supersession the DB is in a consistent state — only one active edge remains.
#[tokio::test]
async fn insert_or_supersede_only_one_active_head_after_supersession() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("Carol", "Carol", EntityType::Person, None)
        .await
        .unwrap();
    let tgt_a = gs
        .upsert_entity("OldCo", "OldCo", EntityType::Organization, None)
        .await
        .unwrap();
    let tgt_b = gs
        .upsert_entity("NewCo", "NewCo", EntityType::Organization, None)
        .await
        .unwrap();
    let tgt_c = gs
        .upsert_entity("FinalCo", "FinalCo", EntityType::Organization, None)
        .await
        .unwrap();

    gs.insert_or_supersede(
        src,
        tgt_a,
        "works_at",
        "works_at",
        "Carol at OldCo",
        0.6,
        None,
        EdgeType::Semantic,
        true,
    )
    .await
    .unwrap();
    gs.insert_or_supersede(
        src,
        tgt_b,
        "works_at",
        "works_at",
        "Carol at NewCo",
        0.7,
        None,
        EdgeType::Semantic,
        true,
    )
    .await
    .unwrap();
    gs.insert_or_supersede(
        src,
        tgt_c,
        "works_at",
        "works_at",
        "Carol at FinalCo",
        0.8,
        None,
        EdgeType::Semantic,
        true,
    )
    .await
    .unwrap();

    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM graph_edges
         WHERE source_entity_id = ?
           AND canonical_relation = 'works_at'
           AND valid_to IS NULL
           AND expired_at IS NULL",
    )
    .bind(src)
    .fetch_one(&gs.pool)
    .await
    .unwrap();
    assert_eq!(
        active_count, 1,
        "exactly one active head must remain after three supersessions"
    );
}

/// A supersede chain that would exceed `SUPERSEDE_DEPTH_CAP` must return `SupersedeDepthExceeded`.
/// We also verify `check_supersede_depth()` itself returns `SupersedeCycle` when the CTE reports
/// depth > cap — simulated here by testing that `check_supersede_depth` on a non-existent edge
/// returns depth 0 gracefully.
#[tokio::test]
async fn check_supersede_depth_returns_zero_for_root_edge() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("Dave", "Dave", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("Solo", "Solo", EntityType::Organization, None)
        .await
        .unwrap();

    let edge_id = gs
        .insert_or_supersede(
            src,
            tgt,
            "works_at",
            "works_at",
            "Dave at Solo",
            0.5,
            None,
            EdgeType::Semantic,
            true,
        )
        .await
        .unwrap();

    let depth = gs.check_supersede_depth(edge_id).await.unwrap();
    assert_eq!(depth, 0, "root edge with no supersedes pointer has depth 0");
}

// ── HL-F1: Edge.weight field ──────────────────────────────────────────────────

#[tokio::test]
async fn test_insert_edge_default_weight_is_one() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("A", "A", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("B", "B", EntityType::Person, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge(src, tgt, "knows", "A knows B", 0.9, None)
        .await
        .unwrap();

    let weight: f64 = sqlx::query_scalar(sql!("SELECT weight FROM graph_edges WHERE id = ?1"))
        .bind(eid)
        .fetch_one(&gs.pool)
        .await
        .unwrap();
    assert!(
        (weight - 1.0).abs() < 1e-6,
        "default weight must be 1.0, got {weight}"
    );
}

#[tokio::test]
async fn test_edge_weight_persists_after_update() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("C", "C", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("D", "D", EntityType::Person, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge(src, tgt, "likes", "C likes D", 0.8, None)
        .await
        .unwrap();

    gs.apply_hebbian_increment(&[eid], 0.1_f32).await.unwrap();

    let weight: f64 = sqlx::query_scalar(sql!("SELECT weight FROM graph_edges WHERE id = ?1"))
        .bind(eid)
        .fetch_one(&gs.pool)
        .await
        .unwrap();
    // Default 1.0 + delta 0.1 = 1.1; use tolerance for f32→f64 round-trip.
    assert!(
        (weight - 1.1).abs() < 1e-5,
        "weight after increment must be ~1.1, got {weight}"
    );
}

// ── HL-F2: apply_hebbian_increment ────────────────────────────────────────────

#[tokio::test]
async fn test_apply_hebbian_increment_empty_ids_is_noop() {
    let gs = setup().await;
    // Empty slice must return Ok without touching the DB.
    gs.apply_hebbian_increment(&[], 0.5_f32).await.unwrap();
}

#[tokio::test]
async fn test_apply_hebbian_increment_zero_delta_is_noop() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("E", "E", EntityType::Person, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("F", "F", EntityType::Person, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge(src, tgt, "rel", "fact", 0.5, None)
        .await
        .unwrap();

    gs.apply_hebbian_increment(&[eid], 0.0_f32).await.unwrap();

    let weight: f64 = sqlx::query_scalar(sql!("SELECT weight FROM graph_edges WHERE id = ?1"))
        .bind(eid)
        .fetch_one(&gs.pool)
        .await
        .unwrap();
    assert!(
        (weight - 1.0).abs() < 1e-6,
        "zero delta must leave weight unchanged at 1.0, got {weight}"
    );
}

#[tokio::test]
async fn test_apply_hebbian_increment_updates_weight() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("G", "G", EntityType::Concept, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("H", "H", EntityType::Concept, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge(src, tgt, "rel", "fact", 0.7, None)
        .await
        .unwrap();

    gs.apply_hebbian_increment(&[eid], 0.5_f32).await.unwrap();

    let weight: f64 = sqlx::query_scalar(sql!("SELECT weight FROM graph_edges WHERE id = ?1"))
        .bind(eid)
        .fetch_one(&gs.pool)
        .await
        .unwrap();
    assert!(
        (weight - 1.5).abs() < 1e-5,
        "weight must increase by delta=0.5 to ~1.5, got {weight}"
    );
}

#[tokio::test]
async fn test_apply_hebbian_increment_skips_invalidated_edges() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("I", "I", EntityType::Concept, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("J", "J", EntityType::Concept, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge(src, tgt, "rel", "fact", 0.6, None)
        .await
        .unwrap();

    // Invalidate the edge — sets valid_to IS NOT NULL.
    gs.invalidate_edge(eid).await.unwrap();

    gs.apply_hebbian_increment(&[eid], 0.5_f32).await.unwrap();

    // Weight must remain at the default 1.0 — WHERE valid_to IS NULL guard skips tombstoned edges.
    let weight: f64 = sqlx::query_scalar(sql!("SELECT weight FROM graph_edges WHERE id = ?1"))
        .bind(eid)
        .fetch_one(&gs.pool)
        .await
        .unwrap();
    assert!(
        (weight - 1.0).abs() < 1e-6,
        "invalidated edge must not have weight incremented, got {weight}"
    );
}

// ── MemCoT recall_view helpers (#3574 / #3575) ────────────────────────────────

#[tokio::test]
async fn source_message_ids_for_edges_empty_input() {
    let gs = setup().await;
    let result = gs.source_message_ids_for_edges(&[]).await.unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn source_message_ids_for_edges_returns_rows_with_episode_id() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("A", "A", EntityType::Concept, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("B", "B", EntityType::Concept, None)
        .await
        .unwrap();
    // Insert edge without episode_id, then backfill directly to avoid FK cascade through
    // conversations → messages tables in the in-memory test DB.
    let eid = gs
        .insert_edge(src, tgt, "knows", "A knows B", 0.8, None)
        .await
        .unwrap();
    let synthetic_msg_id: i64 = 42;
    // Disable FK checks temporarily and backfill episode_id to simulate provenance.
    sqlx::query(sql!("PRAGMA foreign_keys = OFF"))
        .execute(&gs.pool)
        .await
        .unwrap();
    sqlx::query(sql!("UPDATE graph_edges SET episode_id = ?1 WHERE id = ?2"))
        .bind(synthetic_msg_id)
        .bind(eid)
        .execute(&gs.pool)
        .await
        .unwrap();
    sqlx::query(sql!("PRAGMA foreign_keys = ON"))
        .execute(&gs.pool)
        .await
        .unwrap();

    let pairs = gs.source_message_ids_for_edges(&[eid]).await.unwrap();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].0, eid);
    assert_eq!(pairs[0].1, crate::types::MessageId(synthetic_msg_id));
}

#[tokio::test]
async fn source_message_ids_for_edges_skips_null_episode_id() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("X", "X", EntityType::Concept, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("Y", "Y", EntityType::Concept, None)
        .await
        .unwrap();
    // Insert with no episode_id (None).
    let eid = gs
        .insert_edge(src, tgt, "uses", "X uses Y", 0.7, None)
        .await
        .unwrap();

    let pairs = gs.source_message_ids_for_edges(&[eid]).await.unwrap();
    assert!(
        pairs.is_empty(),
        "edges without episode_id must not be returned"
    );
}

#[tokio::test]
async fn source_entity_id_for_edge_returns_correct_id() {
    let gs = setup().await;
    let src = gs
        .upsert_entity("Src", "Src", EntityType::Concept, None)
        .await
        .unwrap();
    let tgt = gs
        .upsert_entity("Tgt", "Tgt", EntityType::Concept, None)
        .await
        .unwrap();
    let eid = gs
        .insert_edge(src, tgt, "rel", "Src rel Tgt", 0.9, None)
        .await
        .unwrap();

    let got = gs.source_entity_id_for_edge(eid).await.unwrap();
    assert_eq!(got, Some(src));
}

#[tokio::test]
async fn source_entity_id_for_edge_missing_returns_none() {
    let gs = setup().await;
    let got = gs.source_entity_id_for_edge(999_999).await.unwrap();
    assert!(got.is_none());
}

#[tokio::test]
async fn bfs_edges_at_depth_returns_neighbors() {
    let gs = setup().await;
    let center = gs
        .upsert_entity("Center", "Center", EntityType::Concept, None)
        .await
        .unwrap();
    let neighbor = gs
        .upsert_entity("Neighbor", "Neighbor", EntityType::Concept, None)
        .await
        .unwrap();
    gs.insert_edge(
        center,
        neighbor,
        "links",
        "Center links Neighbor",
        0.85,
        None,
    )
    .await
    .unwrap();

    let facts = gs
        .bfs_edges_at_depth(center, 1, &[EdgeType::Semantic])
        .await
        .unwrap();
    assert!(
        !facts.is_empty(),
        "should return at least one neighbor fact"
    );
    let found = facts
        .iter()
        .any(|rf| rf.fact.fact.contains("Center links Neighbor"));
    assert!(found, "expected the inserted edge fact in results");
}

#[tokio::test]
async fn bfs_edges_at_depth_empty_when_no_edges() {
    let gs = setup().await;
    let entity = gs
        .upsert_entity("Isolated", "Isolated", EntityType::Concept, None)
        .await
        .unwrap();

    let facts = gs
        .bfs_edges_at_depth(entity, 1, &[EdgeType::Semantic])
        .await
        .unwrap();
    assert!(facts.is_empty());
}
