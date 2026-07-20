// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`zeph_commands::GraphAccess`] implementation for [`Agent<C>`]: graph memory (entities,
//! edges, communities, backfill) and the knowledge-ingest ledger.
//!
//! Each method returns a formatted `String` result (without sending to the channel
//! directly), so that `CommandContext::sink` does not conflict with this borrow — these
//! subsystems are already channel-free.
//!
//! [`Agent<C>`]: super::Agent

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tracing::Instrument as _;
use zeph_commands::{CommandError, GraphAccess};
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::{Edge, Entity, GraphExtractionConfig, GraphStore, extract_and_store};

use super::Agent;
use crate::channel::Channel;

impl<C: Channel + Send + 'static> Agent<C> {
    fn resolve_graph_store(&self) -> Result<(Arc<SemanticMemory>, Arc<GraphStore>), String> {
        let Some(memory) = self.services.memory.persistence.memory.clone() else {
            return Err("Graph memory is not enabled.".to_owned());
        };
        let Some(store) = memory.graph_store.clone() else {
            if self.services.memory.extraction.graph_config.enabled {
                return Err(
                    "Graph memory enabled but vector store unavailable (Qdrant unreachable)."
                        .to_owned(),
                );
            }
            return Err("Graph memory is not enabled.".to_owned());
        };
        Ok((memory, store))
    }
}

/// Outcome of resolving an entity by display name against the graph store: either the
/// entity was found, or a user-facing message that the caller should return as-is (no
/// match, or the store timed out).
enum EntityLookup {
    Found(Entity),
    Message(String),
}

/// Outcome of a graph-store call bounded by [`with_graph_store_timeout`]'s 5s deadline:
/// either it completed, or it timed out (Qdrant unreachable).
enum StoreCallOutcome<T> {
    Completed(T),
    TimedOut,
}

/// Runs `fut` under a 5s timeout — the deadline shared by `resolve_entity_by_name` and the
/// edge lookups in `graph_facts`/`graph_history`. Maps a store error to [`CommandError`];
/// logs and reports a timeout via [`StoreCallOutcome::TimedOut`] so callers only need to
/// turn that into their own user-facing message.
async fn with_graph_store_timeout<T>(
    fut: impl Future<Output = Result<T, zeph_memory::MemoryError>>,
) -> Result<StoreCallOutcome<T>, CommandError> {
    match tokio::time::timeout(Duration::from_secs(5), fut).await {
        Ok(Ok(v)) => Ok(StoreCallOutcome::Completed(v)),
        Ok(Err(e)) => Err(CommandError::new(e.to_string())),
        Err(_) => {
            tracing::warn!("graph store call timed out after 5s (Qdrant unreachable)");
            Ok(StoreCallOutcome::TimedOut)
        }
    }
}

/// Resolves `name` to an [`Entity`] via [`GraphStore::find_entity_by_name`], bounded by a
/// 5s timeout — the lookup block shared by `graph_facts` and `graph_history`.
async fn resolve_entity_by_name(
    store: &GraphStore,
    name: &str,
) -> Result<EntityLookup, CommandError> {
    let matches = match with_graph_store_timeout(store.find_entity_by_name(name)).await? {
        StoreCallOutcome::Completed(v) => v,
        StoreCallOutcome::TimedOut => {
            return Ok(EntityLookup::Message(
                "Graph store unavailable (Qdrant unreachable).".to_owned(),
            ));
        }
    };
    let Some(entity) = matches.into_iter().next() else {
        return Ok(EntityLookup::Message(format!(
            "No entity found matching '{name}'."
        )));
    };
    Ok(EntityLookup::Found(entity))
}

/// Builds the `entity_id -> display_name` lookup map shared by `graph_facts` and
/// `graph_history`: seeds `entity`'s own name, inserts a placeholder for every edge
/// endpoint, then resolves each placeholder via [`GraphStore::find_entity_by_id`] (5s
/// timeout), falling back to `#{id}` when the lookup fails or times out.
async fn build_entity_name_map(
    store: &GraphStore,
    entity: &Entity,
    edges: &[Edge],
) -> std::collections::HashMap<i64, String> {
    let mut entity_names: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    entity_names.insert(entity.id.0, entity.name.clone());
    for edge in edges {
        entity_names.entry(edge.source_entity_id).or_default();
        entity_names.entry(edge.target_entity_id).or_default();
    }
    for (&id, name_val) in &mut entity_names {
        if name_val.is_empty() {
            let result =
                tokio::time::timeout(Duration::from_secs(5), store.find_entity_by_id(id)).await;
            if let Ok(Ok(Some(other))) = result {
                *name_val = other.name;
            } else {
                *name_val = format!("#{id}");
            }
        }
    }
    entity_names
}

