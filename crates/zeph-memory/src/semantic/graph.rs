// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
use std::sync::atomic::Ordering;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::vector_store::VectorFilter;

use super::SemanticMemory;

/// Config for the spawned background extraction task.
///
/// Owned clone of the relevant fields from `GraphConfig` — no references, safe to send to
/// spawned tasks.
#[derive(Debug, Clone, Default)]
pub struct GraphExtractionConfig {
    pub max_entities: usize,
    pub max_edges: usize,
    pub extraction_timeout_secs: u64,
    pub community_refresh_interval: usize,
    pub expired_edge_retention_days: u32,
    pub max_entities_cap: usize,
    pub community_summary_max_prompt_bytes: usize,
    pub community_summary_concurrency: usize,
    pub lpa_edge_chunk_size: usize,
    /// A-MEM note linking config, cloned from `GraphConfig.note_linking`.
    pub note_linking: NoteLinkingConfig,
}

/// Config for A-MEM dynamic note linking, owned by the spawned extraction task.
#[derive(Debug, Clone)]
pub struct NoteLinkingConfig {
    pub enabled: bool,
    pub similarity_threshold: f32,
    pub top_k: usize,
    pub timeout_secs: u64,
}

impl Default for NoteLinkingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            similarity_threshold: 0.85,
            top_k: 10,
            timeout_secs: 5,
        }
    }
}

/// Stats returned from a completed extraction.
#[derive(Debug, Default)]
pub struct ExtractionStats {
    pub entities_upserted: usize,
    pub edges_inserted: usize,
}

/// Result returned from `extract_and_store`, combining stats with entity IDs needed for linking.
#[derive(Debug, Default)]
pub struct ExtractionResult {
    pub stats: ExtractionStats,
    /// IDs of entities upserted during this extraction pass. Passed to `link_memory_notes`.
    pub entity_ids: Vec<i64>,
}

/// Stats returned from a completed note-linking pass.
#[derive(Debug, Default)]
pub struct LinkingStats {
    pub entities_processed: usize,
    pub edges_created: usize,
}

/// Qdrant collection name for entity embeddings (mirrors the constant in `resolver.rs`).
const ENTITY_COLLECTION: &str = "zeph_graph_entities";

/// Link newly extracted entities to semantically similar entities in the graph.
///
/// For each entity in `entity_ids`:
/// 1. Load the entity name + summary from `SQLite`.
/// 2. Re-embed the entity text using `provider.embed()`.
/// 3. Search the entity embedding collection for the `top_k + 1` most similar points.
/// 4. Filter out the entity itself (by `qdrant_point_id`) and points below `similarity_threshold`.
/// 5. Insert a unidirectional `similar_to` edge where `source_id < target_id` to avoid
///    double-counting in BFS recall while still being traversable via the OR clause in
///    `edges_for_entity`. The edge confidence is set to the cosine similarity score.
///
/// Errors are logged and not propagated — this is a best-effort background enrichment step.
pub async fn link_memory_notes(
    entity_ids: &[i64],
    pool: sqlx::SqlitePool,
    embedding_store: Arc<EmbeddingStore>,
    provider: AnyProvider,
    cfg: &NoteLinkingConfig,
) -> LinkingStats {
    use crate::graph::GraphStore;

    let store = GraphStore::new(pool);
    let mut stats = LinkingStats::default();

    for &entity_id in entity_ids {
        let entity = match store.find_entity_by_id(entity_id).await {
            Ok(Some(e)) => e,
            Ok(None) => {
                tracing::debug!("note_linking: entity {entity_id} not found, skipping");
                continue;
            }
            Err(e) => {
                tracing::debug!("note_linking: DB error loading entity {entity_id}: {e:#}");
                continue;
            }
        };

        // Build embed text matching the pattern used during entity resolution.
        let embed_text = match &entity.summary {
            Some(s) if !s.is_empty() => format!("{}: {s}", entity.canonical_name),
            _ => entity.canonical_name.clone(),
        };

        let query_vec = match provider.embed(&embed_text).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    "note_linking: embed failed for entity {:?}: {e:#}",
                    entity.canonical_name
                );
                continue;
            }
        };

        let search_limit = cfg.top_k + 1; // +1 to account for self-match
        let results = match embedding_store
            .search_collection(
                ENTITY_COLLECTION,
                &query_vec,
                search_limit,
                None::<VectorFilter>,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(
                    "note_linking: search failed for entity {:?}: {e:#}",
                    entity.canonical_name
                );
                continue;
            }
        };

        stats.entities_processed += 1;

        // Filter: exclude self, exclude below-threshold, then take top_k.
        let self_point_id = entity.qdrant_point_id.as_deref();
        let candidates: Vec<_> = results
            .iter()
            .filter(|p| {
                // Exclude self by point_id comparison.
                Some(p.id.as_str()) != self_point_id && p.score >= cfg.similarity_threshold
            })
            .take(cfg.top_k)
            .collect();

        for point in candidates {
            let Some(target_id) = point
                .payload
                .get("entity_id")
                .and_then(serde_json::Value::as_i64)
            else {
                tracing::debug!(
                    "note_linking: missing entity_id in payload for point {}",
                    point.id
                );
                continue;
            };

            if target_id == entity_id {
                continue; // additional self-guard in case qdrant_point_id was null
            }

            // Unidirectional: only insert where source < target to avoid double-counting in BFS.
            // edges_for_entity uses "source = ? OR target = ?" so the edge is still traversable
            // in both directions without creating a duplicate in the opposite direction.
            let (src, tgt) = if entity_id < target_id {
                (entity_id, target_id)
            } else {
                (target_id, entity_id)
            };

            let fact = format!("Semantically similar entities (score: {:.3})", point.score);

            match store
                .insert_edge(src, tgt, "similar_to", &fact, point.score, None)
                .await
            {
                Ok(_) => stats.edges_created += 1,
                Err(e) => {
                    tracing::debug!("note_linking: insert_edge failed: {e:#}");
                }
            }
        }
    }

    stats
}

