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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // complex algorithm function; both suppressions justified until the function is decomposed in a future refactor
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
) -> Result<Vec<GraphFact>, MemoryError> {
    let _span = tracing::info_span!("memory.graph.watercircles", query_len = query.len()).entered();

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
                .map(|e| (e.id, e.canonical_name.as_str()))
                .collect();

            let traversed_ids: Vec<i64> = edges.iter().map(|e| e.id).collect();

            for edge in &edges {
                // Only include edges that belong exactly to this hop ring.
                let hop_dist = depth_map
                    .get(&edge.source_entity_id)
                    .or_else(|| depth_map.get(&edge.target_entity_id))
                    .copied();
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
        )
        .await
        .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn watercircles_ring_limit_auto_respects_limit() {
        let store = setup_store().await;
        let root = store
            .upsert_entity("Root", "root", EntityType::Concept, None)
            .await
            .unwrap();
        for i in 0..10usize {
            let target = store
                .upsert_entity(
                    &format!("T{i}"),
                    &format!("t{i}"),
                    EntityType::Concept,
                    None,
                )
                .await
                .unwrap();
            store
                .insert_edge(root, target, "has", &format!("Root has T{i}"), 0.8, None)
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
        )
        .await
        .unwrap();
        assert!(result.len() <= 5, "limit must be respected");
    }
}
