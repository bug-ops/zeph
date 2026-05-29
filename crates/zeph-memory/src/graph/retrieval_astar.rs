// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A* shortest-path graph recall via `petgraph`.
//!
//! [`graph_recall_astar`] seeds from fuzzy entity matches and uses A* (with a
//! zero heuristic, degrading to Dijkstra) to collect the shortest paths from
//! each seed to all reachable nodes within `max_hops`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use petgraph::algo::astar;
use petgraph::graph::{NodeIndex, UnGraph};

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::graph::retrieval::find_seed_entities;
use crate::graph::store::GraphStore;
use crate::graph::types::{EdgeType, GraphFact};

const ENTITY_COLLECTION: &str = "zeph_graph_entities";

/// Maximum number of graph nodes passed to the A* traversal loop.
///
/// Without a cap the A* inner loop is O(|nodes|²) per seed, which becomes
/// prohibitively slow on large or highly-connected graphs. Nodes are ranked by
/// their seed score before truncation so only the least-relevant nodes are dropped.
const MAX_GRAPH_NODES: usize = 500;

/// Cosine similarity of two equal-length slices. Returns `0.0` when either norm is zero.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(&x, &y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    let denom = (norm_a * norm_b).max(f32::EPSILON);
    dot / denom
}

const DEFAULT_STRUCTURAL_WEIGHT: f32 = 0.4;
const DEFAULT_COMMUNITY_CAP: usize = 3;

/// Query embedding paired with per-entity embedding map, produced by the PRISM path.
type PrismEmbeddings = Option<(Vec<f32>, HashMap<i64, Vec<f32>>)>;

