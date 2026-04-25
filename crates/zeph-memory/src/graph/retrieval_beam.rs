// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Beam search graph recall.
//!
//! [`graph_recall_beam`] keeps only the top-K candidate entities at each hop,
//! enabling multi-hop reasoning paths to be explored efficiently without
//! unbounded BFS expansion.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::graph::retrieval::find_seed_entities;
use crate::graph::store::GraphStore;
use crate::graph::types::{EdgeType, GraphFact};

const DEFAULT_STRUCTURAL_WEIGHT: f32 = 0.4;
const DEFAULT_COMMUNITY_CAP: usize = 3;

/// Retrieve graph facts using beam search.
///
/// Algorithm:
/// 1. Find seed entities via hybrid FTS5 + structural scoring; take top `beam_width` as initial beam.
/// 2. Per hop: fetch edges for beam entities, score each neighbour, keep top `beam_width` entity IDs.
/// 3. Collect all traversed edges; convert to [`GraphFact`]; dedup; sort; truncate.
///
/// # Errors
///
/// Returns an error if any database query fails.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // complex algorithm function; both suppressions justified until the function is decomposed in a future refactor
pub async fn graph_recall_beam(
    store: &GraphStore,
    embeddings: Option<&EmbeddingStore>,
    provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    beam_width: usize,
    max_hops: u32,
    edge_types: &[EdgeType],
    temporal_decay_rate: f64,
    hebbian_enabled: bool,
    hebbian_lr: f32,
) -> Result<Vec<GraphFact>, MemoryError> {
    let _span = tracing::info_span!("memory.graph.beam", query_len = query.len()).entered();

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

    // Initial beam: top-`beam_width` seeds by score.
    let mut beam_scores: Vec<(i64, f32)> = entity_scores.into_iter().collect();
    beam_scores.sort_by(|(_, sa), (_, sb)| sb.total_cmp(sa));
    beam_scores.truncate(beam_width);

    let mut beam_ids: Vec<i64> = beam_scores.iter().map(|(id, _)| *id).collect();
    let mut beam_score_map: HashMap<i64, f32> = beam_scores.into_iter().collect();

    let mut all_db_edges: Vec<crate::graph::types::Edge> = Vec::new();
    let mut entity_name_map: HashMap<i64, String> = HashMap::new();

    for _hop in 0..max_hops {
        if beam_ids.is_empty() {
            break;
        }

        let edges = store.edges_for_entities(&beam_ids, edge_types).await?;
        if edges.is_empty() {
            break;
        }

        // Collect entity IDs from edges to resolve names.
        let new_entity_ids: Vec<i64> = edges
            .iter()
            .flat_map(|e| [e.source_entity_id, e.target_entity_id])
            .filter(|id| !entity_name_map.contains_key(id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        for id in new_entity_ids {
            if let Ok(Some(entity)) = store.find_entity_by_id(id).await {
                entity_name_map.insert(id, entity.canonical_name.clone());
            }
        }

        // Score each neighbour by edge confidence (proxy for traversal quality).
        let mut neighbour_scores: HashMap<i64, f32> = HashMap::new();
        for edge in &edges {
            let edge_conf = edge.confidence;
            neighbour_scores
                .entry(edge.target_entity_id)
                .and_modify(|s| *s = s.max(edge_conf))
                .or_insert(edge_conf);
            neighbour_scores
                .entry(edge.source_entity_id)
                .and_modify(|s| *s = s.max(edge_conf))
                .or_insert(edge_conf);
        }

        // Next beam: top-`beam_width` by score (excluding current beam members).
        let mut candidates: Vec<(i64, f32)> = neighbour_scores
            .into_iter()
            .filter(|(id, _)| !beam_score_map.contains_key(id))
            .collect();
        candidates.sort_by(|(_, sa), (_, sb)| sb.total_cmp(sa));
        candidates.truncate(beam_width);

        beam_ids = candidates.iter().map(|(id, _)| *id).collect();
        for (id, cand_score) in candidates {
            beam_score_map.insert(id, cand_score);
        }

        all_db_edges.extend(edges);
    }

    if all_db_edges.is_empty() {
        return Ok(Vec::new());
    }

    // Record retrievals fire-and-forget.
    let edge_ids: Vec<i64> = all_db_edges.iter().map(|e| e.id).collect();
    if let Err(e) = store.record_edge_retrieval(&edge_ids).await {
        tracing::warn!(error = %e, "graph_recall_beam: failed to record edge retrieval");
    }
    // HL-F2: Hebbian weight reinforcement (fire-and-forget).
    if hebbian_enabled
        && !edge_ids.is_empty()
        && let Err(e) = store.apply_hebbian_increment(&edge_ids, hebbian_lr).await
    {
        tracing::warn!(error = %e, "graph_recall_beam: hebbian increment failed");
    }

    // Convert to GraphFact, dedup, sort, truncate.
    let mut facts: Vec<GraphFact> = Vec::new();
    let mut seen: HashSet<(String, String, String, EdgeType)> = HashSet::new();

    for edge in &all_db_edges {
        let entity_name = entity_name_map
            .get(&edge.source_entity_id)
            .cloned()
            .unwrap_or_default();
        let target_name = entity_name_map
            .get(&edge.target_entity_id)
            .cloned()
            .unwrap_or_default();
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
            let seed_score = beam_score_map
                .get(&edge.source_entity_id)
                .copied()
                .unwrap_or(0.5);
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

    facts.sort_by(|a, b| {
        let sa = a.score_with_decay(temporal_decay_rate, now_secs);
        let sb = b.score_with_decay(temporal_decay_rate, now_secs);
        sb.total_cmp(&sa)
    });
    facts.truncate(limit);

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
    async fn beam_empty_graph_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall_beam(
            &store,
            None,
            &provider,
            "anything",
            10,
            5,
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
    async fn beam_zero_limit_returns_empty() {
        let store = setup_store().await;
        let provider = mock_provider();
        let result = graph_recall_beam(
            &store,
            None,
            &provider,
            "anything",
            0,
            5,
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
    async fn beam_finds_direct_edge() {
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
        let result = graph_recall_beam(
            &store,
            None,
            &provider,
            "Alice",
            10,
            5,
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
