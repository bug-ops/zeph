// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Concentric BFS (`WaterCircles`) graph recall.
//!
//! [`graph_recall_watercircles`] performs ring-by-ring BFS from seed entities,
//! capping facts per ring independently before concatenating rings and truncating
//! to the global `limit`. This yields a more balanced cross-hop distribution than
//! plain BFS.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::graph::retrieval::find_seed_entities;
use crate::graph::store::GraphStore;
use crate::graph::types::{EdgeType, GraphFact};

const DEFAULT_STRUCTURAL_WEIGHT: f32 = 0.4;
const DEFAULT_COMMUNITY_CAP: usize = 3;

/// Retrieve graph facts using concentric BFS (`WaterCircles`).
///
/// Algorithm:
/// 1. Find seed entities via hybrid FTS5 + structural scoring.
/// 2. BFS ring by ring: for each hop depth, fetch edges at exactly that depth.
/// 3. Score edges; cap each ring independently at `ring_limit` (auto when `ring_limit = 0`).
/// 4. Concatenate rings; dedup; sort by score; truncate to `limit`.
///
/// # Errors
///
/// Returns an error if any database query fails.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
// complex algorithm function; both suppressions justified until the function is decomposed in a future refactor
#[tracing::instrument(skip_all, name = "memory.graph.watercircles", fields(query_len = query.len()))]
pub async fn graph_recall_watercircles(
    store: &GraphStore,
    embeddings: Option<&EmbeddingStore>,
    provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    max_hops: u32,
    ring_limit: usize,
    edge_types: &[EdgeType],
    temporal_decay_rate: f64,
    hebbian_enabled: bool,
    hebbian_lr: f32,
    embed_timeout: std::time::Duration,
) -> Result<Vec<GraphFact>, MemoryError> {
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

    // Auto ring_limit: distribute limit evenly across hops.
    let effective_ring_limit = if ring_limit == 0 {
        let hops = max_hops.max(1) as usize;
        (limit / hops).max(1)
    } else {
        ring_limit
    };

    let mut all_facts: Vec<GraphFact> = Vec::new();
    let mut global_seen: HashSet<(String, String, String, EdgeType)> = HashSet::new();

    // Process each hop ring independently per seed.
    for hop in 1..=max_hops {
        let mut ring_facts: Vec<(f32, GraphFact)> = Vec::new();

        for (&seed_id, &seed_score) in &entity_scores {
            let (entities, edges, depth_map) = if edge_types.is_empty() {
                store.bfs_with_depth(seed_id, hop).await?
            } else {
                store.bfs_typed(seed_id, hop, edge_types).await?
            };

            let name_map: HashMap<i64, &str> = entities
                .iter()
                .map(|e| (e.id.0, e.canonical_name.as_str()))
                .collect();

            let traversed_ids: Vec<i64> = edges.iter().map(|e| e.id).collect();

            for edge in &edges {
                // An edge's ring is determined by its farther endpoint from the seed: `bfs_fetch_results`
                // returns every edge between two visited entities regardless of traversal direction, so
                // taking only one side's depth (e.g. always source) misclassifies edges discovered in the
                // opposite orientation. `max` correctly identifies the newly-reached ring on either side.
                let hop_dist = match (
                    depth_map.get(&edge.source_entity_id).copied(),
                    depth_map.get(&edge.target_entity_id).copied(),
                ) {
                    (Some(source_depth), Some(target_depth)) => {
                        Some(source_depth.max(target_depth))
                    }
                    (source_depth, target_depth) => source_depth.or(target_depth),
                };
                let Some(dist) = hop_dist else { continue };
                if dist != hop {
                    continue;
                }

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

                let fact = GraphFact {
                    entity_name: entity_name.to_owned(),
                    relation: edge.relation.clone(),
                    target_name: target_name.to_owned(),
                    fact: edge.fact.clone(),
                    entity_match_score: seed_score,
                    hop_distance: dist,
                    confidence: edge.confidence,
                    valid_from: Some(edge.valid_from.clone()),
                    edge_type: edge.edge_type,
                    retrieval_count: edge.retrieval_count,
                    edge_id: Some(edge.id),
                };
                let fact_score = fact.score_with_decay(temporal_decay_rate, now_secs);
                ring_facts.push((fact_score, fact));
            }

            if !traversed_ids.is_empty()
                && let Err(e) = store.record_edge_retrieval(&traversed_ids).await
            {
                tracing::warn!(
                    error = %e,
                    "graph_recall_watercircles: failed to record edge retrieval"
                );
            }
            // HL-F2: Hebbian weight reinforcement (fire-and-forget).
            if hebbian_enabled
                && !traversed_ids.is_empty()
                && let Err(e) = store
                    .apply_hebbian_increment(&traversed_ids, hebbian_lr)
                    .await
            {
                tracing::warn!(error = %e, "graph_recall_watercircles: hebbian increment failed");
            }
        }

        // Sort ring by score, cap, then add to global list (deduplicating).
        ring_facts.sort_by(|(sa, _), (sb, _)| sb.total_cmp(sa));
        ring_facts.truncate(effective_ring_limit);

        for (_, fact) in ring_facts {
            let key = (
                fact.entity_name.clone(),
                fact.relation.clone(),
                fact.target_name.clone(),
                fact.edge_type,
            );
            if global_seen.insert(key) {
                all_facts.push(fact);
            }
        }
    }

    // Final sort and truncation.
    all_facts.sort_by(|a, b| {
        let sa = a.score_with_decay(temporal_decay_rate, now_secs);
        let sb = b.score_with_decay(temporal_decay_rate, now_secs);
        sb.total_cmp(&sa)
    });
    all_facts.truncate(limit);

    Ok(all_facts)
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
    async fn watercircles_empty_graph_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall_watercircles(
            &store,
            None,
            &provider,
            "anything",
            10,
            2,
            0,
            &[],
            0.0,
            false,
            0.0,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn watercircles_zero_limit_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall_watercircles(
            &store,
            None,
            &provider,
            "anything",
            0,
            2,
            0,
            &[],
            0.0,
            false,
            0.0,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn watercircles_ring_limit_auto_respects_limit() {
        let store = setup_store().await;
        let root = store
            .upsert_entity("Root", "root", EntityType::Concept, None, None)
            .await
            .unwrap()
            .0;
        for i in 0..10usize {
            let target = store
                .upsert_entity(
                    &format!("T{i}"),
                    &format!("t{i}"),
                    EntityType::Concept,
                    None,
                    None,
                )
                .await
                .unwrap()
                .0;
            store
                .insert_edge(
                    root,
                    target,
                    "has",
                    &format!("Root has T{i}"),
                    0.8,
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let provider = mock_provider();
        let result = graph_recall_watercircles(
            &store,
            None,
            &provider,
            "Root",
            5,
            2,
            0,
            &[],
            0.0,
            false,
            0.0,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(
            !result.is_empty(),
            "ring-1 edges must be returned, not silently dropped by the hop-ring filter"
        );
        assert!(result.len() <= 5, "limit must be respected");
    }

    #[tokio::test]
    async fn watercircles_two_ring_hop_distance_matches_target_depth() {
        let store = setup_store().await;
        let root = store
            .upsert_entity("Root", "root", EntityType::Concept, None, None)
            .await
            .unwrap()
            .0;
        let a = store
            .upsert_entity("A", "A", EntityType::Concept, None, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("B", "B", EntityType::Concept, None, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(root, a, "has", "Root has A", 0.9, None, None)
            .await
            .unwrap();
        store
            .insert_edge(a, b, "has", "A has B", 0.9, None, None)
            .await
            .unwrap();

        let provider = mock_provider();
        let result = graph_recall_watercircles(
            &store,
            None,
            &provider,
            "Root",
            10,
            2,
            10,
            &[],
            0.0,
            false,
            0.0,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();

        let ring1 = result
            .iter()
            .find(|f| f.target_name == "A")
            .expect("ring-1 edge Root->A must be present");
        assert_eq!(ring1.hop_distance, 1, "Root->A must be assigned to ring 1");

        let ring2 = result
            .iter()
            .find(|f| f.target_name == "B")
            .expect("ring-2 edge A->B must be present");
        assert_eq!(ring2.hop_distance, 2, "A->B must be assigned to ring 2");
    }
}
