// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};
#[allow(unused_imports)]
use zeph_db::sql;

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;

use super::activation::{ActivatedFact, SpreadingActivation, SpreadingActivationParams};
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
    embeddings: Option<&crate::embedding_store::EmbeddingStore>,
    provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    max_hops: u32,
    at_timestamp: Option<&str>,
    temporal_decay_rate: f64,
    edge_types: &[EdgeType],
) -> Result<Vec<GraphFact>, MemoryError> {
    // graph_recall has no SpreadingActivationParams — use spec defaults.
    const DEFAULT_STRUCTURAL_WEIGHT: f32 = 0.4;
    const DEFAULT_COMMUNITY_CAP: usize = 3;

    if limit == 0 {
        return Ok(Vec::new());
    }

    // Step 1: hybrid seed selection (FTS5 score + structural score + community cap).
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

    // Capture current time once for consistent decay scoring across all facts.
    let now_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed());

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

        // Collect edge IDs before conversion to GraphFact (critic: issue 7 fix).
        let traversed_edge_ids: Vec<i64> = edges.iter().map(|e| e.id).collect();

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
                retrieval_count: edge.retrieval_count,
            });
        }

        // Record edge retrievals (fire-and-forget).
        if !traversed_edge_ids.is_empty()
            && let Err(e) = store.record_edge_retrieval(&traversed_edge_ids).await
        {
            tracing::warn!(error = %e, "graph_recall: failed to record edge retrieval");
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

/// Find seed entities using hybrid ranking: FTS5 score + structural score + community cap.
///
/// Algorithm:
/// 1. Run `find_entities_ranked()` per query word (up to 5 words).
/// 2. If empty and `embeddings` is available, fall back to embedding similarity search.
/// 3. Compute structural scores (degree + edge type diversity).
/// 4. Look up community IDs.
/// 5. Combine: `hybrid_score = fts_score * (1 - structural_weight) + structural_score * structural_weight`.
/// 6. Apply community cap: keep top `seed_community_cap` per community (0 = unlimited).
/// 7. Guard: if cap empties the result, return top-N ignoring cap (SA-INV-10).
///
/// # Errors
///
/// Returns an error if any database query fails.
/// Fill `fts_map` via embedding similarity when FTS5 returned zero results.
///
/// Returns `false` when `embed()` fails (caller should return empty seeds).
/// On search failure: logs warning and leaves map empty (caller continues normally).
async fn seed_embedding_fallback(
    store: &GraphStore,
    emb_store: &EmbeddingStore,
    provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    fts_map: &mut HashMap<i64, (super::types::Entity, f32)>,
) -> bool {
    use zeph_llm::LlmProvider as _;
    const ENTITY_COLLECTION: &str = "zeph_graph_entities";
    let embedding = match provider.embed(query).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "seed fallback: embed() failed, returning empty seeds");
            return false;
        }
    };
    match emb_store
        .search_collection(ENTITY_COLLECTION, &embedding, limit, None)
        .await
    {
        Ok(results) => {
            for result in results {
                if let Some(entity_id) = result
                    .payload
                    .get("entity_id")
                    .and_then(serde_json::Value::as_i64)
                    && let Ok(Some(entity)) = store.find_entity_by_id(entity_id).await
                {
                    fts_map.insert(entity_id, (entity, result.score));
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "seed fallback: embedding search failed");
        }
    }
    true
}

