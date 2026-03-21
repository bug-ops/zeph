// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::MemoryError;

use super::store::GraphStore;
use super::types::{EdgeType, GraphFact};

/// Retrieve graph facts relevant to `query` via BFS traversal from matched seed entities.
///
/// Algorithm:
/// 1. Split query into words and search for entity matches via fuzzy LIKE for each word.
/// 2. For each matched seed entity, run BFS up to `max_hops` hops (temporal BFS when
///    `at_timestamp` is `Some`, typed BFS when `edge_types` is non-empty).
/// 3. Build `GraphFact` structs from edges, using depth map for `hop_distance`.
/// 4. Deduplicate by `(entity_name, relation, target_name, edge_type)` keeping highest score.
/// 5. Sort by score desc, truncate to `limit`.
///
/// # Parameters
///
/// - `at_timestamp`: `SQLite` datetime string (`"YYYY-MM-DD HH:MM:SS"`). When `Some`, only edges
///   valid at that point in time are traversed. When `None`, only currently active edges are used.
/// - `temporal_decay_rate`: non-negative decay rate (units: 1/day). `0.0` preserves the original
///   `composite_score` ordering with no temporal adjustment.
/// - `edge_types`: MAGMA subgraph filter. When non-empty, only traverses edges of the given types.
///   When empty, traverses all active edges (backward-compatible).
///
/// # Errors
///
/// Returns an error if any database query fails.
#[allow(clippy::too_many_arguments)]
pub async fn graph_recall(
    store: &GraphStore,
    _embeddings: Option<&crate::embedding_store::EmbeddingStore>,
    _provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    max_hops: u32,
    at_timestamp: Option<&str>,
    temporal_decay_rate: f64,
    edge_types: &[EdgeType],
) -> Result<Vec<GraphFact>, MemoryError> {
    // Cap at MAX_WORDS to bound the number of sequential full-table-scan LIKE queries.
    const MAX_WORDS: usize = 5;

    if limit == 0 {
        return Ok(Vec::new());
    }

    // Step 1: fuzzy search per query word (avoids full-sentence LIKE misses).
    // Fall back to the full query string when all words are too short (len < 3).
    let filtered: Vec<&str> = query
        .split_whitespace()
        .filter(|w| w.len() >= 3)
        .take(MAX_WORDS)
        .collect();
    let words: Vec<&str> = if filtered.is_empty() && !query.is_empty() {
        vec![query]
    } else {
        filtered
    };

    let mut entity_scores: HashMap<i64, f32> = HashMap::new();

    for word in &words {
        let matches = store.find_entities_fuzzy(word, limit * 2).await?;
        for entity in matches {
            entity_scores
                .entry(entity.id)
                .and_modify(|s| *s = s.max(1.0))
                .or_insert(1.0);
        }
    }

    if entity_scores.is_empty() {
        return Ok(Vec::new());
    }

    // Capture current time once for consistent decay scoring across all facts.
    let now_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().cast_signed())
        .unwrap_or(0);

    // Step 2: BFS from each seed entity, collect facts
    let mut all_facts: Vec<GraphFact> = Vec::new();

    for (seed_id, seed_score) in &entity_scores {
        let (entities, edges, depth_map) = if let Some(ts) = at_timestamp {
            store.bfs_at_timestamp(*seed_id, max_hops, ts).await?
        } else if !edge_types.is_empty() {
            store.bfs_typed(*seed_id, max_hops, edge_types).await?
        } else {
            store.bfs_with_depth(*seed_id, max_hops).await?
        };

        // Use canonical_name for stable dedup keys (S5 fix): entities reached via different
        // aliases have different display names but share canonical_name, preventing duplicates.
        let name_map: HashMap<i64, &str> = entities
            .iter()
            .map(|e| (e.id, e.canonical_name.as_str()))
            .collect();

        for edge in &edges {
            let Some(&hop_distance) = depth_map
                .get(&edge.source_entity_id)
                .or_else(|| depth_map.get(&edge.target_entity_id))
            else {
                continue;
            };

            let entity_name = name_map
                .get(&edge.source_entity_id)
                .copied()
                .unwrap_or_default();
            let target_name = name_map
                .get(&edge.target_entity_id)
                .copied()
                .unwrap_or_default();

            if entity_name.is_empty() || target_name.is_empty() {
                continue;
            }

            all_facts.push(GraphFact {
                entity_name: entity_name.to_owned(),
                relation: edge.relation.clone(),
                target_name: target_name.to_owned(),
                fact: edge.fact.clone(),
                entity_match_score: *seed_score,
                hop_distance,
                confidence: edge.confidence,
                valid_from: Some(edge.valid_from.clone()),
                edge_type: edge.edge_type,
            });
        }
    }

    // Step 3: sort by score desc (total_cmp for deterministic NaN ordering),
    // then dedup keeping highest-scored fact per (entity, relation, target) key.
    // Pre-compute scores to avoid recomputing composite_score() O(n log n) times.
    let mut scored: Vec<(f32, GraphFact)> = all_facts
        .into_iter()
        .map(|f| {
            let s = f.score_with_decay(temporal_decay_rate, now_secs);
            (s, f)
        })
        .collect();
    scored.sort_by(|(sa, _), (sb, _)| sb.total_cmp(sa));
    let mut all_facts: Vec<GraphFact> = scored.into_iter().map(|(_, f)| f).collect();

    // Dedup key includes edge_type (critic mitigation): the same (entity, relation, target)
    // triple can legitimately exist with different edge types. Without edge_type in the key,
    // typed BFS would return fewer facts than expected.
    let mut seen: HashSet<(String, String, String, EdgeType)> = HashSet::new();
    all_facts.retain(|f| {
        seen.insert((
            f.entity_name.clone(),
            f.relation.clone(),
            f.target_name.clone(),
            f.edge_type,
        ))
    });

    // Step 4: truncate to limit
    all_facts.truncate(limit);

    Ok(all_facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::store::GraphStore;
    use crate::graph::types::EntityType;
    use crate::sqlite::SqliteStore;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    async fn setup_store() -> GraphStore {
        let store = SqliteStore::new(":memory:").await.unwrap();
        GraphStore::new(store.pool().clone())
    }

    fn mock_provider() -> AnyProvider {
        AnyProvider::Mock(MockProvider::default())
    }

    #[tokio::test]
    async fn graph_recall_empty_graph_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall(&store, None, &provider, "anything", 10, 2, None, 0.0, &[])
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn graph_recall_zero_limit_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall(&store, None, &provider, "user", 0, 2, None, 0.0, &[])
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn graph_recall_fuzzy_match_returns_facts() {
        let store = setup_store().await;
        let user_id = store
            .upsert_entity("Alice", "Alice", EntityType::Person, None)
            .await
            .unwrap();
        let tool_id = store
            .upsert_entity("neovim", "neovim", EntityType::Tool, None)
            .await
            .unwrap();
        store
            .insert_edge(user_id, tool_id, "uses", "Alice uses neovim", 0.9, None)
            .await
            .unwrap();

        let provider = mock_provider();
        // "Ali" matches "Alice" via LIKE
        let result = graph_recall(&store, None, &provider, "Ali neovim", 10, 2, None, 0.0, &[])
            .await
            .unwrap();
        assert!(!result.is_empty());
        assert_eq!(result[0].relation, "uses");
    }

    #[tokio::test]
    async fn graph_recall_respects_max_hops() {
        let store = setup_store().await;
        let a = store
            .upsert_entity("Alpha", "Alpha", EntityType::Person, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("Beta", "Beta", EntityType::Person, None)
            .await
            .unwrap();
        let c = store
            .upsert_entity("Gamma", "Gamma", EntityType::Person, None)
            .await
            .unwrap();
        store
            .insert_edge(a, b, "knows", "Alpha knows Beta", 0.8, None)
            .await
            .unwrap();
        store
            .insert_edge(b, c, "knows", "Beta knows Gamma", 0.8, None)
            .await
            .unwrap();

        let provider = mock_provider();
        // max_hops=1: only the A→B edge should be reachable from A
        let result = graph_recall(&store, None, &provider, "Alp", 10, 1, None, 0.0, &[])
            .await
            .unwrap();
        // Should find A→B edge, but not B→C (which is hop 2 from A)
        assert!(result.iter().all(|f| f.hop_distance <= 1));
    }

    #[tokio::test]
    async fn graph_recall_deduplicates_facts() {
        let store = setup_store().await;
        let alice = store
            .upsert_entity("Alice", "Alice", EntityType::Person, None)
            .await
            .unwrap();
        let bob = store
            .upsert_entity("Bob", "Bob", EntityType::Person, None)
            .await
            .unwrap();
        store
            .insert_edge(alice, bob, "knows", "Alice knows Bob", 0.9, None)
            .await
            .unwrap();

        let provider = mock_provider();
        // Both "Ali" and "Bob" match and BFS from both seeds yields the same edge
        let result = graph_recall(&store, None, &provider, "Ali Bob", 10, 2, None, 0.0, &[])
            .await
            .unwrap();

        // Should not have duplicate (Alice, knows, Bob) entries
        let mut seen = std::collections::HashSet::new();
        for f in &result {
            let key = (&f.entity_name, &f.relation, &f.target_name);
            assert!(seen.insert(key), "duplicate fact found: {f:?}");
        }
    }

    #[tokio::test]
    async fn graph_recall_sorts_by_composite_score() {
        let store = setup_store().await;
        let a = store
            .upsert_entity("Alpha", "Alpha", EntityType::Person, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("Beta", "Beta", EntityType::Tool, None)
            .await
            .unwrap();
        let c = store
            .upsert_entity("AlphaGadget", "AlphaGadget", EntityType::Tool, None)
            .await
            .unwrap();
        // high-confidence direct edge
        store
            .insert_edge(a, b, "uses", "Alpha uses Beta", 1.0, None)
            .await
            .unwrap();
        // low-confidence direct edge
        store
            .insert_edge(a, c, "mentions", "Alpha mentions AlphaGadget", 0.1, None)
            .await
            .unwrap();

        let provider = mock_provider();
        let result = graph_recall(&store, None, &provider, "Alp", 10, 2, None, 0.0, &[])
            .await
            .unwrap();

        // First result should have higher composite score than second
        assert!(result.len() >= 2);
        let s0 = result[0].composite_score();
        let s1 = result[1].composite_score();
        assert!(s0 >= s1, "expected sorted desc: {s0} >= {s1}");
    }

    #[tokio::test]
    async fn graph_recall_limit_truncates() {
        let store = setup_store().await;
        let root = store
            .upsert_entity("Root", "Root", EntityType::Person, None)
            .await
            .unwrap();
        for i in 0..10 {
            let target = store
                .upsert_entity(
                    &format!("Target{i}"),
                    &format!("Target{i}"),
                    EntityType::Tool,
                    None,
                )
                .await
                .unwrap();
            store
                .insert_edge(
                    root,
                    target,
                    "has",
                    &format!("Root has Target{i}"),
                    0.8,
                    None,
                )
                .await
                .unwrap();
        }

        let provider = mock_provider();
        let result = graph_recall(&store, None, &provider, "Roo", 3, 2, None, 0.0, &[])
            .await
            .unwrap();
        assert!(result.len() <= 3);
    }

    #[tokio::test]
    async fn graph_recall_at_timestamp_excludes_future_edges() {
        let store = setup_store().await;
        let alice = store
            .upsert_entity("Alice", "Alice", EntityType::Person, None)
            .await
            .unwrap();
        let bob = store
            .upsert_entity("Bob", "Bob", EntityType::Person, None)
            .await
            .unwrap();
        // Insert an edge with valid_from = year 2100 (far future).
        sqlx::query(
            "INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
             VALUES (?1, ?2, 'knows', 'Alice knows Bob', 0.9, '2100-01-01 00:00:00')",
        )
        .bind(alice)
        .bind(bob)
        .execute(store.pool())
        .await
        .unwrap();

        let provider = mock_provider();
        // Query at 2026 — should not see the 2100 edge.
        let result = graph_recall(
            &store,
            None,
            &provider,
            "Ali",
            10,
            2,
            Some("2026-01-01 00:00:00"),
            0.0,
            &[],
        )
        .await
        .unwrap();
        assert!(result.is_empty(), "future edge should be excluded");
    }

    #[tokio::test]
    async fn graph_recall_at_timestamp_excludes_invalidated_edges() {
        let store = setup_store().await;
        let alice = store
            .upsert_entity("Alice", "Alice", EntityType::Person, None)
            .await
            .unwrap();
        let carol = store
            .upsert_entity("Carol", "Carol", EntityType::Person, None)
            .await
            .unwrap();
        // Insert an edge valid 2020-01-01 → 2021-01-01 (already expired by 2026).
        sqlx::query(
            "INSERT INTO graph_edges
             (source_entity_id, target_entity_id, relation, fact, confidence, valid_from, valid_to, expired_at)
             VALUES (?1, ?2, 'manages', 'Alice manages Carol', 0.8,
                     '2020-01-01 00:00:00', '2021-01-01 00:00:00', '2021-01-01 00:00:00')",
        )
        .bind(alice)
        .bind(carol)
        .execute(store.pool())
        .await
        .unwrap();

        let provider = mock_provider();

        // Querying at 2026 (after valid_to) → no edge
        let result_current = graph_recall(&store, None, &provider, "Ali", 10, 2, None, 0.0, &[])
            .await
            .unwrap();
        assert!(
            result_current.is_empty(),
            "expired edge should be invisible at current time"
        );

        // Querying at 2020-06-01 (during validity window) → edge visible
        let result_historical = graph_recall(
            &store,
            None,
            &provider,
            "Ali",
            10,
            2,
            Some("2020-06-01 00:00:00"),
            0.0,
            &[],
        )
        .await
        .unwrap();
        assert!(
            !result_historical.is_empty(),
            "edge should be visible within its validity window"
        );
    }

    #[tokio::test]
    async fn graph_recall_temporal_decay_preserves_order_with_zero_rate() {
        let store = setup_store().await;
        let a = store
            .upsert_entity("Alpha", "Alpha", EntityType::Person, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("Beta", "Beta", EntityType::Tool, None)
            .await
            .unwrap();
        let c = store
            .upsert_entity("AlphaGadget", "AlphaGadget", EntityType::Tool, None)
            .await
            .unwrap();
        store
            .insert_edge(a, b, "uses", "Alpha uses Beta", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(a, c, "mentions", "Alpha mentions AlphaGadget", 0.1, None)
            .await
            .unwrap();

        let provider = mock_provider();
        // With decay_rate=0.0 order must be identical to composite_score ordering.
        let result = graph_recall(&store, None, &provider, "Alp", 10, 2, None, 0.0, &[])
            .await
            .unwrap();
        assert!(result.len() >= 2);
        let s0 = result[0].composite_score();
        let s1 = result[1].composite_score();
        assert!(s0 >= s1, "expected sorted desc: {s0} >= {s1}");
    }
}
