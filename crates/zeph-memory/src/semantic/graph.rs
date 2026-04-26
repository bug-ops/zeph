// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
#[allow(unused_imports)]
use zeph_db::sql;

use std::sync::atomic::Ordering;
use zeph_db::DbPool;

pub use zeph_common::config::memory::NoteLinkingConfig;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::graph::extractor::ExtractionResult as ExtractorResult;
use crate::vector_store::VectorFilter;

use super::SemanticMemory;

/// Callback type for post-extraction validation.
///
/// A generic predicate opaque to zeph-memory — callers (zeph-core) provide security
/// validation without introducing a dependency on security policy in this crate.
pub type PostExtractValidator = Option<Box<dyn Fn(&ExtractorResult) -> Result<(), String> + Send>>;

/// Config for the spawned background extraction task.
///
/// Owned clone of the relevant fields from `GraphConfig` — no references, safe to send to
/// spawned tasks.
#[derive(Debug, Clone)]
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
    /// A-MEM link weight decay lambda. Range: `(0.0, 1.0]`. Default: `0.95`.
    pub link_weight_decay_lambda: f64,
    /// Seconds between link weight decay passes. Default: `86400`.
    pub link_weight_decay_interval_secs: u64,
    /// Kumiho belief revision: enable semantic contradiction detection for edges.
    pub belief_revision_enabled: bool,
    /// Cosine similarity threshold for belief revision contradiction detection.
    pub belief_revision_similarity_threshold: f32,
    /// GAAMA episode linking: `conversation_id` to link extracted entities to their episode.
    /// `None` disables episode linking for this extraction pass.
    pub conversation_id: Option<i64>,
}

impl Default for GraphExtractionConfig {
    fn default() -> Self {
        Self {
            max_entities: 0,
            max_edges: 0,
            extraction_timeout_secs: 0,
            community_refresh_interval: 0,
            expired_edge_retention_days: 0,
            max_entities_cap: 0,
            community_summary_max_prompt_bytes: 0,
            community_summary_concurrency: 0,
            lpa_edge_chunk_size: 0,
            note_linking: NoteLinkingConfig::default(),
            link_weight_decay_lambda: 0.95,
            link_weight_decay_interval_secs: 86400,
            belief_revision_enabled: false,
            belief_revision_similarity_threshold: 0.85,
            conversation_id: None,
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

/// Work item for a single entity during a note-linking pass.
struct EntityWorkItem {
    entity_id: i64,
    canonical_name: String,
    embed_text: String,
    self_point_id: Option<String>,
}

/// Link newly extracted entities to semantically similar entities in the graph.
///
/// For each entity in `entity_ids`:
/// 1. Load the entity name + summary from `SQLite`.
/// 2. Embed all entity texts in parallel.
/// 3. Search the entity embedding collection in parallel for the `top_k + 1` most similar points.
/// 4. Filter out the entity itself (by `qdrant_point_id` or `entity_id` payload) and points
///    below `similarity_threshold`.
/// 5. Insert a unidirectional `similar_to` edge where `source_id < target_id` to avoid
///    double-counting in BFS recall while still being traversable via the OR clause in
///    `edges_for_entity`. The edge confidence is set to the cosine similarity score.
/// 6. Deduplicate pairs within a single pass so that a pair encountered from both A→B and B→A
///    directions is only inserted once, keeping `edges_created` accurate.
///
/// Errors are logged and not propagated — this is a best-effort background enrichment step.
#[allow(clippy::too_many_lines)] // long function; decomposition would require extracting state into additional structs — TODO(#3443): decompose into smaller helpers
pub async fn link_memory_notes(
    entity_ids: &[i64],
    pool: DbPool,
    embedding_store: Arc<EmbeddingStore>,
    provider: AnyProvider,
    cfg: &NoteLinkingConfig,
) -> LinkingStats {
    use futures::future;

    use crate::graph::GraphStore;

    let store = GraphStore::new(pool);
    let mut stats = LinkingStats::default();

    // Phase 1: load entities from DB sequentially (cheap; avoids connection-pool contention).
    let mut work_items: Vec<EntityWorkItem> = Vec::with_capacity(entity_ids.len());
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
        let embed_text = match &entity.summary {
            Some(s) if !s.is_empty() => format!("{}: {s}", entity.canonical_name),
            _ => entity.canonical_name.clone(),
        };
        work_items.push(EntityWorkItem {
            entity_id,
            canonical_name: entity.canonical_name,
            embed_text,
            self_point_id: entity.qdrant_point_id,
        });
    }

    if work_items.is_empty() {
        return stats;
    }

    // Phase 2: embed all entity texts in parallel to reduce N serial HTTP round-trips to 1.
    let embed_results: Vec<_> =
        future::join_all(work_items.iter().map(|w| provider.embed(&w.embed_text))).await;

    // Phase 3: search for similar entities in parallel for all successfully embedded entities.
    let search_limit = cfg.top_k + 1; // +1 to account for self-match
    let valid: Vec<(usize, Vec<f32>)> = embed_results
        .into_iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            Ok(v) => Some((i, v)),
            Err(e) => {
                tracing::debug!(
                    "note_linking: embed failed for entity {:?}: {e:#}",
                    work_items[i].canonical_name
                );
                None
            }
        })
        .collect();