/// Truncate `node_map` in-place to at most `cap` entries, keeping those with the highest score.
///
/// Score is looked up from `entity_scores`; entities absent from that map receive `0.0`.
/// When `node_map.len() <= cap` the map is unchanged.
pub(crate) fn cap_node_map<V>(
    node_map: &mut HashMap<i64, V>,
    entity_scores: &HashMap<i64, f32>,
    cap: usize,
) {
    if node_map.len() <= cap {
        return;
    }
    let mut scored: Vec<(i64, f32)> = node_map
        .keys()
        .map(|&id| {
            let s = entity_scores.get(&id).copied().unwrap_or(0.0);
            (id, s)
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(cap);
    let keep: HashSet<i64> = scored.into_iter().map(|(id, _)| id).collect();
    node_map.retain(|id, _| keep.contains(id));
    tracing::debug!(
        retained = node_map.len(),
        "graph_recall_astar: node_map capped"
    );
}

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
/// embedding distances are unavailable).
///
/// When `query_sensitive_cost = false` (default): edge cost = `1.0 - confidence`.
/// When `query_sensitive_cost = true` (PRISM): edge cost =
/// `(1.0 - confidence) * (1.0 - target_cosine).max(0.01)`, where `target_cosine`
/// is the cosine similarity between the query embedding and the target entity embedding.
/// Edges toward semantically relevant entities receive lower cost, guiding A* toward
/// query-aligned paths. Falls back to `1.0 - confidence` when the embedding store is
/// unavailable or a target entity has no stored embedding.
///
/// # Errors
///
/// Returns an error if any database query fails.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // complex algorithm function; both suppressions justified until the function is decomposed in a future refactor
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
    query_sensitive_cost: bool,
    embed_timeout: std::time::Duration,
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
        embed_timeout,
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
                .entry(e.id.0)
                .or_insert_with(|| e.canonical_name.clone());
        }
        all_db_edges.extend(edges);
    }

    if all_db_edges.is_empty() {
        return Ok(Vec::new());
    }

    // PRISM: compute query embedding once and batch-fetch target entity embeddings.
    // When query_sensitive_cost is disabled or embedding store unavailable, entity_vec_map
    // is empty and edge costs fall back to `1.0 - confidence`.
    let query_vec_and_entity_embeddings: PrismEmbeddings = if query_sensitive_cost {
        match embeddings {
            Some(emb_store) => {
                const PRISM_EMBED_TIMEOUT: Duration = Duration::from_secs(10);
                let _embed_span = tracing::info_span!("memory.graph.astar.prism_embed").entered();
                match tokio::time::timeout(
                    PRISM_EMBED_TIMEOUT,
                    zeph_llm::LlmProvider::embed(provider, query),
                )
                .await
                {
                    Err(_elapsed) => {
                        tracing::warn!(
                            "prism: query embed timed out after {}s, falling back to \
                                 confidence-only cost",
                            PRISM_EMBED_TIMEOUT.as_secs()
                        );
                        None
                    }
                    Ok(Ok(q_vec)) => {
                        let entity_ids: Vec<i64> = all_db_edges
                            .iter()
                            .flat_map(|e| [e.source_entity_id, e.target_entity_id])
                            .collect::<HashSet<_>>()
                            .into_iter()
                            .collect();
                        match store.qdrant_point_ids_for_entities(&entity_ids).await {
                            Ok(point_id_map) => {
                                let point_ids: Vec<String> =
                                    point_id_map.values().cloned().collect();
                                match emb_store
                                    .get_vectors_from_collection(ENTITY_COLLECTION, &point_ids)
                                    .await
                                {
                                    Ok(vec_map) => {
                                        // Invert: entity_id → embedding vector.
                                        let entity_vecs: HashMap<i64, Vec<f32>> = entity_ids
                                            .iter()
                                            .filter_map(|&eid| {
                                                let pid = point_id_map.get(&eid)?;
                                                let v = vec_map.get(pid)?.clone();
                                                Some((eid, v))
                                            })
                                            .collect();
                                        Some((q_vec, entity_vecs))
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            "prism: failed to fetch entity vectors, \
                                             falling back to confidence-only cost"
                                        );
                                        None
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "prism: failed to fetch qdrant point ids, \
                                     falling back to confidence-only cost"
                                );
                                None
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            error = %e,
                            "prism: query embed failed, falling back to confidence-only cost"
                        );
                        None
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };

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
        // PRISM: when query_sensitive_cost is enabled and embeddings are available,
        // modulate cost by cosine similarity to the target entity, biasing A* toward
        // semantically relevant paths. Floor of 0.01 prevents zero-cost paths.
        let cost = if let Some((ref q_vec, ref entity_vecs)) = query_vec_and_entity_embeddings {
            let base = 1.0_f32 - edge.confidence.clamp(0.0, 1.0);
            if let Some(tgt_vec) = entity_vecs.get(&edge.target_entity_id) {
                let sim = cosine_similarity(q_vec, tgt_vec).clamp(0.0, 1.0);
                (base * (1.0 - sim)).max(0.01)
            } else {
                base
            }
        } else {
            1.0_f32 - edge.confidence.clamp(0.0, 1.0)
        };
        graph.add_edge(src, tgt, cost);
    }

    // Cap node_map to the highest-scored nodes to bound A* time complexity.
    cap_node_map(&mut node_map, &entity_scores, MAX_GRAPH_NODES);

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
                        edge_id: Some(edge.id),
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
            false,
            std::time::Duration::from_secs(5),
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
            false,
            std::time::Duration::from_secs(5),
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
            .unwrap()
            .0;
        let b = store
            .upsert_entity("Bob", "bob", EntityType::Person, None)
            .await
            .unwrap()
            .0;
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
            false,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(!result.is_empty());
    }

    // PRISM: query_sensitive_cost=true with no embedding store → fallback to confidence-only.
    // Verifies that enabling PRISM without an embedding store does not error and returns results
    // identical to the baseline (no panic, no empty result when edges exist).
    #[tokio::test]
    async fn astar_query_sensitive_cost_without_embeddings_falls_back() {
        let store = setup_store().await;
        let a = store
            .upsert_entity("Alice", "alice", EntityType::Person, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("Bob", "bob", EntityType::Person, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(a, b, "knows", "Alice knows Bob", 0.9, None)
            .await
            .unwrap();

        let provider = mock_provider();
        // embeddings = None → PRISM falls back to confidence-only cost.
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
            true, // query_sensitive_cost enabled but no embedding store
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        // Should return the same edge as without PRISM — no panic, no empty result.
        assert!(
            !result.is_empty(),
            "fallback to confidence-only must still return results"
        );
        assert_eq!(result[0].entity_name, "alice");
    }

    // PRISM: query_sensitive_cost=true with embedding store that has no vectors for graph entities
    // → falls back to confidence-only cost per entity. This exercises the per-entity fallback
    // branch when `entity_vecs.get(&edge.target_entity_id)` returns None.
    #[tokio::test]
    async fn astar_query_sensitive_cost_entity_missing_embedding_uses_confidence_fallback() {
        use crate::embedding_store::EmbeddingStore;
        use crate::store::SqliteStore;

        let store = setup_store().await;
        let a = store
            .upsert_entity("Alice", "alice", EntityType::Person, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("Bob", "bob", EntityType::Person, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(a, b, "knows", "Alice knows Bob", 0.8, None)
            .await
            .unwrap();

        let provider = mock_provider();

        // Build a SQLite-backed EmbeddingStore with no stored vectors.
        // qdrant_point_ids_for_entities will return an empty map, so entity_vecs will be empty,
        // and every edge will use the `1.0 - confidence` fallback cost.
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let emb_store = EmbeddingStore::new_sqlite(sqlite.pool().clone());

        let result = graph_recall_astar(
            &store,
            Some(&emb_store),
            &provider,
            "Alice",
            10,
            2,
            &[],
            0.0,
            false,
            0.0,
            true,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        // Entity embeddings missing → fallback to confidence-only → same results as baseline.
        assert!(!result.is_empty());
        assert_eq!(result[0].entity_name, "alice");
    }

    #[test]
    fn cap_node_map_truncates_to_cap_keeping_highest_scored() {
        use petgraph::graph::NodeIndex;

        let mut node_map: HashMap<i64, NodeIndex> = (0..600i64)
            .map(|id| (id, NodeIndex::new(usize::try_from(id).unwrap())))
            .collect();
        // Assign scores: entity 599 gets highest, 0 gets lowest.
        // i16 covers 0..600 exactly; the i16→f32 cast is lossless (f32 has 23 mantissa bits).
        let entity_scores: HashMap<i64, f32> = (0..600i64)
            .map(|id| (id, f32::from(i16::try_from(id).unwrap()) / 599.0))
            .collect();

        cap_node_map(&mut node_map, &entity_scores, MAX_GRAPH_NODES);

        assert_eq!(node_map.len(), MAX_GRAPH_NODES);
        // The 500 retained nodes must all have id >= 100 (top 500 by score are ids 100..=599).
        for &id in node_map.keys() {
            assert!(id >= 100, "low-scored node {id} should have been dropped");
        }
    }

    #[test]
    fn cap_node_map_noop_when_within_limit() {
        use petgraph::graph::NodeIndex;

        let mut node_map: HashMap<i64, NodeIndex> = (0..10i64)
            .map(|id| (id, NodeIndex::new(usize::try_from(id).unwrap())))
            .collect();
        let entity_scores: HashMap<i64, f32> = HashMap::new();

        cap_node_map(&mut node_map, &entity_scores, MAX_GRAPH_NODES);

        assert_eq!(node_map.len(), 10);
    }
}
