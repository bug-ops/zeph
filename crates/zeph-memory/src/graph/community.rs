// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;
#[allow(unused_imports)]
use zeph_db::sql;

use futures::TryStreamExt as _;
use petgraph::Graph;
use petgraph::graph::NodeIndex;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use zeph_llm::LlmProvider as _;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{Message, Role};

use crate::error::MemoryError;

use super::store::GraphStore;
use super::types::Entity;

const MAX_LABEL_PROPAGATION_ITERATIONS: usize = 50;

/// Strip control characters, Unicode bidi overrides, and zero-width characters from `s`
/// to prevent prompt injection via entity names or edge facts sourced from untrusted text.
///
/// Filtered categories:
/// - All Unicode control characters (`Cc` category, covers ASCII controls and more)
/// - Bidi control characters: U+202A–U+202E, U+2066–U+2069
/// - Zero-width and invisible characters: U+200B–U+200F (includes U+200C, U+200D)
/// - Byte-order mark: U+FEFF
fn scrub_content(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c as u32,
                    0x200B..=0x200F | 0x202A..=0x202E | 0x2066..=0x2069 | 0xFEFF
                )
        })
        .collect()
}

/// Stats returned from graph eviction.
#[derive(Debug, Default)]
pub struct GraphEvictionStats {
    pub expired_edges_deleted: usize,
    pub orphan_entities_deleted: usize,
    pub capped_entities_deleted: usize,
}

/// Truncate `prompt` to at most `max_bytes` at a UTF-8 boundary, appending `"..."`
/// if truncation occurred.
///
/// If `max_bytes` is 0, returns an empty string immediately (disables community summaries).
/// Otherwise clamps the boundary to the nearest valid UTF-8 char boundary and appends `"..."`.
fn truncate_prompt(prompt: String, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    if prompt.len() <= max_bytes {
        return prompt;
    }
    let boundary = prompt.floor_char_boundary(max_bytes);
    format!("{}...", &prompt[..boundary])
}

