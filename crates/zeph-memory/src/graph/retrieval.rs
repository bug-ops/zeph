// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};

use crate::error::MemoryError;

use super::store::GraphStore;
use super::types::GraphFact;

/// Retrieve graph facts relevant to `query` via BFS traversal from matched seed entities.
///
/// Algorithm:
/// 1. Split query into words and search for entity matches via fuzzy LIKE for each word.
/// 2. For each matched seed entity, run BFS up to `max_hops` hops.
/// 3. Build `GraphFact` structs from edges, using depth map for `hop_distance`.
/// 4. Deduplicate by `(entity_name, relation, target_name)` keeping highest `composite_score`.
/// 5. Sort by `composite_score` desc, truncate to `limit`.
///
/// # Errors
///
/// Returns an error if any database query fails.
#[cfg(feature = "graph-memory")]
pub async fn graph_recall(
    store: &GraphStore,
    _embeddings: Option<&crate::embedding_store::EmbeddingStore>,
    _provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    max_hops: u32,
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

    // Step 3: BFS from each seed entity, collect facts
    let mut all_facts: Vec<GraphFact> = Vec::new();

    for (seed_id, seed_score) in &entity_scores {
        let (entities, edges, depth_map) = store.bfs_with_depth(*seed_id, max_hops).await?;

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
            });
        }
    }

    // Step 4 & 5: sort by composite_score desc (total_cmp for deterministic NaN ordering),
    // then dedup keeping highest-scored fact per (entity, relation, target) key.
    all_facts.sort_by(|a, b| b.composite_score().total_cmp(&a.composite_score()));

    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    all_facts.retain(|f| {
        seen.insert((
            f.entity_name.clone(),
            f.relation.clone(),
            f.target_name.clone(),
        ))
    });

    // Step 6: truncate to limit
    all_facts.truncate(limit);

    Ok(all_facts)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "graph-memory")]
    use super::*;
    #[cfg(feature = "graph-memory")]
    use crate::graph::store::GraphStore;
    #[cfg(feature = "graph-memory")]
    use crate::graph::types::EntityType;
    #[cfg(feature = "graph-memory")]
    use crate::sqlite::SqliteStore;
    #[cfg(feature = "graph-memory")]
    use zeph_llm::any::AnyProvider;
    #[cfg(feature = "graph-memory")]
    use zeph_llm::mock::MockProvider;

    #[cfg(feature = "graph-memory")]
    async fn setup_store() -> GraphStore {
        let store = SqliteStore::new(":memory:").await.unwrap();
        GraphStore::new(store.pool().clone())
    }

    #[cfg(feature = "graph-memory")]
    fn mock_provider() -> AnyProvider {
        AnyProvider::Mock(MockProvider::default())
    }

    #[tokio::test]
    #[cfg(feature = "graph-memory")]
    async fn graph_recall_empty_graph_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall(&store, None, &provider, "anything", 10, 2)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    #[cfg(feature = "graph-memory")]
    async fn graph_recall_zero_limit_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall(&store, None, &provider, "user", 0, 2)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    #[cfg(feature = "graph-memory")]
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
        let result = graph_recall(&store, None, &provider, "Ali neovim", 10, 2)
            .await
            .unwrap();
        assert!(!result.is_empty());
        assert_eq!(result[0].relation, "uses");
    }

    #[tokio::test]
    #[cfg(feature = "graph-memory")]
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
        let result = graph_recall(&store, None, &provider, "Alp", 10, 1)
            .await
            .unwrap();
        // Should find A→B edge, but not B→C (which is hop 2 from A)
        assert!(result.iter().all(|f| f.hop_distance <= 1));
    }

    #[tokio::test]
    #[cfg(feature = "graph-memory")]
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
        let result = graph_recall(&store, None, &provider, "Ali Bob", 10, 2)
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
    #[cfg(feature = "graph-memory")]
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
        let result = graph_recall(&store, None, &provider, "Alp", 10, 2)
            .await
            .unwrap();

        // First result should have higher composite score than second
        assert!(result.len() >= 2);
        let s0 = result[0].composite_score();
        let s1 = result[1].composite_score();
        assert!(s0 >= s1, "expected sorted desc: {s0} >= {s1}");
    }

    #[tokio::test]
    #[cfg(feature = "graph-memory")]
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
        let result = graph_recall(&store, None, &provider, "Roo", 3, 2)
            .await
            .unwrap();
        assert!(result.len() <= 3);
    }
}