    let search_results: Vec<_> = future::join_all(valid.iter().map(|(_, vec)| {
        embedding_store.search_collection(
            ENTITY_COLLECTION,
            vec,
            search_limit,
            None::<VectorFilter>,
        )
    }))
    .await;

    // Phase 4: insert edges; deduplicate pairs seen from both A→B and B→A directions.
    // Without deduplication, both directions call insert_edge for the same normalised pair and
    // both return Ok (the second call updates confidence on the existing row), inflating
    // edges_created by the number of bidirectional hits.
    let mut seen_pairs = std::collections::HashSet::new();

    for ((work_idx, _), search_result) in valid.iter().zip(search_results.iter()) {
        let w = &work_items[*work_idx];

        let results = match search_result {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(
                    "note_linking: search failed for entity {:?}: {e:#}",
                    w.canonical_name
                );
                continue;
            }
        };

        stats.entities_processed += 1;

        let self_point_id = w.self_point_id.as_deref();
        let candidates = results
            .iter()
            .filter(|p| Some(p.id.as_str()) != self_point_id && p.score >= cfg.similarity_threshold)
            .take(cfg.top_k);

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

            if target_id == w.entity_id {
                continue; // secondary self-guard when qdrant_point_id is null
            }

            // Normalise direction: always store source_id < target_id.
            let (src, tgt) = if w.entity_id < target_id {
                (w.entity_id, target_id)
            } else {
                (target_id, w.entity_id)
            };