/// Compute a BLAKE3 fingerprint for a community partition.
///
/// The fingerprint is derived from sorted entity IDs and sorted intra-community edge IDs,
/// ensuring both membership and edge mutations trigger re-summarization.
/// BLAKE3 is used (not `DefaultHasher`) to guarantee determinism across process restarts.
fn compute_partition_fingerprint(entity_ids: &[i64], intra_edge_ids: &[i64]) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut sorted_entities = entity_ids.to_vec();
    sorted_entities.sort_unstable();
    hasher.update(b"entities");
    for id in &sorted_entities {
        hasher.update(&id.to_le_bytes());
    }
    let mut sorted_edges = intra_edge_ids.to_vec();
    sorted_edges.sort_unstable();
    hasher.update(b"edges");
    for id in &sorted_edges {
        hasher.update(&id.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Per-community data collected before spawning LLM summarization tasks.
struct CommunityData {
    entity_ids: Vec<i64>,
    entity_names: Vec<String>,
    intra_facts: Vec<String>,
    fingerprint: String,
    name: String,
}

type UndirectedGraph = Graph<i64, (), petgraph::Undirected>;

async fn build_entity_graph_and_maps(
    store: &GraphStore,
    entities: &[Entity],
    edge_chunk_size: usize,
) -> Result<
    (
        UndirectedGraph,
        HashMap<(i64, i64), Vec<String>>,
        HashMap<(i64, i64), Vec<i64>>,
    ),
    MemoryError,
> {
    let mut graph = UndirectedGraph::new_undirected();
    let mut node_map: HashMap<i64, NodeIndex> = HashMap::new();

    for entity in entities {
        let idx = graph.add_node(entity.id.0);
        node_map.insert(entity.id.0, idx);
    }

    let mut edge_facts_map: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    let mut edge_id_map: HashMap<(i64, i64), Vec<i64>> = HashMap::new();

    if edge_chunk_size == 0 {
        let edges: Vec<_> = store.all_active_edges_stream().try_collect().await?;
        for edge in &edges {
            if let (Some(&src_idx), Some(&tgt_idx)) = (
                node_map.get(&edge.source_entity_id),
                node_map.get(&edge.target_entity_id),
            ) {
                graph.add_edge(src_idx, tgt_idx, ());
            }
            let key = (edge.source_entity_id, edge.target_entity_id);
            edge_facts_map
                .entry(key)
                .or_default()
                .push(edge.fact.clone());
            edge_id_map.entry(key).or_default().push(edge.id);
        }
    } else {
        let limit = i64::try_from(edge_chunk_size).unwrap_or(i64::MAX);
        let mut last_id: i64 = 0;
        loop {
            let chunk = store.edges_after_id(last_id, limit).await?;
            if chunk.is_empty() {
                break;
            }
            last_id = chunk.last().expect("non-empty chunk has a last element").id;
            for edge in &chunk {
                if let (Some(&src_idx), Some(&tgt_idx)) = (
                    node_map.get(&edge.source_entity_id),
                    node_map.get(&edge.target_entity_id),
                ) {
                    graph.add_edge(src_idx, tgt_idx, ());
                }
                let key = (edge.source_entity_id, edge.target_entity_id);
                edge_facts_map
                    .entry(key)
                    .or_default()
                    .push(edge.fact.clone());
                edge_id_map.entry(key).or_default().push(edge.id);
            }
        }
    }

    Ok((graph, edge_facts_map, edge_id_map))
}

fn run_label_propagation(graph: &UndirectedGraph) -> HashMap<usize, Vec<i64>> {
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
            let max_count = *freq.values().max().unwrap_or(&0);
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

    let mut communities: HashMap<usize, Vec<i64>> = HashMap::new();
    for node_idx in graph.node_indices() {
        let entity_id = graph[node_idx];
        communities
            .entry(labels[node_idx.index()])
            .or_default()
            .push(entity_id);
    }
    communities.retain(|_, members| members.len() >= 2);
    communities
}

struct ClassifyResult {
    to_summarize: Vec<CommunityData>,
    unchanged_count: usize,
    new_fingerprints: std::collections::HashSet<String>,
}

fn classify_communities(
    communities: &HashMap<usize, Vec<i64>>,
    edge_facts_map: &HashMap<(i64, i64), Vec<String>>,
    edge_id_map: &HashMap<(i64, i64), Vec<i64>>,
    entity_name_map: &HashMap<i64, &str>,
    stored_fingerprints: &HashMap<String, i64>,
    sorted_labels: &[usize],
) -> ClassifyResult {
    let mut to_summarize: Vec<CommunityData> = Vec::new();
    let mut unchanged_count = 0usize;
    let mut new_fingerprints: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (label_index, &label) in sorted_labels.iter().enumerate() {
        let entity_ids = communities[&label].as_slice();
        let member_set: std::collections::HashSet<i64> = entity_ids.iter().copied().collect();

        let mut intra_facts: Vec<String> = Vec::new();
        let mut intra_edge_ids: Vec<i64> = Vec::new();
        for (&(src, tgt), facts) in edge_facts_map {
            if member_set.contains(&src) && member_set.contains(&tgt) {
                intra_facts.extend(facts.iter().map(|f| scrub_content(f)));
                if let Some(ids) = edge_id_map.get(&(src, tgt)) {
                    intra_edge_ids.extend_from_slice(ids);
                }
            }
        }

        let fingerprint = compute_partition_fingerprint(entity_ids, &intra_edge_ids);
        new_fingerprints.insert(fingerprint.clone());

        if stored_fingerprints.contains_key(&fingerprint) {
            unchanged_count += 1;
            continue;
        }

        let entity_names: Vec<String> = entity_ids
            .iter()
            .filter_map(|id| entity_name_map.get(id).map(|&s| scrub_content(s)))
            .collect();

        // Append label_index to prevent ON CONFLICT(name) collisions when two communities
        // share the same top-3 entity names across detect_communities runs (IC-SIG-02).
        let base_name = entity_names
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let name = format!("{base_name} [{label_index}]");

        to_summarize.push(CommunityData {
            entity_ids: entity_ids.to_vec(),
            entity_names,
            intra_facts,
            fingerprint,
            name,
        });
    }

    ClassifyResult {
        to_summarize,
        unchanged_count,
        new_fingerprints,
    }
}

async fn summarize_and_upsert_communities(
    store: &GraphStore,
    provider: &AnyProvider,
    to_summarize: Vec<CommunityData>,
    concurrency: usize,
    community_summary_max_prompt_bytes: usize,
) -> Result<usize, MemoryError> {
    let semaphore = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut join_set: JoinSet<(String, String, Vec<i64>, String)> = JoinSet::new();

    for data in to_summarize {
        let provider = provider.clone();
        let sem = Arc::clone(&semaphore);
        let max_bytes = community_summary_max_prompt_bytes;
        join_set.spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore is never closed");
            let summary = match generate_community_summary(
                &provider,
                &data.entity_names,
                &data.intra_facts,
                max_bytes,
            )
            .await
            {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!(community = %data.name, "community summary generation failed: {e:#}");
                    String::new()
                }
            };
            (data.name, summary, data.entity_ids, data.fingerprint)
        });
    }

    // Collect results — handle task panics explicitly (HIGH-01 fix).
    let mut results: Vec<(String, String, Vec<i64>, String)> = Vec::new();
    while let Some(outcome) = join_set.join_next().await {
        match outcome {
            Ok(tuple) => results.push(tuple),
            Err(e) => {
                tracing::error!(
                    panicked = e.is_panic(),
                    cancelled = e.is_cancelled(),
                    "community summary task failed"
                );
            }
        }
    }

    results.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut count = 0usize;
    for (name, summary, entity_ids, fingerprint) in results {
        store
            .upsert_community(&name, &summary, &entity_ids, Some(&fingerprint))
            .await?;
        count += 1;
    }

    Ok(count)
}