impl<C: Channel + Send + 'static> GraphAccess for Agent<C> {
    // ----- /graph -----

    fn graph_stats<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                let (_, store) = match self.resolve_graph_store() {
                    Ok(pair) => pair,
                    Err(msg) => return Ok(msg),
                };

                let stats_future = async {
                    tokio::join!(
                        store.entity_count(),
                        store.active_edge_count(),
                        store.community_count(),
                        store.edge_type_distribution()
                    )
                };
                let Ok((entities, edges, communities, distribution)) =
                    tokio::time::timeout(Duration::from_secs(5), stats_future).await
                else {
                    tracing::warn!("graph store call timed out after 5s (Qdrant unreachable)");
                    return Ok("Graph store unavailable (Qdrant unreachable).".to_owned());
                };
                let mut msg = format!(
                    "Graph memory: {} entities, {} edges, {} communities",
                    entities.unwrap_or(0),
                    edges.unwrap_or(0),
                    communities.unwrap_or(0)
                );
                if let Ok(dist) = distribution
                    && !dist.is_empty()
                {
                    let dist_str: Vec<String> =
                        dist.iter().map(|(t, c)| format!("{t}={c}")).collect();
                    write!(msg, "\nEdge types: {}", dist_str.join(", ")).unwrap_or(());
                }
                Ok(msg)
            }
            .instrument(tracing::info_span!("core.agent_access.graph_stats")),
        )
    }

    fn graph_entities<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                let (_, store) = match self.resolve_graph_store() {
                    Ok(pair) => pair,
                    Err(msg) => return Ok(msg),
                };

                let entities = match tokio::time::timeout(
                    Duration::from_secs(5),
                    store.all_entities(),
                )
                .await
                {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => return Err(CommandError::new(e.to_string())),
                    Err(_) => {
                        tracing::warn!("graph store call timed out after 5s (Qdrant unreachable)");
                        return Ok("Graph store unavailable (Qdrant unreachable).".to_owned());
                    }
                };
                if entities.is_empty() {
                    return Ok("No entities found.".to_owned());
                }

                let total = entities.len();
                let display: Vec<String> = entities
                    .iter()
                    .take(50)
                    .map(|e| {
                        format!(
                            "  {:<40}  {:<15}  {}",
                            e.name,
                            e.entity_type.as_str(),
                            e.last_seen_at.split('T').next().unwrap_or(&e.last_seen_at)
                        )
                    })
                    .collect();
                let mut msg = format!(
                    "Entities ({total} total):\n  {:<40}  {:<15}  {}\n{}",
                    "NAME",
                    "TYPE",
                    "LAST SEEN",
                    display.join("\n")
                );
                if total > 50 {
                    write!(msg, "\n  ...and {} more", total - 50).unwrap_or(());
                }
                Ok(msg)
            }
            .instrument(tracing::info_span!("core.agent_access.graph_entities")),
        )
    }

    fn graph_facts<'a>(
        &'a mut self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                let (_, store) = match self.resolve_graph_store() {
                    Ok(pair) => pair,
                    Err(msg) => return Ok(msg),
                };

                let entity = match resolve_entity_by_name(&store, name).await? {
                    EntityLookup::Found(e) => e,
                    EntityLookup::Message(msg) => return Ok(msg),
                };

                let edges =
                    match with_graph_store_timeout(store.edges_for_entity(entity.id.0)).await? {
                        StoreCallOutcome::Completed(v) => v,
                        StoreCallOutcome::TimedOut => {
                            return Ok("Graph store unavailable (Qdrant unreachable).".to_owned());
                        }
                    };
                if edges.is_empty() {
                    return Ok(format!("Entity '{}' has no known facts.", entity.name));
                }

                let entity_names = build_entity_name_map(&store, &entity, &edges).await;

                let lines: Vec<String> = edges
                    .iter()
                    .map(|e| {
                        let src = entity_names
                            .get(&e.source_entity_id)
                            .cloned()
                            .unwrap_or_else(|| format!("#{}", e.source_entity_id));
                        let tgt = entity_names
                            .get(&e.target_entity_id)
                            .cloned()
                            .unwrap_or_else(|| format!("#{}", e.target_entity_id));
                        format!(
                            "  {} --[{}/{}]--> {}: {} (confidence: {:.2})",
                            src, e.relation, e.edge_type, tgt, e.fact, e.confidence
                        )
                    })
                    .collect();
                Ok(format!(
                    "Facts for '{}':\n{}",
                    entity.name,
                    lines.join("\n")
                ))
            }
            .instrument(tracing::info_span!("core.agent_access.graph_facts")),
        )
    }

    fn graph_history<'a>(
        &'a mut self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                let (_, store) = match self.resolve_graph_store() {
                    Ok(pair) => pair,
                    Err(msg) => return Ok(msg),
                };

                let entity = match resolve_entity_by_name(&store, name).await? {
                    EntityLookup::Found(e) => e,
                    EntityLookup::Message(msg) => return Ok(msg),
                };

                let edges =
                    match with_graph_store_timeout(store.edge_history_for_entity(entity.id.0, 50))
                        .await?
                    {
                        StoreCallOutcome::Completed(v) => v,
                        StoreCallOutcome::TimedOut => {
                            return Ok("Graph store unavailable (Qdrant unreachable).".to_owned());
                        }
                    };
                if edges.is_empty() {
                    return Ok(format!("Entity '{}' has no edge history.", entity.name));
                }

                let entity_names = build_entity_name_map(&store, &entity, &edges).await;

                let n = edges.len();
                let lines: Vec<String> = edges
                    .iter()
                    .map(|e| {
                        let status = if e.valid_to.is_some() {
                            let date = e
                                .valid_to
                                .as_deref()
                                .and_then(|s| s.split('T').next().or_else(|| s.split(' ').next()))
                                .unwrap_or("?");
                            format!("[expired {date}]")
                        } else {
                            "[active]".to_string()
                        };
                        let src = entity_names
                            .get(&e.source_entity_id)
                            .cloned()
                            .unwrap_or_else(|| format!("#{}", e.source_entity_id));
                        let tgt = entity_names
                            .get(&e.target_entity_id)
                            .cloned()
                            .unwrap_or_else(|| format!("#{}", e.target_entity_id));
                        format!(
                            "  {status} {} --[{}/{}]--> {}: {} (confidence: {:.2})",
                            src, e.relation, e.edge_type, tgt, e.fact, e.confidence
                        )
                    })
                    .collect();
                Ok(format!(
                    "Edge history for '{}' ({n} edges):\n{}",
                    entity.name,
                    lines.join("\n")
                ))
            }
            .instrument(tracing::info_span!("core.agent_access.graph_history")),
        )
    }

    fn graph_communities<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                let (_, store) = match self.resolve_graph_store() {
                    Ok(pair) => pair,
                    Err(msg) => return Ok(msg),
                };

                let communities =
                    match tokio::time::timeout(Duration::from_secs(5), store.all_communities())
                        .await
                    {
                        Ok(Ok(v)) => v,
                        Ok(Err(e)) => return Err(CommandError::new(e.to_string())),
                        Err(_) => {
                            tracing::warn!(
                                "graph store call timed out after 5s (Qdrant unreachable)"
                            );
                            return Ok("Graph store unavailable (Qdrant unreachable).".to_owned());
                        }
                    };
                if communities.is_empty() {
                    return Ok("No communities detected yet. Run graph backfill first.".to_owned());
                }

                let lines: Vec<String> = communities
                    .iter()
                    .map(|c| format!("  [{}]: {}", c.name, c.summary))
                    .collect();
                Ok(format!(
                    "Communities ({}):\n{}",
                    communities.len(),
                    lines.join("\n")
                ))
            }
            .instrument(tracing::info_span!("core.agent_access.graph_communities")),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn graph_backfill<'a>(
        &'a mut self,
        limit: Option<usize>,
        progress_cb: &'a mut (dyn FnMut(String) + Send),
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        let store = match self.resolve_graph_store() {
            Ok((_, s)) => s,
            Err(msg) => return Box::pin(async move { Ok(msg) }),
        };
        let graph_cfg = self.services.memory.extraction.graph_config.clone();
        let embed_timeout_secs = self
            .services
            .memory
            .persistence
            .memory
            .as_ref()
            .map_or(5, |m| m.embed_timeout().as_secs());
        let provider = if graph_cfg.extract_provider.as_str().is_empty() {
            self.provider.clone()
        } else {
            self.resolve_background_provider(graph_cfg.extract_provider.as_str())
        };
        Box::pin(
            async move {
                let total = store.unprocessed_message_count().await.unwrap_or(0);
                let cap = limit.unwrap_or(usize::MAX);

                progress_cb(format!(
                    "Starting graph backfill... ({total} unprocessed messages)"
                ));

                let batch_size = 50usize;
                let mut processed = 0usize;
                let mut total_entities = 0usize;
                let mut total_edges = 0usize;

                loop {
                    let remaining_cap = cap.saturating_sub(processed);
                    if remaining_cap == 0 {
                        break;
                    }
                    let batch_limit = batch_size.min(remaining_cap);
                    let messages = store
                        .unprocessed_messages_for_backfill(batch_limit)
                        .await
                        .map_err(|e| CommandError::new(e.to_string()))?;
                    if messages.is_empty() {
                        break;
                    }

                    let ids: Vec<zeph_memory::types::MessageId> =
                        messages.iter().map(|(id, _)| *id).collect();

                    // extraction_cfg is loop-invariant (derived only from graph_cfg /
                    // embed_timeout_secs, never from message content), so it is built once per
                    // batch and cloned per message below.
                    let extraction_cfg = GraphExtractionConfig {
                        max_entities: graph_cfg.max_entities_per_message,
                        max_edges: graph_cfg.max_edges_per_message,
                        extraction_timeout_secs: graph_cfg.extraction_timeout_secs,
                        community_refresh_interval: 0,
                        expired_edge_retention_days: graph_cfg.expired_edge_retention_days,
                        max_entities_cap: graph_cfg.max_entities,
                        community_summary_max_prompt_bytes: graph_cfg
                            .community_summary_max_prompt_bytes,
                        community_summary_concurrency: graph_cfg.community_summary_concurrency,
                        lpa_edge_chunk_size: graph_cfg.lpa_edge_chunk_size,
                        note_linking: zeph_memory::NoteLinkingConfig::default(),
                        link_weight_decay_lambda: graph_cfg.link_weight_decay_lambda,
                        link_weight_decay_interval_secs: graph_cfg.link_weight_decay_interval_secs,
                        belief_revision_enabled: graph_cfg.belief_revision.enabled,
                        belief_revision_similarity_threshold: graph_cfg
                            .belief_revision
                            .similarity_threshold,
                        conversation_id: None,
                        apex_mem_enabled: graph_cfg.apex_mem.enabled,
                        llm_timeout_secs: graph_cfg.llm_timeout_secs,
                        embed_timeout_secs,
                        turn_index: None,
                        write_gate_min_relevance: graph_cfg
                            .write_gate
                            .enabled
                            .then_some(graph_cfg.write_gate.min_edge_relevance),
                        benna_fast_rate: graph_cfg.spreading_activation.benna_fast_rate,
                        benna_slow_rate: graph_cfg.spreading_activation.benna_slow_rate,
                        provenance: None,
                        system_prompt: None,
                        recall_include_imported: graph_cfg.recall_include_imported,
                    };

                    // Extract concurrently, bounded to 4 in-flight — matches
                    // semantic_scan_plugin_add's existing batched-LLM-call bound. Safe because
                    // `extract_and_store` builds a fresh `EntityResolver` per call (its
                    // `lock_name` guard does not span calls), so the actual concurrency-safety
                    // mechanism is the DB-level `UNIQUE(canonical_name, entity_type)` constraint
                    // and `ON CONFLICT ... DO UPDATE ... RETURNING id` upsert in
                    // `GraphStore::upsert_entity` (plus `add_alias`'s `INSERT OR IGNORE`), which
                    // makes concurrent entity creation for the same name idempotent regardless of
                    // in-process locking.
                    {
                        use futures::stream::StreamExt as _;

                        let extraction_futs: Vec<_> = messages
                            .iter()
                            .filter_map(|(_id, content)| {
                                if content.trim().is_empty() {
                                    return None;
                                }
                                let content = content.clone();
                                let provider = provider.clone();
                                let pool = store.pool().clone();
                                let extraction_cfg = extraction_cfg.clone();
                                Some(extract_and_store(
                                    content,
                                    vec![],
                                    provider,
                                    pool,
                                    extraction_cfg,
                                    None,
                                    None,
                                ))
                            })
                            .collect();

                        let results: Vec<_> = futures::stream::iter(extraction_futs)
                            .buffer_unordered(4)
                            .collect()
                            .await;

                        for result in results {
                            match result {
                                Ok(result) => {
                                    total_entities += result.stats.entities_upserted;
                                    total_edges += result.stats.edges_inserted;
                                }
                                Err(e) => {
                                    tracing::warn!("backfill extraction error: {e:#}");
                                }
                            }
                        }
                    }

                    store
                        .mark_messages_graph_processed(&ids)
                        .await
                        .map_err(|e| CommandError::new(e.to_string()))?;
                    processed += messages.len();

                    progress_cb(format!(
                        "Backfill progress: {processed} messages processed, \
                     {total_entities} entities, {total_edges} edges"
                    ));
                }

                Ok(format!(
                    "Backfill complete: {total_entities} entities, {total_edges} edges \
                 extracted from {processed} messages"
                ))
            }
            .instrument(tracing::info_span!("core.agent_access.graph_backfill")),
        )
    }

    // ----- /knowledge -----

    fn knowledge_status<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                use zeph_memory::graph::ingest::IngestLedger;

                let Some(memory) = self.services.memory.persistence.memory.clone() else {
                    return Ok("Memory subsystem not available.".to_owned());
                };
                let pool = memory.sqlite().pool().clone();
                let ledger = IngestLedger::new(pool);

                let rows = match ledger.summary().await {
                    Ok(r) => r,
                    Err(e) => return Err(CommandError(e.to_string())),
                };

                if rows.is_empty() {
                    return Ok("No knowledge has been ingested yet. \
                         Run `zeph knowledge ingest --source <src>`."
                        .to_owned());
                }

                let mut out = format!("Knowledge ingest ledger ({} entries):\n\n", rows.len());
                let mut current_batch = String::new();
                for row in &rows {
                    let batch_short = &row.import_batch_id[..row.import_batch_id.len().min(8)];
                    if current_batch != row.import_batch_id {
                        if !current_batch.is_empty() {
                            out.push('\n');
                        }
                        current_batch.clone_from(&row.import_batch_id);
                    }
                    let uri_display = &row.source_uri[..row.source_uri.floor_char_boundary(40)];
                    let at_display = &row.ingested_at[..row.ingested_at.len().min(19)];
                    let _ = writeln!(
                        out,
                        "  {uri_display:<40} batch={batch_short} at={at_display} \
                         e={} edges={}",
                        row.entities, row.edges,
                    );
                }
                Ok(out.trim_end().to_owned())
            }
            .instrument(tracing::info_span!("core.agent_access.knowledge_status")),
        )
    }

    fn knowledge_rollback<'a>(
        &'a mut self,
        batch_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, CommandError>> + Send + 'a>> {
        Box::pin(
            async move {
                use zeph_memory::graph::ingest::IngestLedger;

                let Some(memory) = self.services.memory.persistence.memory.clone() else {
                    return Ok("Memory subsystem not available.".to_owned());
                };
                let pool = memory.sqlite().pool().clone();
                let ledger = IngestLedger::new(pool.clone());

                match ledger.batch_exists(batch_id).await {
                    Ok(false) => {
                        return Ok(format!("Batch '{batch_id}' not found in ledger."));
                    }
                    Err(e) => return Err(CommandError(e.to_string())),
                    Ok(true) => {}
                }

                let Some(graph_store) = memory.graph_store.clone() else {
                    return Ok(
                        "Graph store unavailable (Qdrant unreachable or graph not enabled)."
                            .to_owned(),
                    );
                };

                let mut tx = zeph_db::begin_write(&pool)
                    .await
                    .map_err(|e| CommandError(e.to_string()))?;

                let (edges, entities) = graph_store
                    .delete_batch_in_tx(batch_id, &mut tx)
                    .await
                    .map_err(|e| CommandError(e.to_string()))?;
                ledger
                    .delete_batch_in_tx(batch_id, &mut tx)
                    .await
                    .map_err(|e| CommandError(e.to_string()))?;

                tx.commit().await.map_err(|e| CommandError(e.to_string()))?;

                let mut msg = format!(
                    "Rolled back batch '{batch_id}': removed {edges} edge(s) and \
                     {entities} entity(ies)."
                );
                if edges == 0 && entities == 0 {
                    msg.push_str(
                        "\nNote: no graph rows found. Phase-1 ingest writes to Qdrant notes — \
                         Qdrant embeddings are NOT removed by this rollback.",
                    );
                }
                Ok(msg)
            }
            .instrument(tracing::info_span!("core.agent_access.knowledge_rollback")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;

    async fn memory_without_qdrant() -> SemanticMemory {
        SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap()
    }

    // R-CRIT-4111: when graph is enabled in config but graph_store is None
    // (Qdrant unreachable), graph command handlers must report
    // "unavailable" rather than "not enabled".
    #[tokio::test]
    async fn graph_stats_enabled_but_no_store_reports_unavailable() {
        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let memory = memory_without_qdrant().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_stats().await.unwrap();
        assert!(
            result.contains("unavailable"),
            "expected 'unavailable' but got: {result}"
        );
        assert!(
            !result.contains("not enabled"),
            "must not report 'not enabled' when graph is enabled: {result}"
        );
    }

    #[tokio::test]
    async fn graph_stats_disabled_reports_not_enabled() {
        let cfg = crate::config::GraphConfig {
            enabled: false,
            ..Default::default()
        };
        let memory = memory_without_qdrant().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_stats().await.unwrap();
        assert!(
            result.contains("not enabled"),
            "expected 'not enabled' but got: {result}"
        );
    }

    // R-CRIT-4136: graph_backfill must resolve extract_provider before entering the async block.
    // When extract_provider is set to an unknown name, resolve_background_provider falls back to
    // the primary provider — the backfill still completes (no messages to process).
    // This test confirms that the provider-resolution code path executes without panic or borrow
    // errors, which would occur if the old code tried to access `&mut self` inside `async move`.
    #[tokio::test]
    async fn graph_backfill_with_extract_provider_resolves_without_panic() {
        let cfg = crate::config::GraphConfig {
            enabled: true,
            extract_provider: zeph_config::providers::ProviderName::new("nonexistent-provider"),
            ..Default::default()
        };
        let mut memory = memory_without_qdrant().await;
        // Install a real SQLite-backed GraphStore so resolve_graph_store succeeds.
        let pool = memory.sqlite().pool().clone();
        memory.graph_store = Some(std::sync::Arc::new(zeph_memory::GraphStore::new(pool)));
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let mut progress = vec![];
        let result = agent
            .graph_backfill(Some(10), &mut |msg| progress.push(msg))
            .await
            .unwrap();

        // With an empty store there are zero unprocessed messages → backfill completes immediately.
        assert!(
            result.contains("Backfill complete"),
            "expected 'Backfill complete' but got: {result}"
        );
    }

    // #6261: graph_backfill extracts each batch's unprocessed messages concurrently via
    // `futures::stream::iter(...).buffer_unordered(4)` instead of a sequential per-message
    // loop. buffer_unordered completes futures in an order that need not match input order, so
    // this test asserts on aggregate totals (immune to completion order) and on per-entity /
    // per-message presence, proving the concurrent rewrite neither drops nor double-counts
    // results relative to the pre-#6261 sequential behavior.
    #[tokio::test]
    async fn graph_backfill_concurrent_extraction_aggregates_stats_without_dropping_results() {
        let n = 6;
        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let mut memory = memory_without_qdrant().await;
        let store = install_graph_store(&mut memory);
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..n {
            sqlx::query(zeph_db::sql!(
                "INSERT INTO messages (conversation_id, role, content) VALUES (?1, 'user', ?2)"
            ))
            .bind(cid.0)
            .bind(format!("message body {i}"))
            .execute(memory.sqlite().pool())
            .await
            .unwrap();
        }

        // One canned extraction response per message, each yielding exactly one distinct
        // entity. MockProvider serves responses in call order (not message order), which
        // mirrors buffer_unordered's out-of-order completion.
        let responses: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    r#"{{"entities":[{{"name":"Entity{i}","type":"concept","summary":""}}],"edges":[]}}"#
                )
            })
            .collect();

        let mut agent = Agent::new(
            mock_provider(responses),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let mut progress = vec![];
        let result = agent
            .graph_backfill(None, &mut |msg| progress.push(msg))
            .await
            .unwrap();

        assert!(
            result.contains(&format!("{n} entities")),
            "expected all {n} entities aggregated in the result, got: {result}"
        );
        assert!(
            result.contains(&format!("from {n} messages")),
            "expected all {n} messages counted as processed, got: {result}"
        );

        // No drops/double-counts at the store level: every entity must be present exactly once.
        for i in 0..n {
            let name = format!("entity{i}");
            let found = store
                .find_entity(&name, zeph_memory::EntityType::Concept)
                .await
                .unwrap();
            assert!(found.is_some(), "entity{i} must have been upserted");
        }

        // Every message in the batch must be marked processed — none left behind by a
        // buffer_unordered future that was dropped or never polled to completion.
        let remaining = store.unprocessed_message_count().await.unwrap();
        assert_eq!(remaining, 0, "all messages must be marked graph_processed");
    }

    // #6261 follow-up (impl-critic finding): the aggregation test above uses an in-memory
    // SQLite database, which `zeph-db`'s pool forces to a single connection
    // (`connect_sqlite`'s `effective_max = if path == ":memory:" { 1 }`, see
    // `crates/zeph-db/src/pool.rs`) — so it never actually exercises concurrent writers racing
    // for the SQLite write lock. This test uses a real file-backed database instead (default
    // pool_size = 5, WAL journal mode + 5s busy_timeout — see `DbConfig::connect_sqlite`) with
    // more unprocessed messages than the `buffer_unordered(4)` bound, so multiple pooled
    // connections genuinely contend for writes concurrently. It confirms `extract_and_store`'s
    // upserts — relying on WAL mode + busy_timeout + `EntityResolver`'s per-entity-name locking,
    // the same assumption `semantic_scan_plugin_add`'s existing `buffer_unordered(4)` usage
    // relies on — complete without a "database is locked" error under real multi-connection
    // write contention.
    #[tokio::test]
    async fn graph_backfill_concurrent_extraction_survives_real_sqlite_write_contention() {
        let n = 8;
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_str().expect("valid utf-8 path").to_owned();

        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let mut memory = SemanticMemory::new(
            &path,
            "http://127.0.0.1:1",
            None,
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap();
        let store = install_graph_store(&mut memory);
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..n {
            sqlx::query(zeph_db::sql!(
                "INSERT INTO messages (conversation_id, role, content) VALUES (?1, 'user', ?2)"
            ))
            .bind(cid.0)
            .bind(format!("contention message body {i}"))
            .execute(memory.sqlite().pool())
            .await
            .unwrap();
        }

        // A small per-call delay forces the (up to 4) concurrently in-flight extraction futures
        // to genuinely overlap their subsequent SQLite writes, rather than happening to resolve
        // one at a time fast enough to never actually race.
        let responses: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    r#"{{"entities":[{{"name":"ContentionEntity{i}","type":"concept","summary":""}}],"edges":[]}}"#
                )
            })
            .collect();
        let mut provider = zeph_llm::mock::MockProvider::with_responses(responses);
        provider.delay_ms = 15;
        let provider = zeph_llm::any::AnyProvider::Mock(provider);

        let mut agent = Agent::new(
            provider,
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let mut progress = vec![];
        let result = agent
            .graph_backfill(None, &mut |msg| progress.push(msg))
            .await
            .unwrap();

        assert!(
            result.contains(&format!("{n} entities")),
            "expected all {n} entities aggregated despite concurrent SQLite writers, got: {result}"
        );

        // The decisive assertion: if a concurrent writer had hit "database is locked"
        // (SQLITE_BUSY surfacing as an error instead of the busy_timeout retry succeeding),
        // extract_and_store logs a warning and skips that message's upsert (the `Err(e) =>
        // tracing::warn!(...)` arm in graph_backfill) rather than failing the whole batch — so a
        // missing entity here is the observable symptom of exactly the failure mode flagged.
        for i in 0..n {
            let name = format!("contentionentity{i}");
            let found = store
                .find_entity(&name, zeph_memory::EntityType::Concept)
                .await
                .unwrap();
            assert!(
                found.is_some(),
                "entity {i} must have been upserted; a missing entity indicates a dropped/failed \
                 concurrent write (e.g. a 'database is locked' error) under real multi-connection \
                 contention"
            );
        }

        let remaining = store.unprocessed_message_count().await.unwrap();
        assert_eq!(remaining, 0, "all messages must be marked graph_processed");
    }

    // R-4139: graph_entities with enabled graph but no store (Qdrant unreachable) must
    // report unavailable, not panic or hang.
    #[tokio::test]
    async fn graph_entities_enabled_but_no_store_reports_unavailable() {
        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let memory = memory_without_qdrant().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_entities().await.unwrap();
        assert!(
            result.contains("unavailable"),
            "expected 'unavailable' but got: {result}"
        );
    }

    // R-4139: graph_communities with enabled graph but no store must report unavailable.
    #[tokio::test]
    async fn graph_communities_enabled_but_no_store_reports_unavailable() {
        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let memory = memory_without_qdrant().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_communities().await.unwrap();
        assert!(
            result.contains("unavailable"),
            "expected 'unavailable' but got: {result}"
        );
    }

    // R-4139: verify that the tokio::time::timeout pattern used in graph handlers
    // correctly returns Err on a never-resolving future. This is a direct regression
    // guard for the fix introduced in #4139: before the fix, these calls had no
    // timeout guard and would block indefinitely when Qdrant was unreachable.
    #[tokio::test]
    async fn graph_store_timeout_pattern_fires_on_pending_future() {
        use std::future;
        let result = tokio::time::timeout(
            Duration::from_millis(10),
            future::pending::<Result<Vec<()>, String>>(),
        )
        .await;
        assert!(
            result.is_err(),
            "timeout must fire on a never-resolving future"
        );
    }

    // ── #5770: with_graph_store_timeout had zero coverage of its own timeout branch —
    // existing tests only reached the "no store configured" short-circuit in
    // resolve_graph_store(), never the 5s deadline shared by resolve_entity_by_name,
    // graph_facts, and graph_history. Exercise the extracted helper directly with a
    // paused clock so the deadline fires deterministically without a real wall-clock wait.

    #[tokio::test]
    async fn with_graph_store_timeout_completes_on_success() {
        let result = with_graph_store_timeout(async { Ok::<_, zeph_memory::MemoryError>(42) })
            .await
            .unwrap();
        assert!(matches!(result, StoreCallOutcome::Completed(42)));
    }

    #[tokio::test]
    async fn with_graph_store_timeout_maps_store_error_to_command_error() {
        let result = with_graph_store_timeout(async {
            Err::<i32, _>(zeph_memory::MemoryError::GraphStore("boom".to_owned()))
        })
        .await;
        assert!(result.is_err(), "store error must surface as CommandError");
    }

    #[tokio::test]
    async fn with_graph_store_timeout_times_out_on_pending_future() {
        tokio::time::pause();
        let fut = with_graph_store_timeout(std::future::pending::<
            Result<i32, zeph_memory::MemoryError>,
        >());
        let handle = tokio::spawn(fut); // EXEMPT: test-only tokio::time::pause harness
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        let result = handle.await.expect("task panicked");
        assert!(
            matches!(result, Ok(StoreCallOutcome::TimedOut)),
            "call must resolve to TimedOut once the 5s deadline elapses"
        );
    }

    // ── #5764: graph_facts / graph_history had zero dedicated test coverage ──────

    /// Installs a real SQLite-backed `GraphStore` on `memory` (mirrors
    /// `graph_backfill_with_extract_provider_resolves_without_panic`), returning an `Arc`
    /// clone so callers can seed entities/edges before handing `memory` to `with_memory`.
    fn install_graph_store(memory: &mut SemanticMemory) -> std::sync::Arc<zeph_memory::GraphStore> {
        let pool = memory.sqlite().pool().clone();
        let store = std::sync::Arc::new(zeph_memory::GraphStore::new(pool));
        memory.graph_store = Some(store.clone());
        store
    }

    #[tokio::test]
    async fn graph_facts_happy_path_returns_formatted_facts() {
        let mut memory = memory_without_qdrant().await;
        let store = install_graph_store(&mut memory);
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let alice = store
            .upsert_entity(
                "Alice",
                "alice",
                zeph_memory::EntityType::Person,
                None,
                None,
            )
            .await
            .unwrap();
        let bob = store
            .upsert_entity("Bob", "bob", zeph_memory::EntityType::Person, None, None)
            .await
            .unwrap();
        store
            .insert_edge(alice.0, bob.0, "knows", "Alice knows Bob", 0.9, None, None)
            .await
            .unwrap();

        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_facts("Alice").await.unwrap();
        assert!(
            result.contains("Facts for 'Alice'"),
            "expected facts header, got: {result}"
        );
        assert!(
            result.contains("Bob"),
            "expected target entity name, got: {result}"
        );
        assert!(result.contains("knows"), "expected relation, got: {result}");
        assert!(
            result.contains("Alice knows Bob"),
            "expected fact text, got: {result}"
        );
    }

    #[tokio::test]
    async fn graph_facts_entity_not_found_returns_message() {
        let mut memory = memory_without_qdrant().await;
        install_graph_store(&mut memory);
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_facts("Nobody").await.unwrap();
        assert_eq!(result, "No entity found matching 'Nobody'.");
    }

    // Mirrors graph_entities_enabled_but_no_store_reports_unavailable (R-4139): when the graph
    // store is None (Qdrant unreachable) but graph is enabled, report unavailable rather than
    // hang or panic.
    #[tokio::test]
    async fn graph_facts_enabled_but_no_store_reports_unavailable() {
        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let memory = memory_without_qdrant().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_facts("Alice").await.unwrap();
        assert!(
            result.contains("unavailable"),
            "expected 'unavailable' but got: {result}"
        );
    }

    // Self-loop edges (source == target) are rejected both by `GraphStore::insert_edge_typed`
    // and by a DB-level trigger (migration 044_graph_edges_no_self_loops) — so a real one can
    // only arise from data written before that migration. Drop the trigger to simulate that
    // legacy row and confirm graph_facts' defensive `entity_names` bookkeeping (which already
    // knows the entity's own name before resolving edge endpoints) handles it without panicking
    // or falling back to a raw `#id` placeholder.
    #[tokio::test]
    async fn graph_facts_self_loop_edge_does_not_panic() {
        let mut memory = memory_without_qdrant().await;
        let store = install_graph_store(&mut memory);
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let self_entity = store
            .upsert_entity("Self", "self", zeph_memory::EntityType::Concept, None, None)
            .await
            .unwrap();
        let pool = memory.sqlite().pool().clone();
        zeph_db::query(zeph_db::sql!(
            "DROP TRIGGER IF EXISTS graph_edges_no_self_loops"
        ))
        .execute(&pool)
        .await
        .unwrap();
        zeph_db::query(zeph_db::sql!(
            "INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence) \
             VALUES (?, ?, ?, ?, ?)"
        ))
        .bind(self_entity.0)
        .bind(self_entity.0)
        .bind("refers_to")
        .bind("Self refers to itself")
        .bind(1.0_f64)
        .execute(&pool)
        .await
        .unwrap();

        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_facts("Self").await.unwrap();
        assert!(
            result.contains("Facts for 'Self'"),
            "expected facts header, got: {result}"
        );
        assert!(
            result.contains("refers_to"),
            "expected self-loop relation, got: {result}"
        );
        assert!(
            !result.contains('#'),
            "self-loop endpoint must resolve to the entity's own name, not a raw #id \
             placeholder: {result}"
        );
    }

    #[tokio::test]
    async fn graph_history_happy_path_returns_formatted_history() {
        let mut memory = memory_without_qdrant().await;
        let store = install_graph_store(&mut memory);
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let alice = store
            .upsert_entity(
                "Alice",
                "alice",
                zeph_memory::EntityType::Person,
                None,
                None,
            )
            .await
            .unwrap();
        let bob = store
            .upsert_entity("Bob", "bob", zeph_memory::EntityType::Person, None, None)
            .await
            .unwrap();
        store
            .insert_edge(alice.0, bob.0, "knows", "Alice knows Bob", 0.9, None, None)
            .await
            .unwrap();

        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_history("Alice").await.unwrap();
        assert!(
            result.contains("Edge history for 'Alice'"),
            "expected history header, got: {result}"
        );
        assert!(
            result.contains("[active]"),
            "expected active tag, got: {result}"
        );
        assert!(
            result.contains("Bob"),
            "expected target entity name, got: {result}"
        );
    }

    #[tokio::test]
    async fn graph_history_entity_not_found_returns_message() {
        let mut memory = memory_without_qdrant().await;
        install_graph_store(&mut memory);
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_history("Nobody").await.unwrap();
        assert_eq!(result, "No entity found matching 'Nobody'.");
    }

    #[tokio::test]
    async fn graph_history_enabled_but_no_store_reports_unavailable() {
        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let memory = memory_without_qdrant().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_history("Alice").await.unwrap();
        assert!(
            result.contains("unavailable"),
            "expected 'unavailable' but got: {result}"
        );
    }

    // See graph_facts_self_loop_edge_does_not_panic for why the DB trigger must be dropped.
    #[tokio::test]
    async fn graph_history_self_loop_edge_does_not_panic() {
        let mut memory = memory_without_qdrant().await;
        let store = install_graph_store(&mut memory);
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let self_entity = store
            .upsert_entity("Self", "self", zeph_memory::EntityType::Concept, None, None)
            .await
            .unwrap();
        let pool = memory.sqlite().pool().clone();
        zeph_db::query(zeph_db::sql!(
            "DROP TRIGGER IF EXISTS graph_edges_no_self_loops"
        ))
        .execute(&pool)
        .await
        .unwrap();
        zeph_db::query(zeph_db::sql!(
            "INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence) \
             VALUES (?, ?, ?, ?, ?)"
        ))
        .bind(self_entity.0)
        .bind(self_entity.0)
        .bind("refers_to")
        .bind("Self refers to itself")
        .bind(1.0_f64)
        .execute(&pool)
        .await
        .unwrap();

        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 100)
        .with_graph_config(cfg);

        let result = agent.graph_history("Self").await.unwrap();
        assert!(
            result.contains("Edge history for 'Self'"),
            "expected history header, got: {result}"
        );
        assert!(
            result.contains("refers_to"),
            "expected self-loop relation, got: {result}"
        );
        assert!(
            !result.contains('#'),
            "self-loop endpoint must resolve to the entity's own name, not a raw #id \
             placeholder: {result}"
        );
    }
}
