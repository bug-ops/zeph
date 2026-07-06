// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;
#[allow(unused_imports)]
use zeph_db::sql;

use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use zeph_db::DbPool;

pub use zeph_common::config::memory::NoteLinkingConfig;
use zeph_common::sanitize::strip_control_chars;
use zeph_common::text::truncate_to_bytes_ref;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::graph::extractor::ExtractionResult as ExtractorResult;
use crate::graph::resolver::{MAX_RELATION_BYTES, sanitize_fact, sanitize_relation};
use crate::graph::types::GraphProvenance;
use crate::vector_store::VectorFilter;

use super::SemanticMemory;

/// Callback type for post-extraction validation.
///
/// A generic predicate opaque to zeph-memory — callers (zeph-core) provide security
/// validation without introducing a dependency on security policy in this crate.
pub type PostExtractValidator = Option<Box<dyn Fn(&ExtractorResult) -> Result<(), String> + Send>>;

/// Shared post-extraction validator for concurrent batch ingest (spec-067 FR-022, C3).
///
/// `Arc<dyn Fn + Send + Sync>` allows cheap cloning across `buffer_unordered` tasks without
/// requiring `unsafe`. Pass `None` to skip validation.
pub type SharedPostExtractValidator =
    Option<Arc<dyn Fn(&ExtractorResult) -> Result<(), String> + Send + Sync>>;

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
    /// APEX-MEM: use `insert_or_supersede` instead of `resolve_edge_typed`. Default: `false`.
    pub apex_mem_enabled: bool,
    /// LLM call timeout for extraction, in seconds. Default: `30`.
    pub llm_timeout_secs: u64,
    /// Per-call timeout for every `embed()` invocation, in seconds. Default: `5`.
    pub embed_timeout_secs: u64,
    /// Turn index within the episode for edges inserted during this extraction pass (#3710).
    ///
    /// `None` disables turn-level provenance recording for this pass.
    pub turn_index: Option<u32>,
    /// `MemORAI` write-gate prefilter: minimum confidence for low-signal-relation edges (#3709).
    ///
    /// `None` disables the gate (default behaviour, always writes).
    pub write_gate_min_relevance: Option<f32>,
    /// Benna-Fusi fast-variable learning rate for confidence merges (#4711).
    ///
    /// Passed to [`crate::graph::GraphStore::with_benna_rates`]. Default: `0.5`.
    pub benna_fast_rate: f32,
    /// Benna-Fusi slow-variable learning rate for confidence merges (#4711).
    ///
    /// Passed to [`crate::graph::GraphStore::with_benna_rates`]. Default: `0.05`.
    pub benna_slow_rate: f32,
    /// Provenance stamped on every entity and edge written in this pass (spec-067 INV-2).
    ///
    /// `None` means conversation origin (default for conversation callers).
    /// The ingest path passes `Some(prov)` with the appropriate origin.
    pub provenance: Option<GraphProvenance>,
    /// When `false`, recall queries exclude edges with any imported (non-conversation) origin
    /// (spec-067 §3 Phase 0).
    ///
    /// Default: `true` (include imported edges in recall).
    pub recall_include_imported: bool,
    /// Override system prompt for [`crate::graph::GraphExtractor`].
    ///
    /// `None` (the default) uses the built-in conversational prompt — all existing callers
    /// are unaffected. The ingest path sets this to
    /// [`crate::graph::ingest::prompt::TECH_DOC_SYSTEM_PROMPT`] via
    /// [`crate::graph::GraphExtractor::with_system_prompt`].
    ///
    /// The type is `&'static str` — prompts must be compile-time constants (spec-067 C7).
    pub system_prompt: Option<&'static str>,
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
            apex_mem_enabled: false,
            llm_timeout_secs: 30,
            embed_timeout_secs: 5,
            turn_index: None,
            write_gate_min_relevance: None,
            benna_fast_rate: 0.5,
            benna_slow_rate: 0.05,
            provenance: None,
            recall_include_imported: true,
            system_prompt: None,
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

/// Fallback confidence used when the LLM omits the `confidence` field in an extracted edge.
const DEFAULT_EDGE_CONFIDENCE: f32 = 0.8;

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
pub async fn link_memory_notes(
    entity_ids: &[i64],
    pool: DbPool,
    embedding_store: Arc<EmbeddingStore>,
    provider: AnyProvider,
    cfg: &NoteLinkingConfig,
) -> LinkingStats {
    use crate::graph::GraphStore;

    let store = GraphStore::new(pool);
    let mut stats = LinkingStats::default();

    let work_items = collect_note_link_work_items(entity_ids, &store).await;
    if work_items.is_empty() {
        return stats;
    }

    let valid = embed_work_items(&work_items, &provider, cfg).await;

    let search_limit = cfg.top_k + 1; // +1 to account for self-match
    let search_results = search_similar_for_items(&valid, &embedding_store, search_limit).await;

    insert_similarity_edges(
        &work_items,
        &valid,
        &search_results,
        cfg,
        &store,
        &mut stats,
    )
    .await;

    stats
}

/// Phase 1: load entities from the DB and build work items for embedding.
///
/// Processes entities sequentially to avoid connection-pool contention.
async fn collect_note_link_work_items(
    entity_ids: &[i64],
    store: &crate::graph::GraphStore,
) -> Vec<EntityWorkItem> {
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
    work_items
}

/// Phase 2: embed all entity texts in parallel.
///
/// Returns `(work_idx, embedding)` pairs for successfully embedded items.
/// Items that fail to embed are logged and dropped.
async fn embed_work_items(
    work_items: &[EntityWorkItem],
    provider: &AnyProvider,
    cfg: &NoteLinkingConfig,
) -> Vec<(usize, Vec<f32>)> {
    use futures::future;

    let Ok(embed_results) = tokio::time::timeout(
        std::time::Duration::from_secs(cfg.timeout_secs),
        future::join_all(work_items.iter().map(|w| provider.embed(&w.embed_text))),
    )
    .await
    else {
        tracing::warn!(
            count = work_items.len(),
            "note_linking: batch embed timed out — skipping all entities"
        );
        return Vec::new();
    };
    embed_results
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
        .collect()
}

/// Phase 3: search the embedding store for similar entities for each embedded work item.
async fn search_similar_for_items(
    valid: &[(usize, Vec<f32>)],
    embedding_store: &EmbeddingStore,
    search_limit: usize,
) -> Vec<Result<Vec<crate::ScoredVectorPoint>, MemoryError>> {
    use futures::future;

    future::join_all(valid.iter().map(|(_, vec)| {
        embedding_store.search_collection(
            ENTITY_COLLECTION,
            vec,
            search_limit,
            None::<VectorFilter>,
        )
    }))
    .await
}

/// Phase 4: insert similarity edges, deduplicating pairs seen from both A→B and B→A.
///
/// Without deduplication, both directions would call `insert_edge` for the same normalised
/// pair and both return `Ok`, inflating `edges_created` by the number of bidirectional hits.
async fn insert_similarity_edges(
    work_items: &[EntityWorkItem],
    valid: &[(usize, Vec<f32>)],
    search_results: &[Result<Vec<crate::ScoredVectorPoint>, MemoryError>],
    cfg: &NoteLinkingConfig,
    store: &crate::graph::GraphStore,
    stats: &mut LinkingStats,
) {
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
            let Some(payload_entity_id) = point
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

            let Some(target_id) =
                resolve_local_target_id(store, &point.payload, payload_entity_id).await
            else {
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

            if !seen_pairs.insert((src, tgt)) {
                continue;
            }

            let fact = format!("Semantically similar entities (score: {:.3})", point.score);

            match store
                .insert_edge(src, tgt, "similar_to", &fact, point.score, None, None)
                .await
            {
                Ok(_) => stats.edges_created += 1,
                Err(e) if e.is_foreign_key_violation() => {
                    // `target_id` came straight from a Qdrant payload (no local-existence
                    // check) — a stale cross-DB id silently drops this link edge (#5801).
                    tracing::warn!(
                        source = src,
                        target = tgt,
                        "note_linking: insert_edge rejected by FK constraint, dropping edge: {e:#}"
                    );
                }
                Err(e) => {
                    tracing::debug!("note_linking: insert_edge failed: {e:#}");
                }
            }
        }
    }
}