            // Skip pairs already processed in this pass to avoid double-counting.
            if !seen_pairs.insert((src, tgt)) {
                continue;
            }

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
/// The optional `embedding_store` enables entity embedding storage in Qdrant, which is
/// required for A-MEM note linking to find semantically similar entities across sessions.
///
/// # Errors
///
/// Returns an error if the database query fails or LLM extraction fails.
#[cfg_attr(
    feature = "profiling",
    tracing::instrument(name = "memory.graph_extract", skip_all, fields(entities = tracing::field::Empty, edges = tracing::field::Empty))
)]
#[allow(clippy::too_many_lines)] // long function; decomposition would require extracting state into additional structs — TODO(#3443): decompose into smaller helpers
pub async fn extract_and_store(
    content: String,
    context_messages: Vec<String>,
    provider: AnyProvider,
    pool: DbPool,
    config: GraphExtractionConfig,
    post_extract_validator: PostExtractValidator,
    embedding_store: Option<Arc<EmbeddingStore>>,
) -> Result<ExtractionResult, MemoryError> {
    use crate::graph::{EntityResolver, GraphExtractor, GraphStore};

    let extractor = GraphExtractor::new(provider.clone(), config.max_entities, config.max_edges);
    let ctx_refs: Vec<&str> = context_messages.iter().map(String::as_str).collect();

    let store = GraphStore::new(pool);

    let pool = store.pool();
    zeph_db::query(sql!(
        "INSERT INTO graph_metadata (key, value) VALUES ('extraction_count', '0')
         ON CONFLICT(key) DO NOTHING"
    ))
    .execute(pool)
    .await?;
    zeph_db::query(sql!(
        "UPDATE graph_metadata
         SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
         WHERE key = 'extraction_count'"
    ))
    .execute(pool)
    .await?;

    let Some(result) = extractor.extract(&content, &ctx_refs).await? else {
        return Ok(ExtractionResult::default());
    };

    // Post-extraction validation callback. zeph-memory does not know the callback is a
    // security validator — it is a generic predicate opaque to this crate (design decision D1).
    if let Some(ref validator) = post_extract_validator
        && let Err(reason) = validator(&result)
    {
        tracing::warn!(
            reason,
            "graph extraction validation failed, skipping upsert"
        );
        return Ok(ExtractionResult::default());
    }

    let resolver = if let Some(ref emb) = embedding_store {
        EntityResolver::new(&store)
            .with_embedding_store(emb)
            .with_provider(&provider)
    } else {
        EntityResolver::new(&store)
    };

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
        if src_id == tgt_id {
            tracing::debug!(
                "graph: skipping self-loop edge {:?}->{:?} (entity_id={src_id})",
                edge.source,
                edge.target
            );
            continue;
        }
        // Parse LLM-provided edge_type; default to Semantic on any parse failure so
        // edges are never dropped due to classification errors.
        let edge_type = edge
            .edge_type
            .parse::<crate::graph::EdgeType>()
            .unwrap_or_else(|_| {
                tracing::warn!(
                    raw_type = %edge.edge_type,
                    "graph: unknown edge_type from LLM, defaulting to semantic"
                );
                crate::graph::EdgeType::Semantic
            });
        let belief_cfg =
            config
                .belief_revision_enabled
                .then_some(crate::graph::BeliefRevisionConfig {
                    similarity_threshold: config.belief_revision_similarity_threshold,
                });
        match resolver
            .resolve_edge_typed(
                src_id,
                tgt_id,
                &edge.relation,
                &edge.fact,
                0.8,
                None,
                edge_type,
                belief_cfg.as_ref(),
            )
            .await
        {
            Ok(Some(_)) => edges_inserted += 1,
            Ok(None) => {} // deduplicated
            Err(e) => {
                tracing::debug!("graph: skipping edge: {e:#}");
            }
        }
    }

    store.checkpoint_wal().await?;

    let new_entity_ids: Vec<i64> = entity_name_to_id.into_values().collect();

    // GAAMA episode linking: link all extracted entities to the episode for this conversation.
    if let Some(conv_id) = config.conversation_id {
        match store.ensure_episode(conv_id).await {
            Ok(episode_id) => {
                for &entity_id in &new_entity_ids {
                    if let Err(e) = store.link_entity_to_episode(episode_id, entity_id).await {
                        tracing::debug!("episode linking skipped for entity {entity_id}: {e:#}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("failed to ensure episode for conversation {conv_id}: {e:#}");
            }
        }
    }

    #[cfg(feature = "profiling")]
    {
        let span = tracing::Span::current();
        span.record("entities", entities_upserted);
        span.record("edges", edges_inserted);
    }

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
    /// The optional `post_extract_validator` is called after extraction, before upsert.
    /// It is a generic predicate opaque to zeph-memory (design decision D1).
    ///
    /// When `config.note_linking.enabled` is `true` and an embedding store is available,
    /// `link_memory_notes` runs after successful extraction inside the same task, bounded
    /// by `config.note_linking.timeout_secs`.
    #[allow(clippy::too_many_lines)] // long function; decomposition would require extracting state into additional structs — TODO(#3443): decompose into smaller helpers
    pub fn spawn_graph_extraction(
        &self,
        content: String,
        context_messages: Vec<String>,
        config: GraphExtractionConfig,
        post_extract_validator: PostExtractValidator,
    ) -> tokio::task::JoinHandle<()> {
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
                    post_extract_validator,
                    embedding_store.clone(),
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
                    let decay_lambda = config.link_weight_decay_lambda;
                    let decay_interval_secs = config.link_weight_decay_interval_secs;
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

                        // Time-based link weight decay — independent of eviction cycle.
                        if decay_lambda > 0.0 && decay_interval_secs > 0 {
                            let now_secs = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(0, |d| d.as_secs());
                            let last_decay = store2
                                .get_metadata("last_link_weight_decay_at")
                                .await
                                .ok()
                                .flatten()
                                .and_then(|s| s.parse::<u64>().ok())
                                .unwrap_or(0);
                            if now_secs.saturating_sub(last_decay) >= decay_interval_secs {
                                match store2
                                    .decay_edge_retrieval_counts(decay_lambda, decay_interval_secs)
                                    .await
                                {
                                    Ok(affected) => {
                                        tracing::info!(affected, "link weight decay applied");
                                        let _ = store2
                                            .set_metadata(
                                                "last_link_weight_decay_at",
                                                &now_secs.to_string(),
                                            )
                                            .await;
                                    }
                                    Err(e) => {
                                        tracing::warn!("link weight decay failed: {e:#}");
                                    }
                                }
                            }
                        }
                    });
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_llm::any::AnyProvider;

    use super::extract_and_store;
    use crate::embedding_store::EmbeddingStore;
    use crate::graph::GraphStore;
    use crate::in_memory_store::InMemoryVectorStore;
    use crate::store::SqliteStore;

    use super::GraphExtractionConfig;

    async fn setup() -> (GraphStore, Arc<EmbeddingStore>) {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();
        let mem_store = Box::new(InMemoryVectorStore::new());
        let emb = Arc::new(EmbeddingStore::with_store(mem_store, pool.clone()));
        let gs = GraphStore::new(pool);
        (gs, emb)
    }

    /// Regression test for #1829: `extract_and_store()` must pass the provider to `EntityResolver`
    /// so that `store_entity_embedding()` is called and `qdrant_point_id` is set in `SQLite`.
    #[tokio::test]
    async fn extract_and_store_sets_qdrant_point_id_when_embedding_store_provided() {
        let (gs, emb) = setup().await;

        // MockProvider: supports embeddings, returns a valid extraction JSON for chat
        let extraction_json = r#"{"entities":[{"name":"Rust","type":"language","summary":"systems language"}],"edges":[]}"#;
        let mut mock =
            zeph_llm::mock::MockProvider::with_responses(vec![extraction_json.to_owned()]);
        mock.supports_embeddings = true;
        mock.embedding = vec![1.0_f32, 0.0, 0.0, 0.0];
        let provider = AnyProvider::Mock(mock);

        let config = GraphExtractionConfig {
            max_entities: 10,
            max_edges: 10,
            extraction_timeout_secs: 10,
            ..Default::default()
        };

        let result = extract_and_store(
            "Rust is a systems programming language.".to_owned(),
            vec![],
            provider,
            gs.pool().clone(),
            config,
            None,
            Some(emb.clone()),
        )
        .await
        .unwrap();

        assert_eq!(
            result.stats.entities_upserted, 1,
            "one entity should be upserted"
        );

        // The entity must have a qdrant_point_id — this proves store_entity_embedding() was called.
        // Before the fix, EntityResolver was built without a provider, so embed() was never called
        // and qdrant_point_id remained NULL.
        let entity = gs
            .find_entity("rust", crate::graph::EntityType::Language)
            .await
            .unwrap()
            .expect("entity 'rust' must exist in SQLite");

        assert!(
            entity.qdrant_point_id.is_some(),
            "qdrant_point_id must be set when embedding_store + provider are both provided (regression for #1829)"
        );
    }

    /// When no `embedding_store` is provided, `extract_and_store()` must still work correctly
    /// (no embeddings stored, but entities are still upserted).
    #[tokio::test]
    async fn extract_and_store_without_embedding_store_still_upserts_entities() {
        let (gs, _emb) = setup().await;

        let extraction_json = r#"{"entities":[{"name":"Python","type":"language","summary":"scripting"}],"edges":[]}"#;
        let mock = zeph_llm::mock::MockProvider::with_responses(vec![extraction_json.to_owned()]);
        let provider = AnyProvider::Mock(mock);

        let config = GraphExtractionConfig {
            max_entities: 10,
            max_edges: 10,
            extraction_timeout_secs: 10,
            ..Default::default()
        };

        let result = extract_and_store(
            "Python is a scripting language.".to_owned(),
            vec![],
            provider,
            gs.pool().clone(),
            config,
            None,
            None, // no embedding_store
        )
        .await
        .unwrap();

        assert_eq!(result.stats.entities_upserted, 1);

        let entity = gs
            .find_entity("python", crate::graph::EntityType::Language)
            .await
            .unwrap()
            .expect("entity 'python' must exist");

        assert!(
            entity.qdrant_point_id.is_none(),
            "qdrant_point_id must remain None when no embedding_store is provided"
        );
    }

    /// Regression test for #2166: FTS5 entity writes must be visible to a new connection pool
    /// opened after extraction completes. Without `checkpoint_wal()` in `extract_and_store`,
    /// a fresh pool sees stale FTS5 shadow tables and `find_entities_fuzzy` returns empty.
    #[tokio::test]
    async fn extract_and_store_fts5_cross_session_visibility() {
        let file = tempfile::NamedTempFile::new().expect("tempfile");
        let path = file.path().to_str().expect("valid path").to_string();

        // Session A: run extract_and_store on a file DB (not :memory:) so WAL is used.
        {
            let sqlite = crate::store::SqliteStore::new(&path).await.unwrap();
            let extraction_json = r#"{"entities":[{"name":"Ferris","type":"concept","summary":"Rust mascot"}],"edges":[]}"#;
            let mock =
                zeph_llm::mock::MockProvider::with_responses(vec![extraction_json.to_owned()]);
            let provider = AnyProvider::Mock(mock);
            let config = GraphExtractionConfig {
                max_entities: 10,
                max_edges: 10,
                extraction_timeout_secs: 10,
                ..Default::default()
            };
            extract_and_store(
                "Ferris is the Rust mascot.".to_owned(),
                vec![],
                provider,
                sqlite.pool().clone(),
                config,
                None,
                None,
            )
            .await
            .unwrap();
        }

        // Session B: new pool — FTS5 must see the entity extracted in session A.
        let sqlite_b = crate::store::SqliteStore::new(&path).await.unwrap();
        let gs_b = crate::graph::GraphStore::new(sqlite_b.pool().clone());
        let results = gs_b.find_entities_fuzzy("Ferris", 10).await.unwrap();
        assert!(
            !results.is_empty(),
            "FTS5 cross-session (#2166): entity extracted in session A must be visible in session B"
        );
    }

    /// Regression test for #2215: self-loop edges (source == target entity) must be silently
    /// skipped; no edge row should be inserted.
    #[tokio::test]
    async fn extract_and_store_skips_self_loop_edges() {
        let (gs, _emb) = setup().await;

        // LLM returns one entity and one self-loop edge (source == target).
        let extraction_json = r#"{
            "entities":[{"name":"Rust","type":"language","summary":"systems language"}],
            "edges":[{"source":"Rust","target":"Rust","relation":"is","fact":"Rust is Rust","edge_type":"semantic"}]
        }"#;
        let mock = zeph_llm::mock::MockProvider::with_responses(vec![extraction_json.to_owned()]);
        let provider = AnyProvider::Mock(mock);

        let config = GraphExtractionConfig {
            max_entities: 10,
            max_edges: 10,
            extraction_timeout_secs: 10,
            ..Default::default()
        };

        let result = extract_and_store(
            "Rust is a language.".to_owned(),
            vec![],
            provider,
            gs.pool().clone(),
            config,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.stats.entities_upserted, 1);
        assert_eq!(
            result.stats.edges_inserted, 0,
            "self-loop edge must be rejected (#2215)"
        );
    }
}