/// Extract entities and edges from `content` and persist them to the graph store.
///
/// This function runs inside a spawned task — it receives owned data only.
///
/// # Errors
///
/// Returns an error if the database query fails or LLM extraction fails.
pub async fn extract_and_store(
    content: String,
    context_messages: Vec<String>,
    provider: AnyProvider,
    pool: sqlx::SqlitePool,
    config: GraphExtractionConfig,
) -> Result<ExtractionResult, MemoryError> {
    use crate::graph::{EntityResolver, GraphExtractor, GraphStore};

    let extractor = GraphExtractor::new(provider, config.max_entities, config.max_edges);
    let ctx_refs: Vec<&str> = context_messages.iter().map(String::as_str).collect();

    let store = GraphStore::new(pool);

    let pool = store.pool();
    sqlx::query(
        "INSERT INTO graph_metadata (key, value) VALUES ('extraction_count', '0')
         ON CONFLICT(key) DO NOTHING",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE graph_metadata
         SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         WHERE key = 'extraction_count'",
    )
    .execute(pool)
    .await?;

    let Some(result) = extractor.extract(&content, &ctx_refs).await? else {
        return Ok(ExtractionResult::default());
    };

    let resolver = EntityResolver::new(&store);

    let mut entities_upserted = 0usize;
    let mut entity_name_to_id: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();

    for entity in &result.entities {
        match resolver
            .resolve(&entity.name, &entity.entity_type, entity.summary.as_deref())
            .await
        {
            Ok((id, _outcome)) => {
                entity_name_to_id.insert(entity.name.clone(), id);
                entities_upserted += 1;
            }
            Err(e) => {
                tracing::debug!("graph: skipping entity {:?}: {e:#}", entity.name);
            }
        }
    }

    let mut edges_inserted = 0usize;
    for edge in &result.edges {
        let (Some(&src_id), Some(&tgt_id)) = (
            entity_name_to_id.get(&edge.source),
            entity_name_to_id.get(&edge.target),
        ) else {
            tracing::debug!(
                "graph: skipping edge {:?}->{:?}: entity not resolved",
                edge.source,
                edge.target
            );
            continue;
        };
        match resolver
            .resolve_edge(src_id, tgt_id, &edge.relation, &edge.fact, 0.8, None)
            .await
        {
            Ok(Some(_)) => edges_inserted += 1,
            Ok(None) => {} // deduplicated
            Err(e) => {
                tracing::debug!("graph: skipping edge: {e:#}");
            }
        }
    }

    let new_entity_ids: Vec<i64> = entity_name_to_id.into_values().collect();

    Ok(ExtractionResult {
        stats: ExtractionStats {
            entities_upserted,
            edges_inserted,
        },
        entity_ids: new_entity_ids,
    })
}