pub(crate) async fn find_seed_entities(
    store: &GraphStore,
    embeddings: Option<&EmbeddingStore>,
    provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    structural_weight: f32,
    community_cap: usize,
) -> Result<HashMap<i64, f32>, MemoryError> {
    use crate::graph::types::ScoredEntity;

    const MAX_WORDS: usize = 5;

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

    // Step 1: gather ranked FTS5 matches per word, merge by max fts_score.
    let mut fts_map: HashMap<i64, (super::types::Entity, f32)> = HashMap::new();
    for word in &words {
        let ranked = store.find_entities_ranked(word, limit * 2).await?;
        for (entity, fts_score) in ranked {
            fts_map
                .entry(entity.id)
                .and_modify(|(_, s)| *s = s.max(fts_score))
                .or_insert((entity, fts_score));
        }
    }

    // Step 2: embedding fallback when FTS5 returns nothing.
    if fts_map.is_empty()
        && let Some(emb_store) = embeddings
        && !seed_embedding_fallback(store, emb_store, provider, query, limit, &mut fts_map).await
    {
        return Ok(HashMap::new());
    }

    if fts_map.is_empty() {
        return Ok(HashMap::new());
    }

    let entity_ids: Vec<i64> = fts_map.keys().copied().collect();

    // Step 3: structural scores.
    let structural_scores = store.entity_structural_scores(&entity_ids).await?;

    // Step 4: community IDs.
    let community_ids = store.entity_community_ids(&entity_ids).await?;

    // Step 5: compute hybrid scores.
    let fts_weight = 1.0 - structural_weight;
    let mut scored: Vec<ScoredEntity> = fts_map
        .into_values()
        .map(|(entity, fts_score)| {
            let struct_score = structural_scores.get(&entity.id).copied().unwrap_or(0.0);
            let community_id = community_ids.get(&entity.id).copied();
            ScoredEntity {
                entity,
                fts_score,
                structural_score: struct_score,
                community_id,
            }
        })
        .collect();

    // Sort by hybrid score descending.
    scored.sort_by(|a, b| {
        let score_a = a.fts_score * fts_weight + a.structural_score * structural_weight;
        let score_b = b.fts_score * fts_weight + b.structural_score * structural_weight;
        score_b.total_cmp(&score_a)
    });

    // Step 6: apply community cap.
    let capped: Vec<&ScoredEntity> = if community_cap == 0 {
        scored.iter().collect()
    } else {
        let mut community_counts: HashMap<i64, usize> = HashMap::new();
        let mut result: Vec<&ScoredEntity> = Vec::new();
        for se in &scored {
            match se.community_id {
                Some(cid) => {
                    let count = community_counts.entry(cid).or_insert(0);
                    if *count < community_cap {
                        *count += 1;
                        result.push(se);
                    }
                }
                None => {
                    // No community — unlimited.
                    result.push(se);
                }
            }
        }
        result
    };

    // Step 7: SA-INV-10 guard — if cap zeroed out non-None-community seeds, fall back to top-N.
    let selected: Vec<&ScoredEntity> = if capped.is_empty() && !scored.is_empty() {
        scored.iter().take(limit).collect()
    } else {
        capped.into_iter().take(limit).collect()
    };

    let entity_scores: HashMap<i64, f32> = selected
        .into_iter()
        .map(|se| {
            let hybrid = se.fts_score * fts_weight + se.structural_score * structural_weight;
            // Clamp to [0.1, 1.0] to keep hybrid seeds above activation_threshold.
            (se.entity.id, hybrid.clamp(0.1, 1.0))
        })
        .collect();

    Ok(entity_scores)
}