/// Run label propagation on the full entity graph, generate community summaries via LLM,
/// and upsert results to `SQLite`.
///
/// Returns the number of communities detected (with `>= 2` entities).
///
/// Unchanged communities (same entity membership and intra-community edges) are skipped —
/// their existing summaries are preserved without LLM calls (incremental detection, #1262).
/// LLM calls for changed communities are parallelized via a `JoinSet` bounded by a
/// semaphore with `concurrency` permits (#1260).
///
/// # Panics
///
/// Does not panic in normal operation. The `semaphore.acquire().await.expect(...)` call is
/// infallible because the semaphore is never closed during the lifetime of this function.
///
/// # Errors
///
/// Returns an error if `SQLite` queries or LLM calls fail.
pub async fn detect_communities(
    store: &GraphStore,
    provider: &AnyProvider,
    community_summary_max_prompt_bytes: usize,
    concurrency: usize,
    edge_chunk_size: usize,
) -> Result<usize, MemoryError> {
    let edge_chunk_size = if edge_chunk_size == 0 {
        tracing::warn!(
            "edge_chunk_size is 0, which would load all edges into memory; \
             using safe default of 10_000"
        );
        10_000_usize
    } else {
        edge_chunk_size
    };

    let entities = store.all_entities().await?;
    if entities.len() < 2 {
        return Ok(0);
    }

    let (graph, edge_facts_map, edge_id_map) =
        build_entity_graph_and_maps(store, &entities, edge_chunk_size).await?;

    let communities = run_label_propagation(&graph);

    let entity_name_map: HashMap<i64, &str> =
        entities.iter().map(|e| (e.id.0, e.name.as_str())).collect();
    let stored_fingerprints = store.community_fingerprints().await?;

    let mut sorted_labels: Vec<usize> = communities.keys().copied().collect();
    sorted_labels.sort_unstable();

    let ClassifyResult {
        to_summarize,
        unchanged_count,
        new_fingerprints,
    } = classify_communities(
        &communities,
        &edge_facts_map,
        &edge_id_map,
        &entity_name_map,
        &stored_fingerprints,
        &sorted_labels,
    );

    tracing::debug!(
        total = sorted_labels.len(),
        unchanged = unchanged_count,
        to_summarize = to_summarize.len(),
        "community detection: partition classification complete"
    );

    // Delete dissolved communities (fingerprints no longer in new partition set).
    for (stored_fp, community_id) in &stored_fingerprints {
        if !new_fingerprints.contains(stored_fp.as_str()) {
            store.delete_community_by_id(*community_id).await?;
        }
    }

    let new_count = summarize_and_upsert_communities(
        store,
        provider,
        to_summarize,
        concurrency,
        community_summary_max_prompt_bytes,
    )
    .await?;

    Ok(unchanged_count + new_count)
}