impl SemanticMemory {
    /// Spawn background graph extraction for a message. Fire-and-forget — never blocks.
    ///
    /// Extraction runs in a separate tokio task with a timeout. Any error or timeout is
    /// logged and the task exits silently; the agent response is never blocked.
    ///
    /// When `config.note_linking.enabled` is `true` and an embedding store is available,
    /// `link_memory_notes` runs after successful extraction inside the same task, bounded
    /// by `config.note_linking.timeout_secs`.
    #[allow(clippy::too_many_lines)]
    pub fn spawn_graph_extraction(
        &self,
        content: String,
        context_messages: Vec<String>,
        config: GraphExtractionConfig,
    ) {
        let pool = self.sqlite.pool().clone();
        let provider = self.provider.clone();
        let failure_counter = self.community_detection_failures.clone();
        let extraction_count = self.graph_extraction_count.clone();
        let extraction_failures = self.graph_extraction_failures.clone();
        // Clone the embedding store Arc before moving into the task.
        let embedding_store = self.qdrant.clone();

        tokio::spawn(async move {
            let timeout_dur = std::time::Duration::from_secs(config.extraction_timeout_secs);
            let extraction_result = tokio::time::timeout(
                timeout_dur,
                extract_and_store(
                    content,
                    context_messages,
                    provider.clone(),
                    pool.clone(),
                    config.clone(),
                ),
            )
            .await;

            let (extraction_ok, new_entity_ids) = match extraction_result {
                Ok(Ok(result)) => {
                    tracing::debug!(
                        entities = result.stats.entities_upserted,
                        edges = result.stats.edges_inserted,
                        "graph extraction completed"
                    );
                    extraction_count.fetch_add(1, Ordering::Relaxed);
                    (true, result.entity_ids)
                }
                Ok(Err(e)) => {
                    tracing::warn!("graph extraction failed: {e:#}");
                    extraction_failures.fetch_add(1, Ordering::Relaxed);
                    (false, vec![])
                }
                Err(_elapsed) => {
                    tracing::warn!("graph extraction timed out");
                    extraction_failures.fetch_add(1, Ordering::Relaxed);
                    (false, vec![])
                }
            };

            // A-MEM note linking: run after successful extraction when enabled.
            if extraction_ok
                && config.note_linking.enabled
                && !new_entity_ids.is_empty()
                && let Some(store) = embedding_store
            {
                let linking_timeout =
                    std::time::Duration::from_secs(config.note_linking.timeout_secs);
                match tokio::time::timeout(
                    linking_timeout,
                    link_memory_notes(
                        &new_entity_ids,
                        pool.clone(),
                        store,
                        provider.clone(),
                        &config.note_linking,
                    ),
                )
                .await
                {
                    Ok(stats) => {
                        tracing::debug!(
                            entities_processed = stats.entities_processed,
                            edges_created = stats.edges_created,
                            "note linking completed"
                        );
                    }
                    Err(_elapsed) => {
                        tracing::debug!("note linking timed out (partial edges may exist)");
                    }
                }
            }

            if extraction_ok && config.community_refresh_interval > 0 {
                use crate::graph::GraphStore;

                let store = GraphStore::new(pool.clone());
                let extraction_count = store.extraction_count().await.unwrap_or(0);
                if extraction_count > 0
                    && i64::try_from(config.community_refresh_interval)
                        .is_ok_and(|interval| extraction_count % interval == 0)
                {
                    tracing::info!(extraction_count, "triggering community detection refresh");
                    let store2 = GraphStore::new(pool);
                    let provider2 = provider;
                    let retention_days = config.expired_edge_retention_days;
                    let max_cap = config.max_entities_cap;
                    let max_prompt_bytes = config.community_summary_max_prompt_bytes;
                    let concurrency = config.community_summary_concurrency;
                    let edge_chunk_size = config.lpa_edge_chunk_size;
                    tokio::spawn(async move {
                        match crate::graph::community::detect_communities(
                            &store2,
                            &provider2,
                            max_prompt_bytes,
                            concurrency,
                            edge_chunk_size,
                        )
                        .await
                        {
                            Ok(count) => {
                                tracing::info!(communities = count, "community detection complete");
                            }
                            Err(e) => {
                                tracing::warn!("community detection failed: {e:#}");
                                failure_counter.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        match crate::graph::community::run_graph_eviction(
                            &store2,
                            retention_days,
                            max_cap,
                        )
                        .await
                        {
                            Ok(stats) => {
                                tracing::info!(
                                    expired_edges = stats.expired_edges_deleted,
                                    orphan_entities = stats.orphan_entities_deleted,
                                    capped_entities = stats.capped_entities_deleted,
                                    "graph eviction complete"
                                );
                            }
                            Err(e) => {
                                tracing::warn!("graph eviction failed: {e:#}");
                            }
                        }
                    });
                }
            }
        });
    }
}