/// Retrieve graph facts via SYNAPSE spreading activation from seed entities.
///
/// Algorithm:
/// 1. Find seed entities via fuzzy word search (same as [`graph_recall`]).
/// 2. Run spreading activation from seeds using `config`.
/// 3. Return `ActivatedFact` records (edges collected during propagation) sorted by
///    activation score descending, truncated to `limit`.
///
/// Edge type filtering via `edge_types` ensures MAGMA subgraph scoping is preserved
/// (mirrors [`graph_recall`]'s `bfs_typed` path, MAJOR-05 fix).
///
/// # Errors
///
/// Returns an error if any database query fails.
pub async fn graph_recall_activated(
    store: &GraphStore,
    embeddings: Option<&EmbeddingStore>,
    provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    params: SpreadingActivationParams,
    edge_types: &[EdgeType],
) -> Result<Vec<ActivatedFact>, MemoryError> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let entity_scores = find_seed_entities(
        store,
        embeddings,
        provider,
        query,
        limit,
        params.seed_structural_weight,
        params.seed_community_cap,
    )
    .await?;

    if entity_scores.is_empty() {
        return Ok(Vec::new());
    }

    tracing::debug!(
        seeds = entity_scores.len(),
        "spreading activation: starting recall"
    );

    let sa = SpreadingActivation::new(params);
    let (_, mut facts) = sa.spread(store, entity_scores, edge_types).await?;

    // Record edge retrievals from activated facts (fire-and-forget).
    let edge_ids: Vec<i64> = facts.iter().map(|f| f.edge.id).collect();
    if !edge_ids.is_empty()
        && let Err(e) = store.record_edge_retrieval(&edge_ids).await
    {
        tracing::warn!(error = %e, "graph_recall_activated: failed to record edge retrieval");
    }

    // Sort by activation score descending and truncate to limit.
    facts.sort_by(|a, b| b.activation_score.total_cmp(&a.activation_score));

    // Deduplicate by (source, relation, target, edge_type) keeping highest activation.
    let mut seen: HashSet<(i64, String, i64, EdgeType)> = HashSet::new();
    facts.retain(|f| {
        seen.insert((
            f.edge.source_entity_id,
            f.edge.relation.clone(),
            f.edge.target_entity_id,
            f.edge.edge_type,
        ))
    });

    facts.truncate(limit);

    tracing::debug!(
        result_count = facts.len(),
        "spreading activation: recall complete"
    );

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
        zeph_db::query(
            sql!("INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
             VALUES (?1, ?2, 'knows', 'Alice knows Bob', 0.9, '2100-01-01 00:00:00')"),
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
        zeph_db::query(
            sql!("INSERT INTO graph_edges
             (source_entity_id, target_entity_id, relation, fact, confidence, valid_from, valid_to, expired_at)
             VALUES (?1, ?2, 'manages', 'Alice manages Carol', 0.8,
                     '2020-01-01 00:00:00', '2021-01-01 00:00:00', '2021-01-01 00:00:00')"),
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

    // Community cap guard (SA-INV-10): when all FTS5 seeds are in a single community and
    // community_cap = 3 < total seeds, the result must still be non-empty.
    //
    // This tests the guard path in find_seed_entities: if after applying the community cap
    // the result set is empty, the function falls back to top-N uncapped.
    #[tokio::test]
    async fn graph_recall_community_cap_guard_non_empty() {
        let store = setup_store().await;
        // Create 5 entities all in the same community
        let mut entity_ids = Vec::new();
        for i in 0..5usize {
            let id = store
                .upsert_entity(
                    &format!("Entity{i}"),
                    &format!("entity{i}"),
                    crate::graph::types::EntityType::Concept,
                    None,
                )
                .await
                .unwrap();
            entity_ids.push(id);
        }

        // Put all 5 in the same community
        let community_id = store
            .upsert_community("TestComm", "test", &entity_ids, Some("fp"))
            .await
            .unwrap();
        let _ = community_id;

        // Create a hub entity with edges to all 5 — so BFS from the hub yields facts
        let hub = store
            .upsert_entity("Hub", "hub", crate::graph::types::EntityType::Concept, None)
            .await
            .unwrap();
        for &target in &entity_ids {
            store
                .insert_edge(hub, target, "has", "Hub has entity", 0.9, None)
                .await
                .unwrap();
        }

        let provider = mock_provider();
        // "hub" query matches the Hub entity via FTS5; it has no community so cap doesn't apply.
        // The community-capped entities are targets, not seeds — so this tests the bypass path
        // (None community => unlimited). Use a query that matches the community entities.
        let result = graph_recall(&store, None, &provider, "entity", 10, 2, None, 0.0, &[])
            .await
            .unwrap();
        // The key invariant: result must not be empty even with cap < total seeds
        assert!(
            !result.is_empty(),
            "SA-INV-10: community cap must not zero out all seeds"
        );
    }

    // Embedding fallback: when FTS5 returns 0 results and embeddings=None,
    // graph_recall must return empty (not error).
    #[tokio::test]
    async fn graph_recall_no_fts_match_no_embeddings_returns_empty() {
        let store = setup_store().await;
        // Populate graph with entities that won't match the query
        let a = store
            .upsert_entity(
                "Zephyr",
                "zephyr",
                crate::graph::types::EntityType::Concept,
                None,
            )
            .await
            .unwrap();
        let b = store
            .upsert_entity(
                "Concept",
                "concept",
                crate::graph::types::EntityType::Concept,
                None,
            )
            .await
            .unwrap();
        store
            .insert_edge(a, b, "rel", "Zephyr rel Concept", 0.9, None)
            .await
            .unwrap();

        let provider = mock_provider();
        // Query that won't match anything via FTS5; no embeddings available
        let result = graph_recall(
            &store,
            None,
            &provider,
            "xyzzyquuxfrob",
            10,
            2,
            None,
            0.0,
            &[],
        )
        .await
        .unwrap();
        assert!(
            result.is_empty(),
            "must return empty (not error) when FTS5 returns 0 and no embeddings available"
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