/// Assign a single entity to an existing community via neighbor majority vote.
///
/// Returns `Some(community_id)` if assigned, `None` if no neighbors have communities.
///
/// When an entity is added, the stored fingerprint is cleared (`NULL`) so the next
/// `detect_communities` run will re-summarize the affected community (CRIT-02 fix).
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
        if !target.entity_ids.iter().any(|eid| eid.0 == entity_id) {
            target.entity_ids.push(crate::types::EntityId(entity_id));
            let raw_ids: Vec<i64> = target.entity_ids.iter().map(|eid| eid.0).collect();
            store
                .upsert_community(&target.name, &target.summary, &raw_ids, None)
                .await?;
            // Clear fingerprint to invalidate cache — next detect_communities will re-summarize.
            store.clear_community_fingerprint(best_community_id).await?;
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
    store: &GraphStore,
    embeddings: &crate::embedding_store::EmbeddingStore,
) -> Result<usize, MemoryError> {
    const ENTITY_COLLECTION: &str = "zeph_graph_entities";

    // Enumerate all (point_id, entity_id) pairs in the Qdrant entity collection.
    // Points without `entity_id_str` (legacy writes) are silently skipped; they will
    // gain the field on the next merge_entity / store_entity_embedding call.
    let pairs = embeddings.scroll_all_entity_ids(ENTITY_COLLECTION).await?;
    if pairs.is_empty() {
        return Ok(0);
    }

    let qdrant_ids: Vec<i64> = pairs.iter().map(|(_, eid)| *eid).collect();
    let live: std::collections::HashSet<i64> = store
        .entity_ids_in(&qdrant_ids)
        .await?
        .into_iter()
        .collect();

    let stale_point_ids: Vec<String> = pairs
        .into_iter()
        .filter_map(|(pid, eid)| (!live.contains(&eid)).then_some(pid))
        .collect();

    if stale_point_ids.is_empty() {
        return Ok(0);
    }

    let count = stale_point_ids.len();
    embeddings
        .delete_from_collection(ENTITY_COLLECTION, stale_point_ids)
        .await?;
    Ok(count)
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
    max_prompt_bytes: usize,
) -> Result<String, MemoryError> {
    let entities_str = entity_names.join(", ");
    // Cap facts at 20 to bound prompt size; data is already scrubbed upstream.
    let facts_str = edge_facts
        .iter()
        .take(20)
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n");

    let raw_prompt = format!(
        "Summarize the following group of related entities and their relationships \
         into a single paragraph (2-3 sentences). Focus on the theme that connects \
         them and the key relationships.\n\nEntities: {entities_str}\n\
         Relationships:\n{facts_str}\n\nSummary:"
    );

    let original_bytes = raw_prompt.len();
    let truncated = raw_prompt.len() > max_prompt_bytes;
    let prompt = truncate_prompt(raw_prompt, max_prompt_bytes);
    if prompt.is_empty() {
        return Ok(String::new());
    }
    if truncated {
        tracing::warn!(
            entity_count = entity_names.len(),
            original_bytes,
            truncated_bytes = prompt.len(),
            "community summary prompt truncated"
        );
    }

    let messages = [Message::from_legacy(Role::User, prompt)];
    let response: String = provider.chat(&messages).await.map_err(MemoryError::Llm)?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::graph::types::EntityType;
    use crate::store::SqliteStore;

    async fn setup() -> GraphStore {
        let store = SqliteStore::new(":memory:").await.unwrap();
        GraphStore::new(store.pool().clone())
    }

    fn mock_provider() -> AnyProvider {
        AnyProvider::Mock(zeph_llm::mock::MockProvider::default())
    }

    fn recording_provider() -> (
        AnyProvider,
        Arc<Mutex<Vec<Vec<zeph_llm::provider::Message>>>>,
    ) {
        let (mock, buf) = zeph_llm::mock::MockProvider::default().with_recording();
        (AnyProvider::Mock(mock), buf)
    }

    #[tokio::test]
    async fn test_detect_communities_empty_graph() {
        let store = setup().await;
        let provider = mock_provider();
        let count = detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
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
        let count = detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
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
            .unwrap()
            .0;
        let b = store
            .upsert_entity("B", "B", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let c = store
            .upsert_entity("C", "C", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let iso = store
            .upsert_entity("Isolated", "Isolated", EntityType::Concept, None)
            .await
            .unwrap()
            .0;

        store
            .insert_edge(a, b, "r", "A relates B", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(b, c, "r", "B relates C", 1.0, None)
            .await
            .unwrap();

        let count = detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        // Isolated entity has no edges — must NOT be persisted as a community.
        assert_eq!(count, 1, "only the 3-entity cluster should be detected");

        let communities = store.all_communities().await.unwrap();
        assert_eq!(communities.len(), 1);
        assert!(
            !communities[0].entity_ids.iter().any(|eid| eid.0 == iso),
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
                    .unwrap()
                    .0;
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

        let count = detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        assert_eq!(count, 4, "expected 4 communities, one per cluster");

        let communities = store.all_communities().await.unwrap();
        assert_eq!(communities.len(), 4);

        // Each cluster's entity IDs must appear in exactly one community.
        for ids in &cluster_ids {
            let found = communities
                .iter()
                .filter(|c| {
                    ids.iter()
                        .any(|id| c.entity_ids.iter().any(|eid| eid.0 == *id))
                })
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

        let count = detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        assert_eq!(count, 0, "zero-edge graph must produce no communities");
        assert_eq!(store.community_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_eviction_expired_edges() {
        let store = setup().await;

        let a = store
            .upsert_entity("EA", "EA", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("EB", "EB", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let edge_id = store.insert_edge(a, b, "r", "f", 1.0, None).await.unwrap();
        store.invalidate_edge(edge_id).await.unwrap();

        // Manually set expired_at to a date far in the past to trigger deletion.
        zeph_db::query(sql!(
            "UPDATE graph_edges SET expired_at = datetime('now', '-200 days') WHERE id = ?1"
        ))
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
            .unwrap()
            .0;

        // Set last_seen_at to far in the past.
        zeph_db::query(sql!(
            "UPDATE graph_entities SET last_seen_at = datetime('now', '-200 days') WHERE id = ?1"
        ))
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
            .unwrap()
            .0;

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
            let s = crate::store::SqliteStore::new(&path).await.unwrap();
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
            let s = crate::store::SqliteStore::new(&path).await.unwrap();
            GraphStore::new(s.pool().clone())
        };
        assert_eq!(store2.extraction_count().await.unwrap(), 5);
    }

    #[test]
    fn test_scrub_content_ascii_control() {
        // Newline, carriage return, null byte, tab (all ASCII control chars) must be stripped.
        let input = "hello\nworld\r\x00\x01\x09end";
        assert_eq!(scrub_content(input), "helloworldend");
    }

    #[test]
    fn test_scrub_content_bidi_overrides() {
        // U+202A LEFT-TO-RIGHT EMBEDDING, U+202E RIGHT-TO-LEFT OVERRIDE,
        // U+2066 LEFT-TO-RIGHT ISOLATE, U+2069 POP DIRECTIONAL ISOLATE.
        let input = "safe\u{202A}inject\u{202E}end\u{2066}iso\u{2069}done".to_string();
        assert_eq!(scrub_content(&input), "safeinjectendisodone");
    }

    #[test]
    fn test_scrub_content_zero_width() {
        // U+200B ZERO WIDTH SPACE, U+200C ZERO WIDTH NON-JOINER, U+200D ZERO WIDTH JOINER,
        // U+200F RIGHT-TO-LEFT MARK.
        let input = "a\u{200B}b\u{200C}c\u{200D}d\u{200F}e".to_string();
        assert_eq!(scrub_content(&input), "abcde");
    }

    #[test]
    fn test_scrub_content_bom() {
        // U+FEFF BYTE ORDER MARK must be stripped.
        let input = "\u{FEFF}hello".to_string();
        assert_eq!(scrub_content(&input), "hello");
    }

    #[test]
    fn test_scrub_content_clean_string_unchanged() {
        let input = "Hello, World! 123 — normal text.";
        assert_eq!(scrub_content(input), input);
    }

    #[test]
    fn test_truncate_prompt_within_limit() {
        let result = truncate_prompt("short".into(), 100);
        assert_eq!(result, "short");
    }

    #[test]
    fn test_truncate_prompt_zero_max_bytes() {
        let result = truncate_prompt("hello".into(), 0);
        assert_eq!(result, "");
    }

    #[test]
    fn test_truncate_prompt_long_facts() {
        let facts: Vec<String> = (0..20)
            .map(|i| format!("fact_{i}_{}", "x".repeat(20)))
            .collect();
        let prompt = facts.join("\n");
        let result = truncate_prompt(prompt, 200);
        assert!(
            result.ends_with("..."),
            "truncated prompt must end with '...'"
        );
        // byte length must be at most max_bytes + 3 (the "..." suffix)
        assert!(result.len() <= 203);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn test_truncate_prompt_utf8_boundary() {
        // Each '🔥' is 4 bytes; 100 emojis = 400 bytes.
        let prompt = "🔥".repeat(100);
        let result = truncate_prompt(prompt, 10);
        assert!(
            result.ends_with("..."),
            "truncated prompt must end with '...'"
        );
        // floor_char_boundary(10) for 4-byte chars lands at 8 (2 full emojis = 8 bytes)
        assert_eq!(result.len(), 8 + 3, "2 emojis (8 bytes) + '...' (3 bytes)");
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn test_assign_to_community_majority_vote() {
        let store = setup().await;

        // Setup: community C1 with members [A, B], then add D with edges to both A and B.
        let a = store
            .upsert_entity("AA", "AA", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("BB", "BB", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let d = store
            .upsert_entity("DD", "DD", EntityType::Concept, None)
            .await
            .unwrap()
            .0;

        store
            .upsert_community("test_community", "summary", &[a, b], None)
            .await
            .unwrap();

        store.insert_edge(d, a, "r", "f", 1.0, None).await.unwrap();
        store.insert_edge(d, b, "r", "f", 1.0, None).await.unwrap();

        let result = assign_to_community(&store, d).await.unwrap();
        assert!(result.is_some());

        // The returned ID must be valid for subsequent lookups (HIGH-IC-01 regression test).
        let returned_id = result.unwrap();
        let community = store
            .find_community_by_id(returned_id)
            .await
            .unwrap()
            .expect("returned community_id must reference an existing row");
        assert!(
            community.entity_ids.iter().any(|eid| eid.0 == d),
            "D should be added to the community"
        );
        // Fingerprint must be NULL after assign (cache invalidated for next detect run).
        assert!(
            community.fingerprint.is_none(),
            "fingerprint must be cleared after assign_to_community"
        );
    }

    /// #1262: Second `detect_communities` call with no graph changes must produce 0 LLM calls.
    #[tokio::test]
    async fn test_incremental_detection_no_changes_skips_llm() {
        let store = setup().await;
        let (provider, call_buf) = recording_provider();

        let a = store
            .upsert_entity("X", "X", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("Y", "Y", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(a, b, "r", "X relates Y", 1.0, None)
            .await
            .unwrap();

        // First run: LLM called once to summarize the community.
        detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        let first_calls = call_buf.lock().unwrap().len();
        assert_eq!(first_calls, 1, "first run must produce exactly 1 LLM call");

        // Second run: graph unchanged — 0 LLM calls.
        detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        let second_calls = call_buf.lock().unwrap().len();
        assert_eq!(
            second_calls, first_calls,
            "second run with no graph changes must produce 0 additional LLM calls"
        );
    }

    /// #1262: Adding an edge changes the fingerprint — LLM must be called again.
    #[tokio::test]
    async fn test_incremental_detection_edge_change_triggers_resummary() {
        let store = setup().await;
        let (provider, call_buf) = recording_provider();

        let a = store
            .upsert_entity("P", "P", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("Q", "Q", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(a, b, "r", "P relates Q", 1.0, None)
            .await
            .unwrap();

        detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        let after_first = call_buf.lock().unwrap().len();
        assert_eq!(after_first, 1);

        // Add a new edge within the community to change its fingerprint.
        store
            .insert_edge(b, a, "r2", "Q also relates P", 1.0, None)
            .await
            .unwrap();

        detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        let after_second = call_buf.lock().unwrap().len();
        assert_eq!(
            after_second, 2,
            "edge change must trigger one additional LLM call"
        );
    }

    /// #1262: Communities whose fingerprints vanish are deleted on refresh.
    #[tokio::test]
    async fn test_incremental_detection_dissolved_community_deleted() {
        let store = setup().await;
        let provider = mock_provider();

        let a = store
            .upsert_entity("M1", "M1", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("M2", "M2", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let edge_id = store
            .insert_edge(a, b, "r", "M1 relates M2", 1.0, None)
            .await
            .unwrap();

        detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        assert_eq!(store.community_count().await.unwrap(), 1);

        // Invalidate the edge — community dissolves.
        store.invalidate_edge(edge_id).await.unwrap();

        detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        assert_eq!(
            store.community_count().await.unwrap(),
            0,
            "dissolved community must be deleted on next refresh"
        );
    }

    /// #1260: Sequential fallback (concurrency=1) produces correct results.
    #[tokio::test]
    async fn test_detect_communities_concurrency_one() {
        let store = setup().await;
        let provider = mock_provider();

        let a = store
            .upsert_entity("C1A", "C1A", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("C1B", "C1B", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        store.insert_edge(a, b, "r", "f", 1.0, None).await.unwrap();

        let count = detect_communities(&store, &provider, usize::MAX, 1, 0)
            .await
            .unwrap();
        assert_eq!(count, 1, "concurrency=1 must still detect the community");
        assert_eq!(store.community_count().await.unwrap(), 1);
    }

    #[test]
    fn test_compute_fingerprint_deterministic() {
        let fp1 = compute_partition_fingerprint(&[1, 2, 3], &[10, 20]);
        let fp2 = compute_partition_fingerprint(&[3, 1, 2], &[20, 10]);
        assert_eq!(fp1, fp2, "fingerprint must be order-independent");

        let fp3 = compute_partition_fingerprint(&[1, 2, 3], &[10, 30]);
        assert_ne!(
            fp1, fp3,
            "different edge IDs must produce different fingerprint"
        );

        let fp4 = compute_partition_fingerprint(&[1, 2, 4], &[10, 20]);
        assert_ne!(
            fp1, fp4,
            "different entity IDs must produce different fingerprint"
        );
    }

    /// Domain separator test: entity/edge sequences with same raw bytes must not collide.
    ///
    /// Without domain separators, entities=[1,2] edges=[3] would hash identically to
    /// entities=[1] edges=[2,3] (same concatenated `le_bytes`). With separators they differ.
    #[test]
    fn test_compute_fingerprint_domain_separation() {
        let fp_a = compute_partition_fingerprint(&[1, 2], &[3]);
        let fp_b = compute_partition_fingerprint(&[1], &[2, 3]);
        assert_ne!(
            fp_a, fp_b,
            "entity/edge sequences with same raw bytes must produce different fingerprints"
        );
    }

    /// Chunked loading with `chunk_size=1` must produce correct community assignments.
    ///
    /// Verifies: (a) community count is correct, (b) `edge_facts_map` and `edge_id_map` are fully
    /// populated (checked via community membership — all edges contribute to fingerprints),
    /// (c) the loop executes multiple iterations by using a tiny chunk size on a 3-edge graph.
    #[tokio::test]
    async fn test_detect_communities_chunked_correct_membership() {
        let store = setup().await;
        let provider = mock_provider();

        // Build two isolated clusters: A-B-C and D-E.
        let node_alpha = store
            .upsert_entity("CA", "CA", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let node_beta = store
            .upsert_entity("CB", "CB", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let node_gamma = store
            .upsert_entity("CC", "CC", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let node_delta = store
            .upsert_entity("CD", "CD", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let node_epsilon = store
            .upsert_entity("CE", "CE", EntityType::Concept, None)
            .await
            .unwrap()
            .0;

        store
            .insert_edge(node_alpha, node_beta, "r", "A-B fact", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(node_beta, node_gamma, "r", "B-C fact", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(node_delta, node_epsilon, "r", "D-E fact", 1.0, None)
            .await
            .unwrap();

        // chunk_size=1: each edge is fetched individually — loop must execute 3 times.
        let count_chunked = detect_communities(&store, &provider, usize::MAX, 4, 1)
            .await
            .unwrap();
        assert_eq!(
            count_chunked, 2,
            "chunked loading must detect both communities"
        );

        // Verify communities contain the correct members.
        let communities = store.all_communities().await.unwrap();
        assert_eq!(communities.len(), 2);

        let abc_ids = [node_alpha, node_beta, node_gamma];
        let de_ids = [node_delta, node_epsilon];
        let has_abc = communities.iter().any(|comm| {
            abc_ids
                .iter()
                .all(|id| comm.entity_ids.iter().any(|eid| eid.0 == *id))
        });
        let has_de = communities.iter().any(|comm| {
            de_ids
                .iter()
                .all(|id| comm.entity_ids.iter().any(|eid| eid.0 == *id))
        });
        assert!(has_abc, "cluster A-B-C must form a community");
        assert!(has_de, "cluster D-E must form a community");
    }

    /// `chunk_size=usize::MAX` must load all edges in a single query and produce correct results.
    #[tokio::test]
    async fn test_detect_communities_chunk_size_max() {
        let store = setup().await;
        let provider = mock_provider();

        let x = store
            .upsert_entity("MX", "MX", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let y = store
            .upsert_entity("MY", "MY", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(x, y, "r", "X-Y fact", 1.0, None)
            .await
            .unwrap();

        let count = detect_communities(&store, &provider, usize::MAX, 4, usize::MAX)
            .await
            .unwrap();
        assert_eq!(count, 1, "chunk_size=usize::MAX must detect the community");
    }

    /// `chunk_size=0` falls back to the stream path without panicking.
    #[tokio::test]
    async fn test_detect_communities_chunk_size_zero_fallback() {
        let store = setup().await;
        let provider = mock_provider();

        let p = store
            .upsert_entity("ZP", "ZP", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let q = store
            .upsert_entity("ZQ", "ZQ", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(p, q, "r", "P-Q fact", 1.0, None)
            .await
            .unwrap();

        let count = detect_communities(&store, &provider, usize::MAX, 4, 0)
            .await
            .unwrap();
        assert_eq!(
            count, 1,
            "chunk_size=0 must detect the community via stream fallback"
        );
    }

    /// Verifies that `edge_facts_map` is fully populated during chunked loading by checking
    /// that the community fingerprint changes when a new edge is added (fingerprint includes
    /// edge IDs, so any missed edges would produce a different or stale fingerprint).
    #[tokio::test]
    async fn test_detect_communities_chunked_edge_map_complete() {
        let store = setup().await;
        let (provider, call_buf) = recording_provider();

        let a = store
            .upsert_entity("FA", "FA", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("FB", "FB", EntityType::Concept, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(a, b, "r", "edge1 fact", 1.0, None)
            .await
            .unwrap();

        // First detection with chunk_size=1.
        detect_communities(&store, &provider, usize::MAX, 4, 1)
            .await
            .unwrap();
        let calls_after_first = call_buf.lock().unwrap().len();
        assert_eq!(calls_after_first, 1, "first run must trigger 1 LLM call");

        // Add another edge — fingerprint must change, triggering a second LLM call.
        store
            .insert_edge(b, a, "r2", "edge2 fact", 1.0, None)
            .await
            .unwrap();

        detect_communities(&store, &provider, usize::MAX, 4, 1)
            .await
            .unwrap();
        let calls_after_second = call_buf.lock().unwrap().len();
        assert_eq!(
            calls_after_second, 2,
            "adding an edge must change fingerprint and trigger re-summarization"
        );
    }

    /// `cleanup_stale_entity_embeddings` returns `Ok(0)` when the collection is empty.
    #[tokio::test]
    async fn cleanup_stale_empty_collection() {
        let store = setup().await;
        let sqlite_store = crate::store::SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite_store.pool().clone();
        let mem_store = Box::new(crate::in_memory_store::InMemoryVectorStore::new());
        let emb_store = crate::embedding_store::EmbeddingStore::with_store(mem_store, pool);
        emb_store
            .ensure_named_collection("zeph_graph_entities", 4)
            .await
            .unwrap();

        let deleted = cleanup_stale_entity_embeddings(&store, &emb_store)
            .await
            .unwrap();
        assert_eq!(deleted, 0, "nothing to delete from empty collection");
    }

    /// `cleanup_stale_entity_embeddings` deletes the Qdrant point when the `SQLite` entity row
    /// has been removed, and leaves live entities untouched.
    #[tokio::test]
    async fn cleanup_stale_deletes_orphaned_points() {
        use crate::graph::types::EntityType;

        let sqlite_store = crate::store::SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite_store.pool().clone();
        let graph_store = GraphStore::new(pool.clone());

        let mem_store = Box::new(crate::in_memory_store::InMemoryVectorStore::new());
        let emb_store = crate::embedding_store::EmbeddingStore::with_store(mem_store, pool.clone());
        emb_store
            .ensure_named_collection("zeph_graph_entities", 4)
            .await
            .unwrap();

        // Insert two entities in SQLite.
        let live_id = graph_store
            .upsert_entity("Live", "live", EntityType::Person, None)
            .await
            .unwrap()
            .0;
        let stale_id = graph_store
            .upsert_entity("Stale", "stale", EntityType::Person, None)
            .await
            .unwrap()
            .0;

        // Store embeddings with `entity_id_str` for both.
        let live_payload = serde_json::json!({
            "entity_id": live_id,
            "entity_id_str": live_id.to_string(),
            "name": "Live",
        });
        let stale_payload = serde_json::json!({
            "entity_id": stale_id,
            "entity_id_str": stale_id.to_string(),
            "name": "Stale",
        });
        emb_store
            .store_to_collection(
                "zeph_graph_entities",
                live_payload,
                vec![1.0, 0.0, 0.0, 0.0],
            )
            .await
            .unwrap();
        emb_store
            .store_to_collection(
                "zeph_graph_entities",
                stale_payload,
                vec![0.0, 1.0, 0.0, 0.0],
            )
            .await
            .unwrap();

        // Delete the stale entity from SQLite (simulating eviction).
        zeph_db::query(zeph_db::sql!("DELETE FROM graph_entities WHERE id = ?"))
            .bind(stale_id)
            .execute(&pool)
            .await
            .unwrap();

        let deleted = cleanup_stale_entity_embeddings(&graph_store, &emb_store)
            .await
            .unwrap();
        assert_eq!(deleted, 1, "exactly one stale point should be removed");

        // The live entity's embedding must remain.
        let remaining = emb_store
            .scroll_all_entity_ids("zeph_graph_entities")
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1, live_id);
    }
}
