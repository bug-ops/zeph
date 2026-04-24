// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A* shortest-path graph recall via `petgraph`.
//!
//! [`graph_recall_astar`] seeds from fuzzy entity matches and uses A* (with a
//! zero heuristic, degrading to Dijkstra) to collect the shortest paths from
//! each seed to all reachable nodes within `max_hops`.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use petgraph::algo::astar;
use petgraph::graph::{NodeIndex, UnGraph};

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::graph::retrieval::find_seed_entities;
use crate::graph::store::GraphStore;
use crate::graph::types::{EdgeType, GraphFact};

const DEFAULT_STRUCTURAL_WEIGHT: f32 = 0.4;
const DEFAULT_COMMUNITY_CAP: usize = 3;

/// Retrieve graph facts using A* shortest-path traversal.
///
/// Algorithm:
/// 1. Find seed entities via hybrid FTS5 + structural scoring.
/// 2. Fetch all edges from seeds via BFS (up to `max_hops`).
/// 3. Build an in-memory `petgraph::UnGraph` from collected edges.
/// 4. Run A* from each seed; collect path edges.
/// 5. Convert to [`GraphFact`], dedup, sort by score, truncate to `limit`.
///
/// The A* heuristic is always `0.0` (admissible, degrades to Dijkstra when
/// embedding distances are unavailable). Edge cost = `1.0 - confidence`.
///
/// # Errors
///
/// Returns an error if any database query fails.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn graph_recall_astar(
    store: &GraphStore,
    embeddings: Option<&EmbeddingStore>,
    provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    max_hops: u32,
    edge_types: &[EdgeType],
    temporal_decay_rate: f64,
    hebbian_enabled: bool,
    hebbian_lr: f32,
) -> Result<Vec<GraphFact>, MemoryError> {
    let _span = tracing::info_span!("memory.graph.astar", query_len = query.len()).entered();

    if limit == 0 {
        return Ok(Vec::new());
    }

    let entity_scores = find_seed_entities(
        store,
        embeddings,
        provider,
        query,
        limit,
        DEFAULT_STRUCTURAL_WEIGHT,
        DEFAULT_COMMUNITY_CAP,
    )
    .await?;

    if entity_scores.is_empty() {
        return Ok(Vec::new());
    }

    let now_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed());

    // Gather all edges reachable from all seeds.
    let mut all_db_edges = Vec::new();
    let mut entity_name_map: HashMap<i64, String> = HashMap::new();

    for &seed_id in entity_scores.keys() {
        let (entities, edges, _depth_map) = if edge_types.is_empty() {
            store.bfs_with_depth(seed_id, max_hops).await?
        } else {
            store.bfs_typed(seed_id, max_hops, edge_types).await?
        };
        for e in &entities {
            entity_name_map
                .entry(e.id)
                .or_insert_with(|| e.canonical_name.clone());
        }
        all_db_edges.extend(edges);
    }

    if all_db_edges.is_empty() {
        return Ok(Vec::new());
    }

    // Build petgraph: node index ↔ entity_id mapping.
    let mut node_map: HashMap<i64, NodeIndex> = HashMap::new();
    let mut id_map: Vec<i64> = Vec::new();
    let mut graph: UnGraph<i64, f32> = UnGraph::new_undirected();

    let get_or_add = |graph: &mut UnGraph<i64, f32>,
                      node_map: &mut HashMap<i64, NodeIndex>,
                      id_map: &mut Vec<i64>,
                      entity_id: i64|
     -> NodeIndex {
        *node_map.entry(entity_id).or_insert_with(|| {
            let idx = graph.add_node(entity_id);
            id_map.push(entity_id);
            idx
        })
    };

    for edge in &all_db_edges {
        let src = get_or_add(
            &mut graph,
            &mut node_map,
            &mut id_map,
            edge.source_entity_id,
        );
        let tgt = get_or_add(
            &mut graph,
            &mut node_map,
            &mut id_map,
            edge.target_entity_id,
        );
        // Cost: low-confidence edges are more expensive.
        let cost = 1.0 - edge.confidence.clamp(0.0, 1.0);
        graph.add_edge(src, tgt, cost);
    }

    // Run A* from each seed; collect path node pairs.
    let mut path_pairs: HashSet<(NodeIndex, NodeIndex)> = HashSet::new();

    for &seed_id in entity_scores.keys() {
        let Some(&seed_idx) = node_map.get(&seed_id) else {
            continue;
        };
        for &target_idx in node_map.values() {
            if target_idx == seed_idx {
                continue;
            }
            if let Some((_cost, path)) = astar(
                &graph,
                seed_idx,
                |n| n == target_idx,
                |e| *e.weight(),
                |_| 0.0,
            ) {
                for window in path.windows(2) {
                    let (a, b) = (window[0], window[1]);
                    let pair = if a.index() < b.index() {
                        (a, b)
                    } else {
                        (b, a)
                    };
                    path_pairs.insert(pair);
                }
            }
        }
    }

    // Build a lookup of db edges by (src_id, tgt_id).
    let edge_lookup: HashMap<(i64, i64), &crate::graph::types::Edge> = all_db_edges
        .iter()
        .map(|e| ((e.source_entity_id, e.target_entity_id), e))
        .collect();

    let mut facts: Vec<GraphFact> = Vec::new();
    let mut seen: HashSet<(String, String, String, EdgeType)> = HashSet::new();

    for (a_idx, b_idx) in &path_pairs {
        let a_id = id_map[a_idx.index()];
        let b_id = id_map[b_idx.index()];

        // Try both directions (undirected graph, but db edges are directed).
        for (src_id, tgt_id) in [(a_id, b_id), (b_id, a_id)] {
            if let Some(&edge) = edge_lookup.get(&(src_id, tgt_id)) {
                let entity_name = entity_name_map.get(&src_id).cloned().unwrap_or_default();
                let target_name = entity_name_map.get(&tgt_id).cloned().unwrap_or_default();
                if entity_name.is_empty() || target_name.is_empty() {
                    continue;
                }
                let key = (
                    entity_name.clone(),
                    edge.relation.clone(),
                    target_name.clone(),
                    edge.edge_type,
                );
                if seen.insert(key) {
                    let seed_score = entity_scores.get(&src_id).copied().unwrap_or(0.5);
                    facts.push(GraphFact {
                        entity_name,
                        relation: edge.relation.clone(),
                        target_name,
                        fact: edge.fact.clone(),
                        entity_match_score: seed_score,
                        hop_distance: 1,
                        confidence: edge.confidence,
                        valid_from: Some(edge.valid_from.clone()),
                        edge_type: edge.edge_type,
                        retrieval_count: edge.retrieval_count,
                    });
                }
            }
        }
    }

    // Sort by decayed score descending, truncate to limit.
    facts.sort_by(|a, b| {
        let sa = a.score_with_decay(temporal_decay_rate, now_secs);
        let sb = b.score_with_decay(temporal_decay_rate, now_secs);
        sb.total_cmp(&sa)
    });
    facts.truncate(limit);

    // Record retrievals fire-and-forget.
    let edge_ids: Vec<i64> = all_db_edges.iter().map(|e| e.id).collect();
    if let Err(e) = store.record_edge_retrieval(&edge_ids).await {
        tracing::warn!(error = %e, "graph_recall_astar: failed to record edge retrieval");
    }
    // HL-F2: Hebbian weight reinforcement (fire-and-forget).
    if hebbian_enabled
        && !edge_ids.is_empty()
        && let Err(e) = store.apply_hebbian_increment(&edge_ids, hebbian_lr).await
    {
        tracing::warn!(error = %e, "graph_recall_astar: hebbian increment failed");
    }

    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::store::GraphStore;
    use crate::graph::types::EntityType;
    use crate::store::SqliteStore;
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
    async fn astar_empty_graph_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall_astar(
            &store,
            None,
            &provider,
            "anything",
            10,
            2,
            &[],
            0.0,
            false,
            0.0,
        )
        .await
        .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn astar_zero_limit_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall_astar(
            &store,
            None,
            &provider,
            "anything",
            0,
            2,
            &[],
            0.0,
            false,
            0.0,
        )
        .await
        .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn astar_finds_direct_edge() {
        let store = setup_store().await;
        let a = store
            .upsert_entity("Alice", "alice", EntityType::Person, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("Bob", "bob", EntityType::Person, None)
            .await
            .unwrap();
        store
            .insert_edge(a, b, "knows", "Alice knows Bob", 0.9, None)
            .await
            .unwrap();

        let provider = mock_provider();
        let result = graph_recall_astar(
            &store,
            None,
            &provider,
            "Alice",
            10,
            2,
            &[],
            0.0,
            false,
            0.0,
        )
        .await
        .unwrap();
        assert!(!result.is_empty());
    }
}