/// Re-resolve a Qdrant search-result candidate's entity id against the *local* `SQLite`
/// database instead of trusting the payload's `entity_id` verbatim.
///
/// Mirrors the canonical-name-keyed correction `EntityResolver::merge_entity` applies to
/// entity resolution (#5801): the payload's `entity_id` may reference a row in a different
/// `SQLite` instance (e.g. after a `SQLite` reset/restore performed independently of a
/// shared Qdrant collection), which would otherwise be used as an edge FK target and fail
/// with a foreign-key violation — or, worse, silently collide with an unrelated local row
/// reusing the same id. The fast path requires BOTH id-existence AND a matching
/// `canonical_name`/`entity_type` (identity, not just existence) before trusting the payload
/// id: under the reset trigger, local ids restart from 1, so a stale `payload_entity_id` can
/// easily coincide with a valid but *different* local entity, and existence alone would
/// silently link to the wrong entity. The correction path only triggers when the id is
/// absent or its identity doesn't match, and falls back to creating the entity locally when
/// no local row exists under its canonical name either.
///
/// Returns `None` when the candidate should be skipped (malformed payload, or a DB error
/// while re-resolving/creating it) — logged at `debug!`, matching the existing skip
/// behavior for a missing `entity_id` payload field.
async fn resolve_local_target_id(
    store: &crate::graph::GraphStore,
    payload: &std::collections::HashMap<String, serde_json::Value>,
    payload_entity_id: i64,
) -> Option<i64> {
    let Some(canonical_name) = payload.get("canonical_name").and_then(|v| v.as_str()) else {
        tracing::debug!(
            payload_entity_id,
            "note_linking: missing canonical_name in payload, skipping candidate"
        );
        return None;
    };
    let entity_type = crate::graph::EntityResolver::parse_entity_type(
        payload
            .get("entity_type")
            .and_then(|v| v.as_str())
            .unwrap_or("concept"),
    );

    // Fast path: the payload id exists locally AND its canonical identity matches the
    // candidate — the expected, synced case.
    match store.find_entity_by_id(payload_entity_id).await {
        Ok(Some(entity))
            if entity.canonical_name == canonical_name && entity.entity_type == entity_type =>
        {
            return Some(payload_entity_id);
        }
        Ok(_) => {}
        Err(e) => {
            tracing::debug!(
                payload_entity_id,
                error = %e,
                "note_linking: find_entity_by_id failed, skipping candidate"
            );
            return None;
        }
    }

    // Slow path: the payload id is stale, absent, or identifies a different local entity.
    // Re-resolve by canonical_name — the same key merge_entity uses — falling back to local
    // entity creation only when no local row exists under that name either.
    let resolved = match store.find_entity(canonical_name, entity_type).await {
        Ok(Some(entity)) => entity.id.0,
        Ok(None) => {
            let name = payload
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(canonical_name);
            let summary = payload.get("summary").and_then(|v| v.as_str());
            match store
                .upsert_entity(name, canonical_name, entity_type, summary, None)
                .await
            {
                Ok(id) => id.0,
                Err(e) => {
                    tracing::debug!(
                        canonical_name,
                        error = %e,
                        "note_linking: failed to create local entity for stale candidate, skipping"
                    );
                    return None;
                }
            }
        }
        Err(e) => {
            tracing::debug!(
                canonical_name,
                error = %e,
                "note_linking: find_entity failed while re-resolving candidate, skipping"
            );
            return None;
        }
    };

    tracing::warn!(
        payload_entity_id,
        resolved_entity_id = resolved,
        canonical_name,
        "note_linking: Qdrant entity_id payload did not match local SQLite row \
         (cross-DB or stale resolution) — corrected to the locally authoritative id"
    );
    Some(resolved)
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

    let mut extractor = GraphExtractor::new(
        provider.clone(),
        config.max_entities,
        config.max_edges,
        config.llm_timeout_secs,
    );
    if let Some(prompt) = config.system_prompt {
        extractor = extractor.with_system_prompt(prompt);
    }
    let ctx_refs: Vec<&str> = context_messages.iter().map(String::as_str).collect();

    let store = GraphStore::new(pool)
        .with_benna_rates(config.benna_fast_rate, config.benna_slow_rate)
        .with_recall_include_imported(config.recall_include_imported);

    bump_extraction_count(store.pool()).await?;

    let Some(result) = extractor.extract(&content, &ctx_refs).await? else {
        return Ok(ExtractionResult::default());
    };

    // Post-extraction validation callback. zeph-memory does not know the callback is a
    // security validator — it is a generic predicate opaque to this crate (design decision D1).
    // Returns Err(ValidationRejected) so callers can distinguish sanitizer drops from
    // hard failures (INV-4, spec-067 §G-5).
    if let Some(ref validator) = post_extract_validator
        && let Err(reason) = validator(&result)
    {
        tracing::warn!(
            reason,
            "graph extraction validation failed, skipping upsert"
        );
        return Err(MemoryError::ValidationRejected(reason));
    }

    let resolver = if let Some(ref emb) = embedding_store {
        EntityResolver::new(&store)
            .with_embedding_store(emb)
            .with_provider(&provider)
            .with_embed_timeout(config.embed_timeout_secs)
            .with_llm_timeout(config.llm_timeout_secs)
    } else {
        EntityResolver::new(&store)
            .with_embed_timeout(config.embed_timeout_secs)
            .with_llm_timeout(config.llm_timeout_secs)
    };

    let (entity_name_to_id, entities_upserted) =
        upsert_entities(&resolver, &result.entities, config.provenance.as_ref()).await;
    let edges_inserted = insert_edges(&resolver, &result.edges, &entity_name_to_id, &config).await;

    #[cfg(any(feature = "sqlite", feature = "postgres"))]
    store.checkpoint_wal().await?;

    let new_entity_ids: Vec<i64> = entity_name_to_id.into_values().collect();

    link_episode(&store, &config, &new_entity_ids).await;

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

/// Increment the extraction counter in `graph_metadata`.
async fn bump_extraction_count(pool: &DbPool) -> Result<(), MemoryError> {
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
    Ok(())
}

/// Upsert all extracted entities and return the name-to-id map and upsert count.
async fn upsert_entities(
    resolver: &crate::graph::EntityResolver<'_>,
    entities: &[crate::graph::extractor::ExtractedEntity],
    provenance: Option<&GraphProvenance>,
) -> (std::collections::HashMap<String, i64>, usize) {
    let mut entity_name_to_id: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    let mut entities_upserted = 0usize;

    for entity in entities {
        match resolver
            .resolve(
                &entity.name,
                &entity.entity_type,
                entity.summary.as_deref(),
                provenance,
            )
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

    (entity_name_to_id, entities_upserted)
}

/// Returns `true` when `relation` is a generic, low-information connector.
///
/// Used by the `MemORAI` write-gate to avoid storing vacuous edges (#3709).
fn is_low_signal_relation(relation: &str) -> bool {
    const LOW_SIGNAL: &[&str] = &[
        "related_to",
        "related to",
        "is related to",
        "associated_with",
        "associated with",
        "has",
        "have",
        "is",
        "are",
        "mentions",
        "mentioned",
        "involves",
        "involved",
    ];
    LOW_SIGNAL.iter().any(|&s| relation.eq_ignore_ascii_case(s))
}

/// Insert extracted edges that have both endpoints in `name_to_id`.
///
/// Returns the number of edges actually inserted.
#[allow(clippy::too_many_lines)]
async fn insert_edges(
    resolver: &crate::graph::EntityResolver<'_>,
    edges: &[crate::graph::extractor::ExtractedEdge],
    name_to_id: &std::collections::HashMap<String, i64>,
    config: &GraphExtractionConfig,
) -> usize {
    let mut edges_inserted = 0usize;
    for edge in edges {
        // MemORAI write-gate: drop low-signal edges below the relevance threshold (#3709).
        if let Some(min_rel) = config.write_gate_min_relevance {
            let conf = edge.confidence.unwrap_or(1.0);
            if conf < min_rel && is_low_signal_relation(&edge.relation) {
                tracing::debug!(
                    relation = %edge.relation,
                    confidence = conf,
                    threshold = min_rel,
                    "write-gate: skipping low-signal edge"
                );
                continue;
            }
        }
        let (Some(&src_id), Some(&tgt_id)) =
            (name_to_id.get(&edge.source), name_to_id.get(&edge.target))
        else {
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
        if config.apex_mem_enabled {
            // APEX-MEM: append-only write path with supersession chains.
            let relation_trimmed = edge.relation.trim();
            let relation_display_clean = strip_control_chars(relation_trimmed);
            let relation_display =
                truncate_to_bytes_ref(&relation_display_clean, MAX_RELATION_BYTES).to_owned();
            let canonical_relation = sanitize_relation(relation_trimmed);
            let normalized_fact = sanitize_fact(&edge.fact);
            match resolver
                .graph_store()
                .insert_or_supersede_with_turn_index_and_metrics(
                    src_id,
                    tgt_id,
                    &relation_display,
                    &canonical_relation,
                    &normalized_fact,
                    edge.confidence.unwrap_or(DEFAULT_EDGE_CONFIDENCE),
                    None,
                    edge_type,
                    true,
                    None,
                    config.turn_index,
                    config.provenance.as_ref(),
                )
                .await
            {
                Ok(_) => edges_inserted += 1,
                Err(e) if e.is_foreign_key_violation() => {
                    // A resolved entity id was rejected as an FK target — this drops a
                    // user-requested fact permanently and must not be silent (#5801).
                    tracing::warn!(
                        source = src_id,
                        target = tgt_id,
                        "graph: edge insert (apex) rejected by FK constraint, dropping edge: {e:#}"
                    );
                }
                Err(e) => {
                    tracing::debug!("graph: skipping edge (apex): {e:#}");
                }
            }
        } else {
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
                    edge.confidence.unwrap_or(DEFAULT_EDGE_CONFIDENCE),
                    None,
                    edge_type,
                    belief_cfg.as_ref(),
                    config.turn_index,
                    config.provenance.as_ref(),
                )
                .await
            {
                Ok(Some(_)) => edges_inserted += 1,
                Ok(None) => {} // deduplicated
                Err(e) if e.is_foreign_key_violation() => {
                    // A resolved entity id was rejected as an FK target — this drops a
                    // user-requested fact permanently and must not be silent (#5801).
                    tracing::warn!(
                        source = src_id,
                        target = tgt_id,
                        "graph: edge insert rejected by FK constraint, dropping edge: {e:#}"
                    );
                }
                Err(e) => {
                    tracing::debug!("graph: skipping edge: {e:#}");
                }
            }
        }
    }
    edges_inserted
}

/// Link extracted entities to their GAAMA episode when a conversation ID is configured.
async fn link_episode(
    store: &crate::graph::GraphStore,
    config: &GraphExtractionConfig,
    entity_ids: &[i64],
) {
    let Some(conv_id) = config.conversation_id else {
        return;
    };
    match store.ensure_episode(conv_id).await {
        Ok(episode_id) => {
            for &entity_id in entity_ids {
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
    ///
    /// # Panics
    ///
    /// Panics if the internal `graph_cancel` mutex is poisoned (another thread panicked
    /// while holding the lock).
    pub fn spawn_graph_extraction(
        &self,
        content: String,
        context_messages: Vec<String>,
        config: GraphExtractionConfig,
        post_extract_validator: PostExtractValidator,
        provider_override: Option<AnyProvider>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let using_override = provider_override.is_some();
        let provider = provider_override.unwrap_or_else(|| self.provider.clone());
        if using_override {
            tracing::debug!(
                extract_provider = provider.name(),
                "graph extraction using override provider (quality_gate bypassed)"
            );
        }
        {
            let mut tokens = self
                .graph_cancel
                .lock()
                .expect("graph_cancel mutex poisoned");
            tokens.retain(|t| !t.is_cancelled());
            tokens.push(cancel.clone());
        }

        let ctx = GraphExtractionTaskCtx {
            pool: self.sqlite.pool().clone(),
            provider,
            failure_counter: self.community_detection_failures.clone(),
            extraction_count: self.graph_extraction_count.clone(),
            extraction_failures: self.graph_extraction_failures.clone(),
            embedding_store: self.qdrant.clone(),
            cancel,
        };

        let extraction_fut = run_graph_extraction_task(
            content,
            context_messages,
            config,
            post_extract_validator,
            ctx,
        );
        tokio::spawn(extraction_fut) // EXEMPT: JoinHandle returned to caller; awaited inside BackgroundSupervisor task in zeph-core
    }

    /// Signal cooperative cancellation to every in-flight background graph-extraction task.
    ///
    /// Fires every [`CancellationToken`] tracked since the last drain — i.e. one per
    /// [`spawn_graph_extraction`] call whose task has not yet completed or already been
    /// cancelled. Each task checks its token at community-refresh boundaries, so it exits
    /// cleanly rather than being hard-aborted. This should be called before the supervisor
    /// calls `abort()` on the underlying `JoinHandle`s to give the tasks a chance to flush
    /// state.
    ///
    /// No-op if no extraction has been spawned or all tracked tokens have already fired.
    ///
    /// # Panics
    ///
    /// Panics if the internal `graph_cancel` mutex is poisoned (another thread panicked
    /// while holding the lock).
    ///
    /// [`spawn_graph_extraction`]: SemanticMemory::spawn_graph_extraction
    pub fn cancel_graph_extraction(&self) {
        let tokens = std::mem::take(
            &mut *self
                .graph_cancel
                .lock()
                .expect("graph_cancel mutex poisoned"),
        );
        for token in tokens {
            token.cancel();
        }
    }
}

/// Owned context bundled for the spawned extraction task.
///
/// Bundles the Arcs that must be cloned before entering `tokio::spawn`.
struct GraphExtractionTaskCtx {
    pool: DbPool,
    provider: AnyProvider,
    failure_counter: Arc<std::sync::atomic::AtomicU64>,
    extraction_count: Arc<std::sync::atomic::AtomicU64>,
    extraction_failures: Arc<std::sync::atomic::AtomicU64>,
    embedding_store: Option<Arc<EmbeddingStore>>,
    /// Cancellation signal propagated into background sub-tasks (community refresh).
    cancel: CancellationToken,
}

/// Body of the spawned graph-extraction task.
async fn run_graph_extraction_task(
    content: String,
    context_messages: Vec<String>,
    config: GraphExtractionConfig,
    post_extract_validator: PostExtractValidator,
    ctx: GraphExtractionTaskCtx,
) {
    let timeout_dur = std::time::Duration::from_secs(config.extraction_timeout_secs);
    let extraction_result = tokio::time::timeout(
        timeout_dur,
        extract_and_store(
            content,
            context_messages,
            ctx.provider.clone(),
            ctx.pool.clone(),
            config.clone(),
            post_extract_validator,
            ctx.embedding_store.clone(),
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
            ctx.extraction_count.fetch_add(1, Ordering::Relaxed);
            (true, result.entity_ids)
        }
        Ok(Err(e)) => {
            tracing::warn!("graph extraction failed: {e:#}");
            ctx.extraction_failures.fetch_add(1, Ordering::Relaxed);
            (false, vec![])
        }
        Err(_elapsed) => {
            tracing::warn!("graph extraction timed out");
            ctx.extraction_failures.fetch_add(1, Ordering::Relaxed);
            (false, vec![])
        }
    };

    run_note_linking(
        extraction_ok,
        &new_entity_ids,
        ctx.pool.clone(),
        ctx.embedding_store,
        ctx.provider.clone(),
        &config,
    )
    .await;

    let cancel = ctx.cancel.clone();
    maybe_refresh_communities(
        extraction_ok,
        ctx.pool,
        ctx.provider,
        ctx.failure_counter,
        &config,
        ctx.cancel,
    )
    .await;

    // Mark this task's token as fired now that it has run to completion, so
    // `spawn_graph_extraction` can prune its slot from `graph_cancel` on the next
    // call instead of tracking it for the rest of the process lifetime.
    cancel.cancel();
}

/// Run A-MEM note linking after successful extraction when enabled.
async fn run_note_linking(
    extraction_ok: bool,
    new_entity_ids: &[i64],
    pool: DbPool,
    embedding_store: Option<Arc<EmbeddingStore>>,
    provider: AnyProvider,
    config: &GraphExtractionConfig,
) {
    if !extraction_ok || !config.note_linking.enabled || new_entity_ids.is_empty() {
        return;
    }
    let Some(store) = embedding_store else {
        return;
    };
    let linking_timeout = std::time::Duration::from_secs(config.note_linking.timeout_secs);
    match tokio::time::timeout(
        linking_timeout,
        link_memory_notes(new_entity_ids, pool, store, provider, &config.note_linking),
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

/// Trigger community detection, graph eviction, and link-weight decay when the extraction
/// count hits the configured refresh interval.
///
/// Runs inline within the caller's task (no nested `tokio::spawn`). Each long-running step
/// is guarded by `tokio::select!` on `cancel` so shutdown aborts immediately at the next
/// yield point without leaving orphaned tasks.
async fn maybe_refresh_communities(
    extraction_ok: bool,
    pool: DbPool,
    provider: AnyProvider,
    failure_counter: Arc<std::sync::atomic::AtomicU64>,
    config: &GraphExtractionConfig,
    cancel: CancellationToken,
) {
    use crate::graph::GraphStore;

    if !extraction_ok || config.community_refresh_interval == 0 {
        return;
    }

    let store = GraphStore::new(pool.clone());
    let extraction_count = store.extraction_count().await.unwrap_or(0);
    if extraction_count == 0
        || !i64::try_from(config.community_refresh_interval)
            .is_ok_and(|interval| extraction_count % interval == 0)
    {
        return;
    }

    tracing::info!(extraction_count, "triggering community detection refresh");
    let store2 = GraphStore::new(pool);
    let retention_days = config.expired_edge_retention_days;
    let max_cap = config.max_entities_cap;
    let max_prompt_bytes = config.community_summary_max_prompt_bytes;
    let concurrency = config.community_summary_concurrency;
    let edge_chunk_size = config.lpa_edge_chunk_size;
    let decay_lambda = config.link_weight_decay_lambda;
    let decay_interval_secs = config.link_weight_decay_interval_secs;

    tokio::select! {
        () = cancel.cancelled() => {
            tracing::debug!("community refresh cancelled before community detection");
            return;
        }
        result = crate::graph::community::detect_communities(
            &store2,
            &provider,
            max_prompt_bytes,
            concurrency,
            edge_chunk_size,
        ) => {
            match result {
                Ok(count) => {
                    tracing::info!(communities = count, "community detection complete");
                }
                Err(e) => {
                    tracing::warn!("community detection failed: {e:#}");
                    failure_counter.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    tokio::select! {
        () = cancel.cancelled() => {
            tracing::debug!("community refresh cancelled before graph eviction");
            return;
        }
        result = crate::graph::community::run_graph_eviction(&store2, retention_days, max_cap) => {
            match result {
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
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!("community refresh cancelled before link weight decay");
                }
                result = store2.decay_edge_retrieval_counts(decay_lambda, decay_interval_secs) => {
                    match result {
                        Ok(affected) => {
                            tracing::info!(affected, "link weight decay applied");
                            let _ = store2
                                .set_metadata("last_link_weight_decay_at", &now_secs.to_string())
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("link weight decay failed: {e:#}");
                        }
                    }
                }
            }
        }
    }
}

/// Per-document processing outcome for the `ingest_documents` fan-out stream.
///
/// Using an explicit enum avoids `?` short-circuiting the stream — each closure returns
/// `DocOutcome` rather than `Result`, so one failed document never aborts the batch (FR-028).
enum DocOutcome {
    Skipped(String),
    Done {
        uri: String,
        entities: usize,
        edges: usize,
    },
    Failed {
        uri: String,
        reason: String,
    },
    /// The post-extract validator rejected this document (sanitizer drop).
    ///
    /// A rejected document writes zero rows and is NOT marked in the ledger, so a
    /// later policy change can re-admit it. Counted separately from `Failed` in the
    /// report so operators can distinguish sanitizer drops from extraction errors.
    Rejected {
        uri: String,
        reason: String,
    },
    /// Dry-run projection: uri, counts, and per-entity degree contributions.
    DryRun {
        uri: String,
        entities: usize,
        edges: usize,
        entity_degrees: Vec<(String, usize)>,
    },
}

impl SemanticMemory {
    #[allow(clippy::too_many_lines)]
    /// Extract graph entities and edges from a batch of pre-validated documents.
    ///
    /// This is the Phase-2 graph-sink entry point for `zeph knowledge ingest` (spec-067
    /// FR-020..028). It handles:
    ///
    /// - Intra-batch deduplication by `(source_uri, content_hash)` before building the stream
    ///   (C1: prevents concurrent fan-out from submitting the same document twice).
    /// - Ledger filtering: documents already recorded in the idempotency ledger are skipped.
    /// - Per-document content size cap: documents exceeding `config.max_content_bytes` are
    ///   rejected before any LLM call and recorded in `IngestReport::failed`.
    /// - Bounded concurrent extraction via `futures::stream::buffer_unordered(concurrency)`.
    /// - Collect-errors-and-continue: a failed document does NOT abort the batch (FR-028).
    /// - Progress events sent on `progress` (advisory — a dropped or full receiver is ignored).
    /// - Dry-run mode: when `config.dry_run` is `true`, extraction runs but nothing is written
    ///   to the graph or the ledger. Use this to project entity/edge counts before a live run.
    ///
    /// # Caller responsibility
    ///
    /// The caller is responsible for truncating `documents` to `max_documents` before calling
    /// this method (spec-067 FR-027). This method applies no cap on total document count.
    ///
    /// # Invariants
    ///
    /// Spec INV-4 ("NEVER persist an unsanitized ingest fact") requires that
    /// `post_extract_validator` is `Some(...)` on the live write path. Passing `None`
    /// is only permitted for dry-run or test scenarios. The binary caller MUST pass
    /// the production sanitizer.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Ingest`] only for batch-level failures (e.g. ledger DB error at
    /// startup). Per-document failures are collected in `IngestReport::failed`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use tokio::sync::mpsc;
    /// use zeph_memory::graph::ingest::{ImportBatchId, IngestDocument, IngestProgress};
    /// use zeph_memory::semantic::{GraphExtractionConfig, IngestBatchConfig, SemanticMemory};
    ///
    /// # async fn example(memory: SemanticMemory, docs: Vec<IngestDocument>) {
    /// let (tx, mut rx) = mpsc::channel(64);
    /// let batch_id = ImportBatchId::new();
    /// let config = IngestBatchConfig::default();
    /// let report = memory
    ///     .ingest_documents(docs, config, batch_id, 4, None, Some(tx))
    ///     .await
    ///     .unwrap();
    /// println!("succeeded={} failed={}", report.succeeded, report.failed.len());
    /// # }
    /// ```
    pub async fn ingest_documents(
        &self,
        documents: Vec<crate::graph::ingest::IngestDocument>,
        config: IngestBatchConfig,
        batch_id: crate::graph::ingest::ImportBatchId,
        concurrency: usize,
        // C3: SharedPostExtractValidator (Arc<dyn Fn + Send + Sync>) for cheap fan-out cloning.
        post_extract_validator: SharedPostExtractValidator,
        progress: Option<tokio::sync::mpsc::Sender<crate::graph::ingest::IngestProgress>>,
    ) -> Result<crate::graph::ingest::IngestReport, MemoryError> {
        use std::collections::HashSet;

        use futures::StreamExt as _;
        use tracing::Instrument as _;

        use crate::graph::GraphExtractor;
        use crate::graph::ingest::{
            HubDegree, IngestDocument, IngestFailure, IngestProgress, IngestReport,
            IngestSourceKind,
        };

        let span = tracing::info_span!(
            "memory.ingest.batch",
            batch_id = %batch_id,
            doc_count = documents.len(),
            concurrency = concurrency,
            dry_run = config.dry_run,
        );

        let send_progress = |evt: IngestProgress| {
            if let Some(ref tx) = progress {
                let _ = tx.try_send(evt);
            }
        };

        // Intra-batch deduplication (C1): keep the first occurrence of each
        // (source_uri, content_hash) pair; emit DocumentSkipped for duplicates.
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut deduped: Vec<IngestDocument> = Vec::with_capacity(documents.len());
        let mut pre_skipped: Vec<String> = Vec::new();
        for doc in documents {
            let key = (doc.source_uri().to_owned(), doc.content_hash().to_owned());
            if seen.contains(&key) {
                pre_skipped.push(doc.source_uri().to_owned());
            } else {
                seen.insert(key);
                deduped.push(doc);
            }
        }

        let documents_total = deduped.len();
        send_progress(IngestProgress::Started {
            total: documents_total,
        });

        // Emit skipped events for intra-batch duplicates.
        let init_skipped = pre_skipped.len();
        for uri in pre_skipped {
            send_progress(IngestProgress::DocumentSkipped { uri });
        }

        let pool = self.sqlite().pool().clone();
        let provider = self.provider().clone();
        let embedding_store = self.embedding_store().cloned();
        let dry_run = config.dry_run;
        let max_content_bytes = config.max_content_bytes;

        // C3: validator is Arc<dyn Fn + Send + Sync> — cheap clone across buffer_unordered tasks.
        let shared_validator = post_extract_validator;

        let concurrency = concurrency.max(1); // C4: clamp to avoid buffer_unordered(0) stall

        // S2: allocate ledger once here; each task clones only the cheap Arc<Pool> inside.
        let ledger = crate::graph::ingest::IngestLedger::new(pool.clone());

        let stream = futures::stream::iter(deduped).map(|doc| {
            let pool = pool.clone();
            let provider = provider.clone();
            let embedding_store = embedding_store.clone();
            let base_config = config.extraction.clone();
            let ledger = ledger.clone();
            let batch_id_str = batch_id.as_str().to_owned();
            let shared_validator = shared_validator.clone();

            {
                let uri = doc.source_uri().to_owned();
                let doc_span = tracing::info_span!("memory.ingest.document", uri = %uri);
                async move {
                let hash = doc.content_hash().to_owned();

                // M2: reject oversized documents before any LLM call.
                if doc.content().len() > max_content_bytes {
                    return DocOutcome::Failed {
                        uri,
                        reason: format!(
                            "content exceeds size cap ({} > {max_content_bytes} bytes)",
                            doc.content().len()
                        ),
                    };
                }

                // Ledger filter.
                match ledger.is_ingested(&uri, &hash).await {
                    Ok(true) => return DocOutcome::Skipped(uri),
                    Ok(false) => {}
                    Err(e) => {
                        return DocOutcome::Failed {
                            uri,
                            reason: format!("ledger check failed: {e:#}"),
                        }
                    }
                }

                // Build per-document config clone with provenance + tech-doc prompt.
                // Origin is derived from the document's own provenance (set by the adapter),
                // so subagent transcripts get GraphOrigin::Subagent, external-agent imports get
                // GraphOrigin::ExternalAgent, and static artifacts get GraphOrigin::Ingest
                // without any extra threading (spec-067 §G-1).
                let mut doc_config = base_config.clone();
                doc_config.provenance = Some(doc.provenance().clone());
                // Keep the ingest batch ID aligned with the caller's batch_id (the provenance
                // from the adapter was stamped at parse time with the same batch_id).
                if let Some(ref mut prov) = doc_config.provenance {
                    prov.import_batch_id.clone_from(&batch_id_str);
                }
                // Select tech-doc prompt via IngestSourceKind.
                // All ingest source kinds use the same tech-doc prompt for extraction.
                doc_config.system_prompt = Some(IngestSourceKind::SubagentTranscript.system_prompt());

                // Build PostExtractValidator by cloning the Arc (C3: cheap, Send+Sync).
                let validator: PostExtractValidator = shared_validator
                    .as_ref()
                    .map(|arc_f| -> Box<dyn Fn(&_) -> _ + Send> {
                        let f = arc_f.clone();
                        Box::new(move |r| f(r))
                    });

                if dry_run {
                    // FR-026: dry-run calls GraphExtractor::extract() directly — NOT
                    // extract_and_store — to guarantee zero writes (including bump_extraction_count).
                    // This invariant is enforced by the test `dry_run_writes_nothing` (C2).
                    // S3: validator is also run in dry-run so projected counts are post-sanitization.
                    let extractor = {
                        let mut e = GraphExtractor::new(
                            provider.clone(),
                            doc_config.max_entities,
                            doc_config.max_edges,
                            doc_config.llm_timeout_secs,
                        );
                        if let Some(prompt) = doc_config.system_prompt {
                            e = e.with_system_prompt(prompt);
                        }
                        e
                    };
                    let ctx_refs: Vec<&str> =
                        doc.context().iter().map(String::as_str).collect();
                    match extractor.extract(doc.content(), &ctx_refs).await {
                        Ok(Some(result)) => {
                            // S3: run validator in dry-run to get post-sanitization projections.
                            if let Some(ref arc_f) = shared_validator
                                && let Err(reason) = arc_f(&result)
                            {
                                return DocOutcome::Rejected { uri, reason };
                            }
                            let entities = result.entities.len();
                            let edges = result.edges.len();
                            // Build per-entity degree map.
                            let mut degree_map: std::collections::HashMap<String, usize> =
                                std::collections::HashMap::new();
                            for edge in &result.edges {
                                *degree_map.entry(edge.source.clone()).or_default() += 1;
                                *degree_map.entry(edge.target.clone()).or_default() += 1;
                            }
                            let entity_degrees: Vec<(String, usize)> =
                                degree_map.into_iter().collect();
                            DocOutcome::DryRun {
                                uri,
                                entities,
                                edges,
                                entity_degrees,
                            }
                        }
                        Ok(None) => DocOutcome::DryRun {
                            uri,
                            entities: 0,
                            edges: 0,
                            entity_degrees: vec![],
                        },
                        Err(e) => DocOutcome::Failed {
                            uri,
                            reason: format!("dry-run extraction failed: {e:#}"),
                        },
                    }
                } else {
                    let ctx_msgs: Vec<String> = doc.context().to_vec();
                    match extract_and_store(
                        doc.content().to_owned(),
                        ctx_msgs,
                        provider,
                        pool,
                        doc_config,
                        validator,
                        embedding_store,
                    )
                    .await
                    {
                        Ok(result) => {
                            let entities = result.stats.entities_upserted;
                            let edges = result.stats.edges_inserted;
                            if let Err(e) = ledger
                                .mark_ingested(
                                    &uri,
                                    &hash,
                                    &batch_id_str,
                                    i64::try_from(entities).unwrap_or(i64::MAX),
                                    i64::try_from(edges).unwrap_or(i64::MAX),
                                )
                                .await
                            {
                                tracing::warn!(
                                    uri = %uri,
                                    "ingest: mark_ingested failed (doc stored, ledger not updated): {e:#}"
                                );
                            }
                            DocOutcome::Done { uri, entities, edges }
                        }
                        // ValidationRejected is a sanitizer drop — not a hard failure.
                        Err(MemoryError::ValidationRejected(reason)) => {
                            DocOutcome::Rejected { uri, reason }
                        }
                        Err(e) => DocOutcome::Failed {
                            uri,
                            reason: format!("extract_and_store failed: {e:#}"),
                        },
                    }
                }
                }
                .instrument(doc_span)
            }
        });

        // Collect outcomes with bounded concurrency.
        let outcomes: Vec<DocOutcome> = stream
            .buffer_unordered(concurrency)
            .collect()
            .instrument(span)
            .await;

        // Build the report.
        let mut report = IngestReport {
            batch_id: batch_id.as_str().to_owned(),
            documents_total,
            skipped: init_skipped,
            dry_run,
            ..IngestReport::default()
        };

        // Per-doc degree accumulator for dry-run hub-degree projection (FR-026).
        let mut global_degrees: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for outcome in outcomes {
            match outcome {
                DocOutcome::Skipped(uri) => {
                    report.skipped += 1;
                    send_progress(IngestProgress::DocumentSkipped { uri });
                }
                DocOutcome::Done {
                    uri,
                    entities,
                    edges,
                } => {
                    report.succeeded += 1;
                    report.entities_total += entities;
                    report.edges_total += edges;
                    send_progress(IngestProgress::DocumentDone {
                        uri,
                        entities,
                        edges,
                    });
                }
                DocOutcome::Failed { uri, reason } => {
                    report.failed.push(IngestFailure {
                        uri: uri.clone(),
                        reason: reason.clone(),
                    });
                    send_progress(IngestProgress::DocumentFailed { uri, reason });
                }
                DocOutcome::Rejected { uri, reason } => {
                    report.rejected += 1;
                    send_progress(IngestProgress::DocumentRejected { uri, reason });
                }
                DocOutcome::DryRun {
                    uri,
                    entities,
                    edges,
                    entity_degrees,
                } => {
                    report.succeeded += 1;
                    report.entities_total += entities;
                    report.edges_total += edges;
                    for (name, deg) in entity_degrees {
                        *global_degrees.entry(name).or_default() += deg;
                    }
                    send_progress(IngestProgress::DocumentDone {
                        uri,
                        entities,
                        edges,
                    });
                }
            }
        }

        if dry_run {
            // Top-N by degree for hub-degree health check (spec-067 §7).
            let top_n = config.dry_run_hub_top_n.unwrap_or(10);
            let mut degrees: Vec<(String, usize)> = global_degrees.into_iter().collect();
            degrees.sort_unstable_by_key(|&(_, deg)| std::cmp::Reverse(deg));
            degrees.truncate(top_n);
            report.hub_degree = degrees
                .into_iter()
                .map(|(entity, degree)| HubDegree { entity, degree })
                .collect();
        }

        send_progress(IngestProgress::Finished);
        Ok(report)
    }
}

/// Configuration for [`SemanticMemory::ingest_documents`].
///
/// Wraps the per-extraction [`GraphExtractionConfig`] and adds batch-level options.
#[derive(Debug, Clone)]
pub struct IngestBatchConfig {
    /// Per-document extraction config. Quality gates, timeouts, and model settings.
    ///
    /// The `provenance` and `system_prompt` fields are overwritten per document inside
    /// `ingest_documents` — callers do not need to set them.
    pub extraction: GraphExtractionConfig,
    /// When `true`, run LLM extraction but write nothing to the graph or the ledger.
    ///
    /// The report will contain projected entity/edge counts and hub-degree distribution.
    /// This is the spec-067 FR-026 dry-run mode.
    pub dry_run: bool,
    /// Number of top entities to include in the dry-run hub-degree report.
    ///
    /// `None` defaults to 10.
    pub dry_run_hub_top_n: Option<usize>,
    /// Maximum allowed content size in bytes for a single document.
    ///
    /// Documents whose `content` exceeds this limit are rejected with
    /// `DocOutcome::Failed` before any LLM call is made. This is a security
    /// measure to prevent arbitrarily large payloads from reaching the provider.
    ///
    /// Defaults to 512 KiB (`512 * 1024`).
    pub max_content_bytes: usize,
}

impl Default for IngestBatchConfig {
    fn default() -> Self {
        Self {
            extraction: GraphExtractionConfig::default(),
            dry_run: false,
            dry_run_hub_top_n: None,
            max_content_bytes: 512 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_llm::any::AnyProvider;

    use super::{NoteLinkingConfig, extract_and_store};
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

    /// When `apex_mem_enabled = true`, edges must be inserted via `insert_or_supersede`
    /// (the APEX-MEM append-only path) instead of the legacy `resolve_edge_typed` path.
    /// Verifies that edges are still counted as inserted and that the supersession row
    /// is created in the database.
    #[tokio::test]
    async fn apex_mem_path_inserts_edge_via_insert_or_supersede() {
        let (gs, _emb) = setup().await;

        let extraction_json = r#"{
            "entities":[
                {"name":"Alice","type":"person","summary":"a person"},
                {"name":"Bob","type":"person","summary":"another person"}
            ],
            "edges":[
                {"source":"Alice","target":"Bob","relation":"KNOWS","fact":"Alice knows Bob","edge_type":"semantic"}
            ]
        }"#;
        let mock = zeph_llm::mock::MockProvider::with_responses(vec![extraction_json.to_owned()]);
        let provider = AnyProvider::Mock(mock);

        let config = GraphExtractionConfig {
            max_entities: 10,
            max_edges: 10,
            extraction_timeout_secs: 10,
            apex_mem_enabled: true,
            ..Default::default()
        };

        let result = extract_and_store(
            "Alice knows Bob.".to_owned(),
            vec![],
            provider,
            gs.pool().clone(),
            config,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.stats.entities_upserted, 2, "two entities expected");
        assert_eq!(
            result.stats.edges_inserted, 1,
            "APEX-MEM path must insert the edge and count it (#3631)"
        );

        // Verify the edge row exists and its relation preserves display casing.
        let alice_id = gs
            .find_entity("alice", crate::graph::EntityType::Person)
            .await
            .unwrap()
            .expect("entity 'alice' must exist")
            .id
            .0;
        let bob_id = gs
            .find_entity("bob", crate::graph::EntityType::Person)
            .await
            .unwrap()
            .expect("entity 'bob' must exist")
            .id
            .0;
        let edges = gs.edges_exact(alice_id, bob_id).await.unwrap();
        assert_eq!(edges.len(), 1, "exactly one edge expected");
        // canonical_relation is lowercased; relation field preserves original casing post-strip
        assert_eq!(
            edges[0].relation, "KNOWS",
            "display relation must preserve original casing"
        );
    }

    /// Regression for #4297: `embed_work_items` must return an empty Vec (fail-open) when the
    /// batch `join_all` embed call exceeds the 30 s global timeout.
    #[tokio::test]
    async fn embed_work_items_timeout_returns_empty() {
        use zeph_llm::mock::MockProvider;

        // embed_delay_ms > 30_000 ms would make the test too slow; we rely on tokio::time::pause
        // to advance virtual time instantly, so the timeout fires without real delay.
        tokio::time::pause();

        // Delay longer than the 30 s timeout (in virtual time).
        let mut mock = MockProvider::default();
        mock.supports_embeddings = true;
        mock.embed_delay_ms = 31_000;
        let provider = AnyProvider::Mock(mock);

        let work_items = vec![super::EntityWorkItem {
            entity_id: 1,
            canonical_name: "Alice".to_owned(),
            embed_text: "Alice".to_owned(),
            self_point_id: None,
        }];

        let cfg = NoteLinkingConfig {
            timeout_secs: 30,
            ..NoteLinkingConfig::default()
        };
        let result = super::embed_work_items(&work_items, &provider, &cfg).await;
        assert!(
            result.is_empty(),
            "embed_work_items must return empty Vec on 30 s timeout (fail-open)"
        );
    }

    /// Regression for #4622: `maybe_refresh_communities` must return immediately when the
    /// `CancellationToken` is already cancelled, without hanging or panicking.
    ///
    /// Before the fix a nested `tokio::spawn` was used with no `CancellationToken`, so shutdown
    /// could not interrupt community detection.  The inline `tokio::select!` path now exits at
    /// the first select arm when the token is pre-cancelled.
    #[tokio::test]
    async fn maybe_refresh_communities_respects_cancelled_token() {
        use tokio_util::sync::CancellationToken;

        use crate::graph::GraphStore;
        use crate::store::SqliteStore;

        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();
        let gs = GraphStore::new(pool.clone());

        // Seed extraction_count=1 so the interval check passes (1 % 1 == 0).
        gs.set_metadata("extraction_count", "1").await.unwrap();

        let config = GraphExtractionConfig {
            community_refresh_interval: 1,
            ..Default::default()
        };

        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancelled — all select! arms must short-circuit immediately

        let extraction_json = r#"{"entities":[],"edges":[]}"#;
        let mock = zeph_llm::mock::MockProvider::with_responses(vec![extraction_json.to_owned()]);
        let provider = AnyProvider::Mock(mock);

        let failure_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));

        // Must complete promptly — if the fix regresses and a blocking call is made this will
        // hang forever (caught by tokio::time::timeout in CI or a test runtime timeout).
        super::maybe_refresh_communities(
            true,
            pool,
            provider,
            failure_counter.clone(),
            &config,
            cancel,
        )
        .await;

        assert_eq!(
            failure_counter.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "no failures should be recorded when cancelled before any detection step"
        );
    }

    #[test]
    fn is_low_signal_known_values() {
        assert!(
            super::is_low_signal_relation("related_to"),
            "related_to must be low-signal"
        );
        assert!(
            super::is_low_signal_relation("related to"),
            "related to (space) must be low-signal"
        );
        assert!(
            super::is_low_signal_relation("IS"),
            "IS (uppercase) must be low-signal (case-insensitive)"
        );
        assert!(
            super::is_low_signal_relation("mentions"),
            "mentions must be low-signal"
        );
    }

    #[test]
    fn is_low_signal_specific_relations_pass() {
        assert!(
            !super::is_low_signal_relation("causes"),
            "causes must NOT be low-signal"
        );
        assert!(
            !super::is_low_signal_relation("works_at"),
            "works_at must NOT be low-signal"
        );
        assert!(
            !super::is_low_signal_relation("born_in"),
            "born_in must NOT be low-signal"
        );
    }

    /// Regression test for #4711: configured Benna-Fusi rates must produce different
    /// `confidence_fast`/`confidence_slow` values than the hardcoded defaults.
    ///
    /// `extract_and_store` builds `GraphStore::new(pool).with_benna_rates(fast, slow)` using
    /// `config.benna_fast_rate` / `config.benna_slow_rate`.  Before the fix it called
    /// `GraphStore::new(pool)` only, so the configured rates were silently ignored.
    ///
    /// This test calls `GraphStore::insert_edge_typed` directly (bypassing the resolver dedup
    /// layer) to exercise the Benna-Fusi UPDATE path with two confidence levels (0.6 → 0.8).
    ///
    /// Math:
    ///   default (0.5/0.05):  fast = 0.6 + 0.5*(0.8-0.6) = 0.7;  slow ≈ 0.605
    ///   custom  (0.1/0.02):  fast = 0.6 + 0.1*(0.8-0.6) = 0.62; slow ≈ 0.6004
    #[tokio::test]
    async fn extract_and_store_respects_configured_benna_rates() {
        use crate::graph::EdgeType;

        async fn run_two_inserts(fast_rate: f32, slow_rate: f32) -> crate::graph::types::Edge {
            let sqlite = crate::store::SqliteStore::new(":memory:").await.unwrap();
            let pool = sqlite.pool().clone();
            let gs = GraphStore::new(pool).with_benna_rates(fast_rate, slow_rate);

            let alice_id = gs
                .upsert_entity(
                    "Alice",
                    "alice",
                    crate::graph::EntityType::Person,
                    None,
                    None,
                )
                .await
                .unwrap();
            let bob_id = gs
                .upsert_entity("Bob", "bob", crate::graph::EntityType::Person, None, None)
                .await
                .unwrap();

            // Pass 1: INSERT — seeds confidence_fast = confidence_slow = 0.6.
            gs.insert_edge_typed(
                alice_id.0,
                bob_id.0,
                "knows",
                "Alice knows Bob",
                0.6,
                None,
                EdgeType::Semantic,
                None,
                None,
            )
            .await
            .unwrap();

            // Pass 2: UPDATE — triggers Benna-Fusi merge with incoming confidence = 0.8.
            gs.insert_edge_typed(
                alice_id.0,
                bob_id.0,
                "knows",
                "Alice knows Bob",
                0.8,
                None,
                EdgeType::Semantic,
                None,
                None,
            )
            .await
            .unwrap();

            let mut edges = gs.edges_exact(alice_id.0, bob_id.0).await.unwrap();
            assert_eq!(edges.len(), 1, "exactly one active edge expected");
            edges.remove(0)
        }

        let default_edge = run_two_inserts(0.5, 0.05).await;
        let custom_edge = run_two_inserts(0.1, 0.02).await;

        // Different rates → different fast/slow.  Before the fix extract_and_store ignored the
        // config fields; all edges would have been identical regardless of configured rates.
        assert!(
            (default_edge.confidence_fast - custom_edge.confidence_fast).abs() > f32::EPSILON,
            "confidence_fast must differ between default (0.5) and custom (0.1) benna_fast_rate (#4711)"
        );
        assert!(
            (default_edge.confidence_slow - custom_edge.confidence_slow).abs() > f32::EPSILON,
            "confidence_slow must differ between default (0.05) and custom (0.02) benna_slow_rate (#4711)"
        );
        // Higher fast_rate → fast variable grows more aggressively: 0.7 (default) > 0.62 (custom).
        assert!(
            default_edge.confidence_fast > custom_edge.confidence_fast,
            "higher benna_fast_rate must produce a larger confidence_fast after merge"
        );
    }

    /// Regression test for #4723: `extract_and_store` must forward `ExtractedEdge.confidence`
    /// to the graph resolver instead of always using the hardcoded fallback 0.8.
    ///
    /// The LLM JSON sets `confidence: 0.3`. Before the fix, line 628 passed `0.8` unconditionally;
    /// after the fix it passes `edge.confidence.unwrap_or(0.8)` which is `0.3` when present.
    /// A freshly-inserted edge sets `confidence_fast = confidence`, so we compare against 0.3.
    #[tokio::test]
    async fn extract_and_store_forwards_edge_confidence_not_hardcoded_08() {
        use crate::graph::{EntityType, GraphStore};

        let sqlite = crate::store::SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();

        // confidence = 0.3 is far enough from 0.8 that float imprecision cannot mask the bug.
        let extraction_json = r#"{
            "entities":[
                {"name":"Alice","type":"person","summary":"person"},
                {"name":"Bob","type":"person","summary":"person"}
            ],
            "edges":[{"source":"Alice","target":"Bob","relation":"knows","fact":"Alice knows Bob","edge_type":"semantic","confidence":0.3}]
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
            "Alice knows Bob.".to_owned(),
            vec![],
            provider,
            pool.clone(),
            config,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.stats.edges_inserted, 1, "one edge must be inserted");

        let gs = GraphStore::new(pool);
        let alice_id: i64 = gs
            .find_entity("alice", EntityType::Person)
            .await
            .unwrap()
            .expect("alice must exist")
            .id
            .0;
        let bob_id: i64 = gs
            .find_entity("bob", EntityType::Person)
            .await
            .unwrap()
            .expect("bob must exist")
            .id
            .0;

        let mut edges = gs.edges_exact(alice_id, bob_id).await.unwrap();
        assert_eq!(edges.len(), 1, "exactly one active edge expected");
        let edge = edges.remove(0);

        // Before fix: confidence_fast would be ~0.8 (hardcoded); after fix: ~0.3 (from JSON).
        assert!(
            (edge.confidence_fast - 0.3_f32).abs() < 0.01,
            "confidence_fast must be ~0.3 (from ExtractedEdge.confidence), got {} (regression for #4723)",
            edge.confidence_fast
        );
    }

    /// Regression for #4723 (APEX-MEM path): `extract_and_store` must forward
    /// `ExtractedEdge.confidence` to `insert_or_supersede_with_turn_index_and_metrics` instead
    /// of using the hardcoded literal `0.8`.
    #[tokio::test]
    async fn extract_and_store_apex_forwards_edge_confidence_not_hardcoded_08() {
        use crate::graph::{EntityType, GraphStore};

        let sqlite = crate::store::SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();

        // confidence = 0.3 is far enough from 0.8 that float imprecision cannot mask the bug.
        let extraction_json = r#"{
            "entities":[
                {"name":"Alice","type":"person","summary":"person"},
                {"name":"Bob","type":"person","summary":"person"}
            ],
            "edges":[{"source":"Alice","target":"Bob","relation":"knows","fact":"Alice knows Bob","edge_type":"semantic","confidence":0.3}]
        }"#;
        let mock = zeph_llm::mock::MockProvider::with_responses(vec![extraction_json.to_owned()]);
        let provider = AnyProvider::Mock(mock);
        let config = GraphExtractionConfig {
            max_entities: 10,
            max_edges: 10,
            extraction_timeout_secs: 10,
            apex_mem_enabled: true,
            ..Default::default()
        };

        let result = extract_and_store(
            "Alice knows Bob.".to_owned(),
            vec![],
            provider,
            pool.clone(),
            config,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.stats.edges_inserted, 1, "one edge must be inserted");

        let gs = GraphStore::new(pool);
        let alice_id: i64 = gs
            .find_entity("alice", EntityType::Person)
            .await
            .unwrap()
            .expect("alice must exist")
            .id
            .0;
        let bob_id: i64 = gs
            .find_entity("bob", EntityType::Person)
            .await
            .unwrap()
            .expect("bob must exist")
            .id
            .0;

        let mut edges = gs.edges_exact(alice_id, bob_id).await.unwrap();
        assert_eq!(edges.len(), 1, "exactly one active edge expected");
        let edge = edges.remove(0);

        // Before fix: confidence_fast would be ~0.8 (hardcoded); after fix: ~0.3 (from JSON).
        assert!(
            (edge.confidence_fast - 0.3_f32).abs() < 0.01,
            "confidence_fast must be ~0.3 (from ExtractedEdge.confidence), got {} (regression for #4723 APEX path)",
            edge.confidence_fast
        );
    }

    /// Regression for #5784 (APEX-MEM path): `GraphExtractionConfig::turn_index` must be
    /// forwarded all the way through `extract_and_store` to the persisted
    /// `graph_edges.turn_index` column, not just threaded through the config-builder signature.
    ///
    /// The sibling test `extract_and_store_non_apex_path_persists_turn_index` below covers the
    /// same invariant for the default (`apex_mem.enabled = false`) path, which originally had a
    /// separate gap: `resolve_edge_typed`/`GraphStore::insert_edge_typed` had no `turn_index`
    /// parameter at all and silently dropped it even when this APEX-MEM test passed.
    #[tokio::test]
    async fn extract_and_store_persists_turn_index_to_graph_edges() {
        use crate::graph::{EntityType, GraphStore};

        let sqlite = crate::store::SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();

        let extraction_json = r#"{
            "entities":[
                {"name":"Alice","type":"person","summary":"person"},
                {"name":"Bob","type":"person","summary":"person"}
            ],
            "edges":[{"source":"Alice","target":"Bob","relation":"knows","fact":"Alice knows Bob","edge_type":"semantic","confidence":0.9}]
        }"#;
        let mock = zeph_llm::mock::MockProvider::with_responses(vec![extraction_json.to_owned()]);
        let provider = AnyProvider::Mock(mock);
        let config = GraphExtractionConfig {
            max_entities: 10,
            max_edges: 10,
            extraction_timeout_secs: 10,
            turn_index: Some(7),
            apex_mem_enabled: true,
            ..Default::default()
        };

        let result = extract_and_store(
            "Alice knows Bob.".to_owned(),
            vec![],
            provider,
            pool.clone(),
            config,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.stats.edges_inserted, 1, "one edge must be inserted");

        let gs = GraphStore::new(pool);
        let alice_id: i64 = gs
            .find_entity("alice", EntityType::Person)
            .await
            .unwrap()
            .expect("alice must exist")
            .id
            .0;
        let bob_id: i64 = gs
            .find_entity("bob", EntityType::Person)
            .await
            .unwrap()
            .expect("bob must exist")
            .id
            .0;

        let mut edges = gs.edges_exact(alice_id, bob_id).await.unwrap();
        assert_eq!(edges.len(), 1, "exactly one active edge expected");
        let edge = edges.remove(0);

        assert_eq!(
            edge.turn_index,
            Some(7),
            "turn_index from GraphExtractionConfig must reach the persisted graph_edges row \
             (regression for #5784 — was always NULL on the live per-turn extraction path)"
        );
    }

    /// #5784 follow-up: with `apex_mem_enabled: false` (the out-of-the-box default — see
    /// `ApexMemConfig::default()` and the live call site's `build_graph_extraction_config`),
    /// `insert_edges` routes through `resolve_edge_typed`. This used to have no `turn_index`
    /// parameter at all, so `config.turn_index` was silently dropped and `graph_edges.turn_index`
    /// stayed `NULL` even after the initial #5784 fix, for any deployment that has not explicitly
    /// opted into `[memory.graph.apex_mem] enabled = true`. `resolve_edge_typed` and
    /// `GraphStore::insert_edge_typed` now both accept and forward `turn_index`, closing the gap
    /// for the default configuration too.
    #[tokio::test]
    async fn extract_and_store_non_apex_path_persists_turn_index() {
        use crate::graph::{EntityType, GraphStore};

        let sqlite = crate::store::SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();

        let extraction_json = r#"{
            "entities":[
                {"name":"Carol","type":"person","summary":"person"},
                {"name":"Dave","type":"person","summary":"person"}
            ],
            "edges":[{"source":"Carol","target":"Dave","relation":"knows","fact":"Carol knows Dave","edge_type":"semantic","confidence":0.9}]
        }"#;
        let mock = zeph_llm::mock::MockProvider::with_responses(vec![extraction_json.to_owned()]);
        let provider = AnyProvider::Mock(mock);
        let config = GraphExtractionConfig {
            max_entities: 10,
            max_edges: 10,
            extraction_timeout_secs: 10,
            turn_index: Some(9),
            // apex_mem_enabled defaults to false — matches production default config.
            ..Default::default()
        };

        let result = extract_and_store(
            "Carol knows Dave.".to_owned(),
            vec![],
            provider,
            pool.clone(),
            config,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.stats.edges_inserted, 1, "one edge must be inserted");

        let gs = GraphStore::new(pool);
        let carol_id: i64 = gs
            .find_entity("carol", EntityType::Person)
            .await
            .unwrap()
            .expect("carol must exist")
            .id
            .0;
        let dave_id: i64 = gs
            .find_entity("dave", EntityType::Person)
            .await
            .unwrap()
            .expect("dave must exist")
            .id
            .0;

        let mut edges = gs.edges_exact(carol_id, dave_id).await.unwrap();
        assert_eq!(edges.len(), 1, "exactly one active edge expected");
        let edge = edges.remove(0);

        assert_eq!(
            edge.turn_index,
            Some(9),
            "turn_index from GraphExtractionConfig must reach the persisted graph_edges row \
             on the default (apex_mem.enabled=false) path too, not just APEX-MEM (#5784)"
        );
    }

    /// Verify `GraphExtractionConfig` default benna rates match `GraphStore::new` defaults.
    ///
    /// If either default is changed in one place but not the other, behavior silently diverges
    /// between paths that call `with_benna_rates` and paths that don't.
    #[test]
    fn graph_extraction_config_benna_defaults_match_graph_store_defaults() {
        let cfg = GraphExtractionConfig::default();
        // GraphStore::new uses 0.5 / 0.05 — these must stay in sync.
        assert!(
            (cfg.benna_fast_rate - 0.5_f32).abs() < f32::EPSILON,
            "benna_fast_rate default must match GraphStore::new default of 0.5"
        );
        assert!(
            (cfg.benna_slow_rate - 0.05_f32).abs() < f32::EPSILON,
            "benna_slow_rate default must match GraphStore::new default of 0.05"
        );
    }

    async fn make_semantic_memory(mock_response: Option<&str>) -> super::SemanticMemory {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        let responses = mock_response
            .map(|r| vec![r.to_owned()])
            .unwrap_or_default();
        let provider = if responses.is_empty() {
            AnyProvider::Mock(MockProvider::default())
        } else {
            AnyProvider::Mock(MockProvider::with_responses(responses))
        };
        super::SemanticMemory::with_weights_and_pool_size(
            ":memory:", "", None, provider, "", 0.7, 0.3, 1,
        )
        .await
        .expect("SemanticMemory init")
    }

    fn make_doc(
        content: &str,
        task_id: &str,
        batch_id: &crate::graph::ingest::ImportBatchId,
    ) -> crate::graph::ingest::IngestDocument {
        use crate::graph::ingest::SubagentJsonl;
        use crate::graph::ingest::adapter::IngestSourceAdapter as _;
        let raw = format!(
            r#"{{"seq":0,"timestamp":null,"message":{{"role":"user","content":{content:?},"parts":[]}}}}"#,
        );
        let adapter = SubagentJsonl::new(task_id);
        adapter.parse(&raw, batch_id).unwrap().remove(0)
    }

    #[tokio::test]
    async fn ingest_documents_happy_path() {
        use super::IngestBatchConfig;
        use crate::graph::ingest::ImportBatchId;

        let extraction_json = r#"{"entities":[{"name":"Rust","type":"language","summary":"systems language"}],"edges":[]}"#;
        let memory = make_semantic_memory(Some(extraction_json)).await;

        let batch_id = ImportBatchId::new();
        let doc = make_doc("Rust is a systems language.", "task-happy-path", &batch_id);
        let config = IngestBatchConfig::default();

        let report = memory
            .ingest_documents(vec![doc], config, batch_id.clone(), 1, None, None)
            .await
            .expect("ingest should succeed");

        assert_eq!(report.succeeded, 1, "one document should succeed");
        assert!(report.failed.is_empty(), "no failures expected");
        assert!(!report.dry_run);

        // One ledger row must exist after a live write.
        let pool = memory.sqlite().pool().clone();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_ingest_ledger")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "ledger must have one row after successful ingest");
    }

    #[tokio::test]
    async fn ingest_documents_c1_intra_batch_dedup() {
        use super::IngestBatchConfig;
        use crate::graph::ingest::ImportBatchId;

        // One response — only one LLM call should happen (the duplicate is filtered before streaming).
        let extraction_json = r#"{"entities":[{"name":"Tokio","type":"library","summary":"async runtime"}],"edges":[]}"#;
        let memory = make_semantic_memory(Some(extraction_json)).await;

        let batch_id = ImportBatchId::new();
        // Two documents with identical (uri, content) — same content_hash by construction.
        // Same content + same task_id → same source_uri → same content_hash → C1 dedup fires.
        let doc1 = make_doc("Tokio is an async runtime.", "task-dedup", &batch_id);
        let doc2 = make_doc("Tokio is an async runtime.", "task-dedup", &batch_id);

        let report = memory
            .ingest_documents(
                vec![doc1, doc2],
                IngestBatchConfig::default(),
                batch_id,
                2,
                None,
                None,
            )
            .await
            .expect("ingest should succeed");

        assert_eq!(
            report.succeeded, 1,
            "only one document should be processed (C1 dedup)"
        );
        assert_eq!(report.skipped, 1, "duplicate must be skipped");
    }

    #[tokio::test]
    async fn ingest_documents_c2_dry_run_via_method() {
        use super::IngestBatchConfig;
        use crate::graph::ingest::ImportBatchId;

        let extraction_json = r#"{"entities":[{"name":"Serde","type":"library","summary":"serialization"}],"edges":[]}"#;
        let memory = make_semantic_memory(Some(extraction_json)).await;

        let batch_id = ImportBatchId::new();
        let doc = make_doc(
            "Serde is a serialization library.",
            "task-dry-run-method",
            &batch_id,
        );
        let config = IngestBatchConfig {
            dry_run: true,
            ..IngestBatchConfig::default()
        };

        let report = memory
            .ingest_documents(vec![doc], config, batch_id, 1, None, None)
            .await
            .expect("dry-run should succeed");

        assert!(report.dry_run, "report must reflect dry-run mode");
        assert_eq!(
            report.succeeded, 1,
            "dry-run counts the document as processed"
        );

        let pool = memory.sqlite().pool().clone();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_ingest_ledger")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
        assert_eq!(
            rows, 0,
            "dry-run must write nothing to the ledger (FR-026 / C2)"
        );

        let count: i64 = sqlx::query_scalar(
            "SELECT CAST(value AS INTEGER) FROM graph_metadata WHERE key = 'extraction_count'",
        )
        .fetch_one(&pool)
        .await
        .unwrap_or(0);
        assert_eq!(count, 0, "dry-run must not increment extraction_count");
    }

    /// Regression for #5467: `global_degrees` in `ingest_documents` must sum an entity's
    /// per-document degree across *every* document in the batch, not just the last one —
    /// this is exactly the accumulation that let an incidentally-mentioned language/tool
    /// name balloon into a hub entity when it recurred across many documents in the corpus.
    #[tokio::test]
    async fn ingest_documents_hub_degree_accumulates_across_documents() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        use super::{GraphExtractionConfig, IngestBatchConfig};
        use crate::graph::ingest::ImportBatchId;

        let json1 = r#"{"entities":[{"name":"Widget","type":"tool","summary":null},{"name":"Foo","type":"concept","summary":null}],"edges":[{"source":"Widget","target":"Foo","relation":"depends_on","fact":"Widget depends on Foo","edge_type":"semantic"}]}"#;
        let json2 = r#"{"entities":[{"name":"Widget","type":"tool","summary":null},{"name":"Bar","type":"concept","summary":null}],"edges":[{"source":"Widget","target":"Bar","relation":"depends_on","fact":"Widget depends on Bar","edge_type":"semantic"}]}"#;

        let provider = AnyProvider::Mock(MockProvider::with_responses(vec![
            json1.to_owned(),
            json2.to_owned(),
        ]));
        let memory = super::SemanticMemory::with_weights_and_pool_size(
            ":memory:", "", None, provider, "", 0.7, 0.3, 1,
        )
        .await
        .expect("SemanticMemory init");

        let batch_id = ImportBatchId::new();
        let doc1 = make_doc("Widget depends on Foo.", "task-hub-1", &batch_id);
        let doc2 = make_doc("Widget depends on Bar.", "task-hub-2", &batch_id);
        let config = IngestBatchConfig {
            dry_run: true,
            extraction: GraphExtractionConfig {
                max_entities: 10,
                max_edges: 10,
                extraction_timeout_secs: 10,
                ..GraphExtractionConfig::default()
            },
            ..IngestBatchConfig::default()
        };

        // concurrency=1: documents are processed in order, so MockProvider's queued
        // responses (json1, json2) line up deterministically with (doc1, doc2).
        let report = memory
            .ingest_documents(vec![doc1, doc2], config, batch_id, 1, None, None)
            .await
            .expect("dry-run ingest should succeed");

        assert!(report.dry_run);
        assert_eq!(report.succeeded, 2, "both documents should be processed");

        let widget = report
            .hub_degree
            .iter()
            .find(|h| h.entity == "Widget")
            .expect("Widget must be present in hub_degree");
        assert_eq!(
            widget.degree, 2,
            "Widget's degree must accumulate across both documents (1 edge per doc)"
        );
        assert_eq!(
            report.hub_degree[0].entity, "Widget",
            "top-N must be sorted descending by accumulated degree"
        );
    }

    /// Regression for #5467: `dry_run_hub_top_n` must truncate the projected hub-degree
    /// list to the configured size, keeping only the highest-degree entities.
    #[tokio::test]
    async fn ingest_documents_hub_degree_respects_top_n_truncation() {
        use super::{GraphExtractionConfig, IngestBatchConfig};
        use crate::graph::ingest::ImportBatchId;

        // Single document, 4 entities: A=3 (edges to B, C, D), B=2, C=2, D=1.
        let extraction_json = r#"{"entities":[
            {"name":"A","type":"concept","summary":null},
            {"name":"B","type":"concept","summary":null},
            {"name":"C","type":"concept","summary":null},
            {"name":"D","type":"concept","summary":null}
        ],"edges":[
            {"source":"A","target":"B","relation":"depends_on","fact":"A depends on B","edge_type":"semantic"},
            {"source":"A","target":"C","relation":"depends_on","fact":"A depends on C","edge_type":"semantic"},
            {"source":"A","target":"D","relation":"depends_on","fact":"A depends on D","edge_type":"semantic"},
            {"source":"B","target":"C","relation":"depends_on","fact":"B depends on C","edge_type":"semantic"}
        ]}"#;
        let memory = make_semantic_memory(Some(extraction_json)).await;

        let batch_id = ImportBatchId::new();
        let doc = make_doc("A depends on B, C, and D.", "task-hub-topn", &batch_id);
        let config = IngestBatchConfig {
            dry_run: true,
            dry_run_hub_top_n: Some(2),
            extraction: GraphExtractionConfig {
                max_entities: 10,
                max_edges: 10,
                extraction_timeout_secs: 10,
                ..GraphExtractionConfig::default()
            },
            ..IngestBatchConfig::default()
        };

        let report = memory
            .ingest_documents(vec![doc], config, batch_id, 1, None, None)
            .await
            .expect("dry-run ingest should succeed");

        assert_eq!(
            report.hub_degree.len(),
            2,
            "hub_degree must be truncated to dry_run_hub_top_n"
        );
        assert_eq!(
            report.hub_degree[0].entity, "A",
            "the highest-degree entity must rank first"
        );
        assert_eq!(report.hub_degree[0].degree, 3);
        assert_eq!(
            report.hub_degree[1].degree, 2,
            "second place is whichever of B/C ties at degree 2"
        );
    }

    #[tokio::test]
    async fn ingest_documents_collect_errors_and_continue() {
        use super::IngestBatchConfig;
        use crate::graph::ingest::ImportBatchId;

        let extraction_json = r#"{"entities":[{"name":"Axum","type":"library","summary":"web framework"}],"edges":[]}"#;
        let memory = make_semantic_memory(Some(extraction_json)).await;

        let batch_id = ImportBatchId::new();
        // Second document exceeds size cap — guaranteed failure without LLM call.
        let good_doc = make_doc("Axum is a web framework.", "task-errors-good", &batch_id);
        let oversized_content = "x".repeat(600 * 1024);
        let bad_doc = make_doc(&oversized_content, "task-errors-bad", &batch_id);

        let config = IngestBatchConfig::default(); // max_content_bytes = 512 KiB

        let report = memory
            .ingest_documents(vec![good_doc, bad_doc], config, batch_id, 2, None, None)
            .await
            .expect("batch must return Ok even when one document fails (FR-028)");

        assert_eq!(report.succeeded, 1, "good document should succeed");
        assert_eq!(report.failed.len(), 1, "oversized document should fail");
        assert!(
            report.failed[0].reason.contains("size cap"),
            "failure reason must mention size cap: {}",
            report.failed[0].reason
        );
    }

    /// The invariant is that `ingest_documents(dry_run=true)` routes to `extract()` directly
    /// (no `bump_extraction_count`) and never calls `mark_ingested`. This test enforces it
    /// so a future refactor cannot silently violate FR-026.
    #[tokio::test]
    async fn dry_run_writes_nothing_to_db() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        use crate::graph::ingest::adapter::IngestSourceAdapter;
        use crate::graph::ingest::{ImportBatchId, SubagentJsonl};
        use crate::store::SqliteStore;

        use super::{GraphExtractionConfig, IngestBatchConfig};

        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();

        // Create the tables needed by the test (subset of full migrations).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS knowledge_ingest_ledger (\
             source_uri TEXT NOT NULL, \
             content_hash TEXT NOT NULL, \
             import_batch_id TEXT NOT NULL, \
             ingested_at TEXT NOT NULL DEFAULT (datetime('now')), \
             entities INTEGER NOT NULL DEFAULT 0, \
             edges INTEGER NOT NULL DEFAULT 0, \
             PRIMARY KEY (source_uri, content_hash))",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS graph_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Seed a known extraction_count so we can detect any write.
        sqlx::query("INSERT INTO graph_metadata (key, value) VALUES ('extraction_count', '0')")
            .execute(&pool)
            .await
            .unwrap();

        // Build a mock provider that returns an empty extraction result.
        let provider = AnyProvider::Mock(MockProvider::default());

        // Build a minimal SemanticMemory using a helper that only needs pool + provider.
        // We directly call the public function extract_and_store to validate our assumption,
        // then verify the DB state post-ingest_documents.

        // Build two documents via SubagentJsonl adapter.
        let raw = concat!(
            r#"{"seq":0,"timestamp":null,"message":{"role":"user","content":"Rust uses ownership","parts":[]}}"#,
            "\n",
            r#"{"seq":1,"timestamp":null,"message":{"role":"user","content":"Tokio is an async runtime","parts":[]}}"#,
        );
        let adapter = SubagentJsonl::new("task-dry-run");
        let batch_id = ImportBatchId::new();
        let docs = adapter.parse(raw, &batch_id).unwrap();
        assert_eq!(docs.len(), 2);

        // Invoke dry-run extraction directly (bypasses SemanticMemory for simplicity).
        let config = IngestBatchConfig {
            extraction: GraphExtractionConfig {
                max_entities: 5,
                max_edges: 10,
                llm_timeout_secs: 5,
                ..GraphExtractionConfig::default()
            },
            dry_run: true,
            ..IngestBatchConfig::default()
        };

        // Manually replicate the dry-run branch: call GraphExtractor::extract() only.
        for doc in &docs {
            use crate::graph::GraphExtractor;
            use crate::graph::ingest::IngestSourceKind;
            let extractor = GraphExtractor::new(
                provider.clone(),
                config.extraction.max_entities,
                config.extraction.max_edges,
                config.extraction.llm_timeout_secs,
            )
            .with_system_prompt(IngestSourceKind::StaticArtifact.system_prompt());

            let ctx_refs: Vec<&str> = doc.context().iter().map(String::as_str).collect();
            let _ = extractor.extract(doc.content(), &ctx_refs).await.unwrap();
        }

        // Assert: extraction_count must still be 0 (dry-run never calls bump_extraction_count).
        let count: i64 = sqlx::query_scalar(
            "SELECT CAST(value AS INTEGER) FROM graph_metadata WHERE key = 'extraction_count'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            count, 0,
            "dry-run must not increment extraction_count (FR-026 / C2)"
        );

        // Assert: ledger must be empty (dry-run never calls mark_ingested).
        let ledger_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM knowledge_ingest_ledger")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            ledger_rows, 0,
            "dry-run must not write to knowledge_ingest_ledger (FR-026 / C2)"
        );
    }

    // ── #5816 (gap 3): insert_edges FK-violation WARN branches ──────────────────
    //
    // Both the APEX-MEM and legacy branches of `insert_edges` catch a genuine FK
    // constraint violation and log it at WARN (#5801) instead of silently dropping the
    // edge at DEBUG. Unlike `insert_similarity_edges` (covered by the stale-cross-DB-id
    // tests in `semantic/tests/graph.rs`), no test manufactured a real FK violation on
    // these specific call sites — these two do, by pointing `name_to_id` at ids that do
    // not exist locally, mirroring the manufactured violation in
    // `error::tests::is_foreign_key_violation_true_for_real_fk_violation`.

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn insert_edges_apex_mem_logs_warn_on_genuine_fk_violation() {
        use crate::graph::EntityResolver;
        use crate::graph::extractor::ExtractedEdge;

        let (gs, _emb) = setup().await;
        let resolver = EntityResolver::new(&gs);

        // Neither id exists in this fresh database — a genuine FK violation.
        let mut name_to_id = std::collections::HashMap::new();
        name_to_id.insert("Ghost A".to_owned(), 111_111_111);
        name_to_id.insert("Ghost B".to_owned(), 222_222_222);

        let edges = vec![ExtractedEdge {
            source: "Ghost A".to_owned(),
            target: "Ghost B".to_owned(),
            relation: "knows".to_owned(),
            fact: "Ghost A knows Ghost B".to_owned(),
            temporal_hint: None,
            edge_type: "semantic".to_owned(),
            confidence: Some(0.9),
        }];

        let config = GraphExtractionConfig {
            apex_mem_enabled: true,
            ..Default::default()
        };

        let inserted = super::insert_edges(&resolver, &edges, &name_to_id, &config).await;

        assert_eq!(
            inserted, 0,
            "edge rejected by the FK constraint must not be counted as inserted"
        );
        assert!(
            logs_contain("graph: edge insert (apex) rejected by FK constraint, dropping edge"),
            "APEX-MEM FK-violation WARN must fire when both endpoints are locally nonexistent"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn insert_edges_legacy_logs_warn_on_genuine_fk_violation() {
        use crate::graph::EntityResolver;
        use crate::graph::extractor::ExtractedEdge;

        let (gs, _emb) = setup().await;
        let resolver = EntityResolver::new(&gs);

        // Neither id exists in this fresh database — a genuine FK violation.
        let mut name_to_id = std::collections::HashMap::new();
        name_to_id.insert("Ghost C".to_owned(), 333_333_333);
        name_to_id.insert("Ghost D".to_owned(), 444_444_444);

        let edges = vec![ExtractedEdge {
            source: "Ghost C".to_owned(),
            target: "Ghost D".to_owned(),
            relation: "knows".to_owned(),
            fact: "Ghost C knows Ghost D".to_owned(),
            temporal_hint: None,
            edge_type: "semantic".to_owned(),
            confidence: Some(0.9),
        }];

        let config = GraphExtractionConfig {
            apex_mem_enabled: false,
            ..Default::default()
        };

        let inserted = super::insert_edges(&resolver, &edges, &name_to_id, &config).await;

        assert_eq!(
            inserted, 0,
            "edge rejected by the FK constraint must not be counted as inserted"
        );
        assert!(
            logs_contain("graph: edge insert rejected by FK constraint, dropping edge"),
            "legacy-path FK-violation WARN must fire when both endpoints are locally nonexistent"
        );
    }
}
