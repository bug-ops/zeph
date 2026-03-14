// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::atomic::Ordering;

use zeph_llm::any::AnyProvider;

use crate::error::MemoryError;

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
}

/// Stats returned from a completed extraction.
#[derive(Debug, Default)]
pub struct ExtractionStats {
    pub entities_upserted: usize,
    pub edges_inserted: usize,
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
) -> Result<ExtractionStats, MemoryError> {
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
        return Ok(ExtractionStats::default());
    };

    let resolver = EntityResolver::new(&store);

    let mut entities_upserted = 0usize;
    let mut entity_ids: std::collections::HashMap<String, i64> = std::collections::HashMap::new();

    for entity in &result.entities {
        match resolver
            .resolve(&entity.name, &entity.entity_type, entity.summary.as_deref())
            .await
        {
            Ok((id, _outcome)) => {
                entity_ids.insert(entity.name.clone(), id);
                entities_upserted += 1;
            }
            Err(e) => {
                tracing::debug!("graph: skipping entity {:?}: {e:#}", entity.name);
            }
        }
    }

    let mut edges_inserted = 0usize;
    for edge in &result.edges {
        let (Some(&src_id), Some(&tgt_id)) =
            (entity_ids.get(&edge.source), entity_ids.get(&edge.target))
        else {
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

    Ok(ExtractionStats {
        entities_upserted,
        edges_inserted,
    })
}

impl SemanticMemory {
    /// Spawn background graph extraction for a message. Fire-and-forget — never blocks.
    ///
    /// Extraction runs in a separate tokio task with a timeout. Any error or timeout is
    /// logged and the task exits silently; the agent response is never blocked.
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

        tokio::spawn(async move {
            let timeout_dur = std::time::Duration::from_secs(config.extraction_timeout_secs);
            let extraction_ok = match tokio::time::timeout(
                timeout_dur,
                extract_and_store(
                    content,
                    context_messages,
                    provider.clone(),
                    pool.clone(),
                    config.clone(),
                ),
            )
            .await
            {
                Ok(Ok(stats)) => {
                    tracing::debug!(
                        entities = stats.entities_upserted,
                        edges = stats.edges_inserted,
                        "graph extraction completed"
                    );
                    extraction_count.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Ok(Err(e)) => {
                    tracing::warn!("graph extraction failed: {e:#}");
                    extraction_failures.fetch_add(1, Ordering::Relaxed);
                    false
                }
                Err(_elapsed) => {
                    tracing::warn!("graph extraction timed out");
                    extraction_failures.fetch_add(1, Ordering::Relaxed);
                    false
                }
            };

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
