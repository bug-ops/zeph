// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use futures::TryStreamExt as _;
use petgraph::Graph;
use petgraph::graph::NodeIndex;
use zeph_llm::LlmProvider as _;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{Message, Role};

use crate::error::MemoryError;

use super::store::GraphStore;

const MAX_LABEL_PROPAGATION_ITERATIONS: usize = 50;

/// Strip newlines and ASCII control characters from `s` to prevent prompt injection
/// via entity names or edge facts sourced from untrusted text.
fn scrub_content(s: &str) -> String {
    s.chars()
        .filter(|&c| c != '\n' && c != '\r' && c != '\x00' && !c.is_ascii_control())
        .collect()
}

/// Stats returned from graph eviction.
#[derive(Debug, Default)]
pub struct GraphEvictionStats {
    pub expired_edges_deleted: usize,
    pub orphan_entities_deleted: usize,
    pub capped_entities_deleted: usize,
}

/// Run label propagation on the full entity graph, generate community summaries via LLM,
/// and upsert results to `SQLite`.
///
/// Returns the number of communities detected (with `>= 2` entities).
///
/// # Errors
///
/// Returns an error if `SQLite` queries or LLM calls fail.
#[allow(clippy::too_many_lines)]
pub async fn detect_communities(
    store: &GraphStore,
    provider: &AnyProvider,
) -> Result<usize, MemoryError> {
    let entities = store.all_entities().await?;
    if entities.len() < 2 {
        return Ok(0);
    }

    // Build undirected graph: node weight = entity_id, no edge weight.
    // Tie-breaking in label propagation is deterministic for a given dataset
    // (labels are NodeIndex values assigned in ORDER BY id ASC order), but may
    // vary if entity IDs change after deletion/re-insertion.
    let mut graph = Graph::<i64, (), petgraph::Undirected>::new_undirected();
    let mut node_map: HashMap<i64, NodeIndex> = HashMap::new();

    for entity in &entities {
        let idx = graph.add_node(entity.id);
        node_map.insert(entity.id, idx);
    }

    let edges: Vec<_> = store.all_active_edges_stream().try_collect().await?;
    for edge in &edges {
        if let (Some(&src_idx), Some(&tgt_idx)) = (
            node_map.get(&edge.source_entity_id),
            node_map.get(&edge.target_entity_id),
        ) {
            graph.add_edge(src_idx, tgt_idx, ());
        }
    }

    // Label propagation: each node starts with its own NodeIndex as label.
    let mut labels: Vec<usize> = (0..graph.node_count()).collect();

    for _ in 0..MAX_LABEL_PROPAGATION_ITERATIONS {
        let mut changed = false;
        for node_idx in graph.node_indices() {
            let neighbors: Vec<NodeIndex> = graph.neighbors(node_idx).collect();
            if neighbors.is_empty() {
                continue;
            }

            let mut freq: HashMap<usize, usize> = HashMap::new();
            for &nbr in &neighbors {
                *freq.entry(labels[nbr.index()]).or_insert(0) += 1;
            }

            // neighbors is non-empty, so freq is non-empty — max and min are safe.
            let max_count = *freq.values().max().unwrap_or(&0);
            // Tie-break: smallest label value among tied candidates (deterministic).
            let best_label = freq
                .iter()
                .filter(|&(_, count)| *count == max_count)
                .map(|(&label, _)| label)
                .min()
                .unwrap_or(labels[node_idx.index()]);

            if labels[node_idx.index()] != best_label {
                labels[node_idx.index()] = best_label;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Group entities by final label.
    let mut communities: HashMap<usize, Vec<i64>> = HashMap::new();
    for node_idx in graph.node_indices() {
        let entity_id = graph[node_idx];
        communities
            .entry(labels[node_idx.index()])
            .or_default()
            .push(entity_id);
    }

    // Keep only communities with >= 2 entities.
    communities.retain(|_, members| members.len() >= 2);

    // Full rebuild: delete all existing communities before upserting new ones (M2 fix).
    store.delete_all_communities().await?;

    // Build entity name lookup for summary generation.
    let entity_name_map: HashMap<i64, &str> =
        entities.iter().map(|e| (e.id, e.name.as_str())).collect();

    // Build edge fact lookup indexed by entity pair.
    let mut edge_facts_map: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    for edge in &edges {
        let key = (edge.source_entity_id, edge.target_entity_id);
        edge_facts_map
            .entry(key)
            .or_default()
            .push(edge.fact.clone());
    }

    let mut count = 0usize;
    for (label_index, (_, entity_ids)) in communities.iter().enumerate() {
        let entity_names: Vec<String> = entity_ids
            .iter()
            .filter_map(|id| entity_name_map.get(id).map(|&s| scrub_content(s)))
            .collect();

        // Collect intra-community edge facts.
        let member_set: std::collections::HashSet<i64> = entity_ids.iter().copied().collect();
        let mut intra_facts: Vec<String> = Vec::new();
        for (&(src, tgt), facts) in &edge_facts_map {
            if member_set.contains(&src) && member_set.contains(&tgt) {
                intra_facts.extend(facts.iter().map(|f| scrub_content(f)));
            }
        }

        // Append label_index to prevent ON CONFLICT(name) collisions when two communities
        // share the same top-3 entity names across detect_communities runs (IC-SIG-02).
        let base_name = entity_names
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let name = format!("{base_name} [{label_index}]");

        // Generate LLM summary sequentially to avoid rate-limit issues.
        // TODO: consider FuturesUnordered with concurrency=3 if latency becomes a concern.
        let summary = match generate_community_summary(provider, &entity_names, &intra_facts).await
        {
            Ok(text) => text,
            Err(e) => {
                tracing::warn!("community summary generation failed: {e:#}");
                String::new()
            }
        };

        store.upsert_community(&name, &summary, entity_ids).await?;
        count += 1;
    }

    Ok(count)
}

/// Assign a single entity to an existing community via neighbor majority vote.
///
/// Returns `Some(community_id)` if assigned, `None` if no neighbors have communities.
///
/// # Errors
///
/// Returns an error if `SQLite` queries fail.
pub async fn assign_to_community(
    store: &GraphStore,
    entity_id: i64,
) -> Result<Option<i64>, MemoryError> {
    let edges = store.edges_for_entity(entity_id).await?;
    if edges.is_empty() {
        return Ok(None);
    }

    let neighbor_ids: Vec<i64> = edges
        .iter()
        .map(|e| {
            if e.source_entity_id == entity_id {
                e.target_entity_id
            } else {
                e.source_entity_id
            }
        })
        .collect();

    let mut community_votes: HashMap<i64, usize> = HashMap::new();
    for &nbr_id in &neighbor_ids {
        if let Some(community) = store.community_for_entity(nbr_id).await? {
            *community_votes.entry(community.id).or_insert(0) += 1;
        }
    }

    if community_votes.is_empty() {
        return Ok(None);
    }

    // Majority vote — tie-break by smallest community_id.
    // community_votes is non-empty (checked above), so max_by always returns Some.
    let Some((&best_community_id, _)) =
        community_votes
            .iter()
            .max_by(|&(&id_a, &count_a), &(&id_b, &count_b)| {
                count_a.cmp(&count_b).then(id_b.cmp(&id_a))
            })
    else {
        return Ok(None);
    };

    if let Some(mut target) = store.find_community_by_id(best_community_id).await? {
        if !target.entity_ids.contains(&entity_id) {
            target.entity_ids.push(entity_id);
            store
                .upsert_community(&target.name, &target.summary, &target.entity_ids)
                .await?;
        }
        return Ok(Some(best_community_id));
    }

    Ok(None)
}

/// Remove `Qdrant` points for entities that no longer exist in `SQLite`.
///
/// Returns the number of stale points deleted.
///
/// # Errors
///
/// Returns an error if `Qdrant` operations fail.
pub async fn cleanup_stale_entity_embeddings(
    _store: &GraphStore,
    _embeddings: &crate::embedding_store::EmbeddingStore,
) -> Result<usize, MemoryError> {
    // TODO: implement when EmbeddingStore exposes a scroll_all API
    // (follow-up: add pub async fn scroll_all(&self, collection, key_field) delegating to
    // self.ops.scroll_all). Then enumerate Qdrant points, collect IDs where entity_id is
    // not in SQLite, and delete stale points.
    Ok(0)
}

/// Run graph eviction: clean expired edges, orphan entities, and cap entity count.
///
/// # Errors
///
/// Returns an error if `SQLite` queries fail.
pub async fn run_graph_eviction(
    store: &GraphStore,
    expired_edge_retention_days: u32,
    max_entities: usize,
) -> Result<GraphEvictionStats, MemoryError> {
    let expired_edges_deleted = store
        .delete_expired_edges(expired_edge_retention_days)
        .await?;
    let orphan_entities_deleted = store
        .delete_orphan_entities(expired_edge_retention_days)
        .await?;
    let capped_entities_deleted = if max_entities > 0 {
        store.cap_entities(max_entities).await?
    } else {
        0
    };

    Ok(GraphEvictionStats {
        expired_edges_deleted,
        orphan_entities_deleted,
        capped_entities_deleted,
    })
}

async fn generate_community_summary(
    provider: &AnyProvider,
    entity_names: &[String],
    edge_facts: &[String],
) -> Result<String, MemoryError> {
    let entities_str = entity_names.join(", ");
    // Cap facts at 20 to bound prompt size; data is already scrubbed upstream.
    let facts_str = edge_facts
        .iter()
        .take(20)
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "Summarize the following group of related entities and their relationships \
         into a single paragraph (2-3 sentences). Focus on the theme that connects \
         them and the key relationships.\n\nEntities: {entities_str}\n\
         Relationships:\n{facts_str}\n\nSummary:"
    );

    let messages = [Message::from_legacy(Role::User, prompt)];
    let response: String = provider.chat(&messages).await.map_err(MemoryError::Llm)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::EntityType;
    use crate::sqlite::SqliteStore;

    async fn setup() -> GraphStore {
        let store = SqliteStore::new(":memory:").await.unwrap();
        GraphStore::new(store.pool().clone())
    }

    fn mock_provider() -> AnyProvider {
        AnyProvider::Mock(zeph_llm::mock::MockProvider::default())
    }

    #[tokio::test]
    async fn test_detect_communities_empty_graph() {
        let store = setup().await;
        let provider = mock_provider();
        let count = detect_communities(&store, &provider).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_detect_communities_single_entity() {
        let store = setup().await;
        let provider = mock_provider();
        store
            .upsert_entity("Solo", "Solo", EntityType::Concept, None)
            .await
            .unwrap();
        let count = detect_communities(&store, &provider).await.unwrap();
        assert_eq!(count, 0, "single isolated entity must not form a community");
    }

    #[tokio::test]
    async fn test_single_entity_community_filtered() {
        let store = setup().await;
        let provider = mock_provider();

        // Create 3 connected entities (cluster A) and 1 isolated entity.
        let a = store
            .upsert_entity("A", "A", EntityType::Concept, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("B", "B", EntityType::Concept, None)
            .await
            .unwrap();
        let c = store
            .upsert_entity("C", "C", EntityType::Concept, None)
            .await
            .unwrap();
        let _iso = store
            .upsert_entity("Isolated", "Isolated", EntityType::Concept, None)
            .await
            .unwrap();

        store
            .insert_edge(a, b, "r", "A relates B", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(b, c, "r", "B relates C", 1.0, None)
            .await
            .unwrap();

        let count = detect_communities(&store, &provider).await.unwrap();
        // Isolated entity has no edges — must NOT be persisted as a community.
        assert_eq!(count, 1, "only the 3-entity cluster should be detected");

        let communities = store.all_communities().await.unwrap();
        assert_eq!(communities.len(), 1);
        assert!(
            !communities[0].entity_ids.contains(&_iso),
            "isolated entity must not be in any community"
        );
    }

    #[tokio::test]
    async fn test_label_propagation_basic() {
        let store = setup().await;
        let provider = mock_provider();

        // Create 4 clusters of 3 entities each (12 entities total), fully isolated.
        let mut cluster_ids: Vec<Vec<i64>> = Vec::new();
        for cluster in 0..4_i64 {
            let mut ids = Vec::new();
            for node in 0..3_i64 {
                let name = format!("c{cluster}_n{node}");
                let id = store
                    .upsert_entity(&name, &name, EntityType::Concept, None)
                    .await
                    .unwrap();
                ids.push(id);
            }
            // Connect nodes within cluster (chain: 0-1-2).
            store
                .insert_edge(ids[0], ids[1], "r", "f", 1.0, None)
                .await
                .unwrap();
            store
                .insert_edge(ids[1], ids[2], "r", "f", 1.0, None)
                .await
                .unwrap();
            cluster_ids.push(ids);
        }

        let count = detect_communities(&store, &provider).await.unwrap();
        assert_eq!(count, 4, "expected 4 communities, one per cluster");

        let communities = store.all_communities().await.unwrap();
        assert_eq!(communities.len(), 4);

        // Each cluster's entity IDs must appear in exactly one community.
        for ids in &cluster_ids {
            let found = communities
                .iter()
                .filter(|c| ids.iter().any(|id| c.entity_ids.contains(id)))
                .count();
            assert_eq!(
                found, 1,
                "all nodes of a cluster must be in the same community"
            );
        }
    }

    #[tokio::test]
    async fn test_all_isolated_nodes() {
        let store = setup().await;
        let provider = mock_provider();

        // Insert 5 entities with no edges at all.
        for i in 0..5_i64 {
            store
                .upsert_entity(
                    &format!("iso_{i}"),
                    &format!("iso_{i}"),
                    EntityType::Concept,
                    None,
                )
                .await
                .unwrap();
        }

        let count = detect_communities(&store, &provider).await.unwrap();
        assert_eq!(count, 0, "zero-edge graph must produce no communities");
        assert_eq!(store.community_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_eviction_expired_edges() {
        let store = setup().await;

        let a = store
            .upsert_entity("EA", "EA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("EB", "EB", EntityType::Concept, None)
            .await
            .unwrap();
        let edge_id = store.insert_edge(a, b, "r", "f", 1.0, None).await.unwrap();
        store.invalidate_edge(edge_id).await.unwrap();

        // Manually set expired_at to a date far in the past to trigger deletion.
        sqlx::query(
            "UPDATE graph_edges SET expired_at = datetime('now', '-200 days') WHERE id = ?1",
        )
        .bind(edge_id)
        .execute(store.pool())
        .await
        .unwrap();

        let stats = run_graph_eviction(&store, 90, 0).await.unwrap();
        assert_eq!(stats.expired_edges_deleted, 1);
    }

    #[tokio::test]
    async fn test_eviction_orphan_entities() {
        let store = setup().await;

        let iso = store
            .upsert_entity("Orphan", "Orphan", EntityType::Concept, None)
            .await
            .unwrap();

        // Set last_seen_at to far in the past.
        sqlx::query(
            "UPDATE graph_entities SET last_seen_at = datetime('now', '-200 days') WHERE id = ?1",
        )
        .bind(iso)
        .execute(store.pool())
        .await
        .unwrap();

        let stats = run_graph_eviction(&store, 90, 0).await.unwrap();
        assert_eq!(stats.orphan_entities_deleted, 1);
    }

    #[tokio::test]
    async fn test_eviction_entity_cap() {
        let store = setup().await;

        // Insert 5 entities with no edges (so they can be capped).
        for i in 0..5_i64 {
            let name = format!("cap_entity_{i}");
            store
                .upsert_entity(&name, &name, EntityType::Concept, None)
                .await
                .unwrap();
        }

        let stats = run_graph_eviction(&store, 90, 3).await.unwrap();
        assert_eq!(
            stats.capped_entities_deleted, 2,
            "should delete 5-3=2 entities"
        );
        assert_eq!(store.entity_count().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn test_assign_to_community_no_neighbors() {
        let store = setup().await;
        let entity_id = store
            .upsert_entity("Loner", "Loner", EntityType::Concept, None)
            .await
            .unwrap();

        let result = assign_to_community(&store, entity_id).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_extraction_count_persistence() {
        use tempfile::NamedTempFile;
        // Create a real on-disk SQLite DB to verify persistence across store instances.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();

        let store1 = {
            let s = crate::sqlite::SqliteStore::new(&path).await.unwrap();
            GraphStore::new(s.pool().clone())
        };

        store1.set_metadata("extraction_count", "0").await.unwrap();
        for i in 1..=5_i64 {
            store1
                .set_metadata("extraction_count", &i.to_string())
                .await
                .unwrap();
        }

        // Open a second handle to the same file and verify the value persists.
        let store2 = {
            let s = crate::sqlite::SqliteStore::new(&path).await.unwrap();
            GraphStore::new(s.pool().clone())
        };
        assert_eq!(store2.extraction_count().await.unwrap(), 5);
    }

    #[tokio::test]
    async fn test_assign_to_community_majority_vote() {
        let store = setup().await;

        // Setup: community C1 with members [A, B], then add D with edges to both A and B.
        let a = store
            .upsert_entity("AA", "AA", EntityType::Concept, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("BB", "BB", EntityType::Concept, None)
            .await
            .unwrap();
        let d = store
            .upsert_entity("DD", "DD", EntityType::Concept, None)
            .await
            .unwrap();

        let community_id = store
            .upsert_community("test_community", "summary", &[a, b])
            .await
            .unwrap();

        store.insert_edge(d, a, "r", "f", 1.0, None).await.unwrap();
        store.insert_edge(d, b, "r", "f", 1.0, None).await.unwrap();

        let result = assign_to_community(&store, d).await.unwrap();
        assert_eq!(result, Some(community_id));

        let community = store
            .find_community_by_id(community_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            community.entity_ids.contains(&d),
            "D should be added to the community"
        );
    }
}
