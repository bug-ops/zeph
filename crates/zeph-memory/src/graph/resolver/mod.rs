// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use dashmap::DashMap;
use futures::stream::{self, StreamExt as _};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;
use zeph_common::sanitize::strip_control_chars;
use zeph_common::text::truncate_to_bytes_ref;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, Role};

use super::store::GraphStore;
use super::types::EntityType;
use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::graph::extractor::ExtractedEntity;
use crate::types::MessageId;
use crate::vector_store::{FieldCondition, FieldValue, VectorFilter};

/// Minimum byte length for entity names — rejects noise tokens like "go", "cd".
const MIN_ENTITY_NAME_BYTES: usize = 3;
/// Maximum byte length for entity names stored in the graph.
const MAX_ENTITY_NAME_BYTES: usize = 512;
/// Maximum byte length for relation strings.
const MAX_RELATION_BYTES: usize = 256;
/// Maximum byte length for fact strings.
const MAX_FACT_BYTES: usize = 2048;

/// Qdrant collection for entity embeddings.
const ENTITY_COLLECTION: &str = "zeph_graph_entities";

/// Timeout for a single `embed()` call in seconds.
const EMBED_TIMEOUT_SECS: u64 = 30;

/// Outcome of an entity resolution attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolutionOutcome {
    /// Exact name+type match in `SQLite`.
    ExactMatch,
    /// Cosine similarity >= merge threshold; score is the cosine similarity value.
    EmbeddingMatch { score: f32 },
    /// LLM confirmed merge in ambiguous similarity range.
    LlmDisambiguated,
    /// New entity was created.
    Created,
}

/// LLM response for entity disambiguation.
#[derive(Debug, Deserialize, JsonSchema)]
struct DisambiguationResponse {
    same_entity: bool,
}

/// Per-entity-name lock guard to prevent concurrent duplicate creation.
///
/// Keyed by normalized entity name. Entities with different names resolve concurrently;
/// entities with the same name are serialized.
///
/// TODO(SEC-M33-02): This map grows unboundedly — one entry per unique normalized name.
/// For a short-lived resolver this is acceptable. If the resolver becomes long-lived
/// (stored in `SemanticMemory`), add eviction or use a fixed-size sharded lock array.
type NameLockMap = Arc<DashMap<String, Arc<Mutex<()>>>>;

pub struct EntityResolver<'a> {
    store: &'a GraphStore,
    embedding_store: Option<&'a Arc<EmbeddingStore>>,
    provider: Option<&'a AnyProvider>,
    similarity_threshold: f32,
    ambiguous_threshold: f32,
    name_locks: NameLockMap,
    /// Counter for error-triggered fallbacks (embed/LLM failures). Tests can read this via Arc.
    fallback_count: Arc<std::sync::atomic::AtomicU64>,
    /// Ensures `ensure_named_collection()` is called at most once per resolver lifetime.
    ///
    /// Arc-wrapped so future clones or spawned tasks can share the same gate.
    collection_ensured: Arc<tokio::sync::OnceCell<()>>,
}

impl<'a> EntityResolver<'a> {
    /// Returns a reference to the underlying graph store.
    #[must_use]
    pub fn graph_store(&self) -> &GraphStore {
        self.store
    }

    #[must_use]
    pub fn new(store: &'a GraphStore) -> Self {
        Self {
            store,
            embedding_store: None,
            provider: None,
            similarity_threshold: 0.85,
            ambiguous_threshold: 0.70,
            name_locks: Arc::new(DashMap::new()),
            fallback_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            collection_ensured: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    #[must_use]
    pub fn with_embedding_store(mut self, store: &'a Arc<EmbeddingStore>) -> Self {
        self.embedding_store = Some(store);
        self
    }

    #[must_use]
    pub fn with_provider(mut self, provider: &'a AnyProvider) -> Self {
        self.provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_thresholds(mut self, similarity: f32, ambiguous: f32) -> Self {
        self.similarity_threshold = similarity;
        self.ambiguous_threshold = ambiguous;
        self
    }

    /// Shared fallback counter — tests can clone this Arc to inspect the value.
    #[must_use]
    pub fn fallback_count(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.fallback_count)
    }

    /// Normalize an entity name: trim, lowercase, strip control chars, truncate.
    fn normalize_name(name: &str) -> String {
        let lowered = name.trim().to_lowercase();
        let cleaned = strip_control_chars(&lowered);
        let normalized = truncate_to_bytes_ref(&cleaned, MAX_ENTITY_NAME_BYTES).to_owned();
        if normalized.len() < cleaned.len() {
            tracing::debug!(
                "graph resolver: entity name truncated to {} bytes",
                MAX_ENTITY_NAME_BYTES
            );
        }
        normalized
    }

    /// Parse an entity type string, falling back to `Concept` on unknown values.
    fn parse_entity_type(entity_type: &str) -> EntityType {
        entity_type
            .trim()
            .to_lowercase()
            .parse::<EntityType>()
            .unwrap_or_else(|_| {
                tracing::debug!(
                    "graph resolver: unknown entity type {:?}, falling back to Concept",
                    entity_type
                );
                EntityType::Concept
            })
    }

    /// Acquire the per-name lock and return the guard. Keeps lock alive for the caller.
    async fn lock_name(&self, normalized: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .name_locks
            .entry(normalized.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        lock.lock_owned().await
    }

    /// Resolve an extracted entity using the alias-first canonicalization pipeline.
    ///
    /// Pipeline:
    /// 1. Normalize: trim, lowercase, strip control chars, truncate to 512 bytes.
    /// 2. Parse entity type (fallback to Concept on unknown).
    /// 3. Alias lookup: search `graph_entity_aliases` by normalized name + `entity_type`.
    ///    If found, touch `last_seen_at` and return the existing entity id.
    /// 4. Canonical name lookup: search `graph_entities` by `canonical_name` + `entity_type`.
    ///    If found, touch `last_seen_at` and return the existing entity id.
    /// 5. When `embedding_store` and `provider` are configured, performs embedding-based fuzzy
    ///    matching: cosine similarity search (Qdrant), LLM disambiguation for ambiguous range,
    ///    merge or create based on result. Failures degrade gracefully to step 6.
    /// 6. Create: upsert new entity with `canonical_name` = normalized name.
    /// 7. Register the normalized form (and original trimmed form if different) as aliases.
    ///
    /// # Errors
    ///
    /// Returns an error if the entity name is empty after normalization, or if a DB operation fails.
    pub async fn resolve(
        &self,
        name: &str,
        entity_type: &str,
        summary: Option<&str>,
    ) -> Result<(i64, ResolutionOutcome), MemoryError> {
        let normalized = Self::normalize_name(name);

        if normalized.is_empty() {
            return Err(MemoryError::GraphStore("empty entity name".into()));
        }

        if normalized.len() < MIN_ENTITY_NAME_BYTES {
            return Err(MemoryError::GraphStore(format!(
                "entity name too short: {normalized:?} ({} bytes, min {MIN_ENTITY_NAME_BYTES})",
                normalized.len()
            )));
        }

        let et = Self::parse_entity_type(entity_type);

        // The surface form preserves the original casing for user-facing display.
        let surface_name = name.trim().to_owned();

        // Acquire per-name lock to prevent concurrent duplicate creation.
        let _guard = self.lock_name(&normalized).await;

        // Step 3: alias-first lookup (filters by entity_type to prevent cross-type collisions).
        if let Some(entity) = self.store.find_entity_by_alias(&normalized, et).await? {
            self.store
                .upsert_entity(&surface_name, &entity.canonical_name, et, summary)
                .await?;
            return Ok((entity.id, ResolutionOutcome::ExactMatch));
        }

        // Step 4: canonical name lookup.
        if let Some(entity) = self.store.find_entity(&normalized, et).await? {
            self.store
                .upsert_entity(&surface_name, &entity.canonical_name, et, summary)
                .await?;
            return Ok((entity.id, ResolutionOutcome::ExactMatch));
        }

        // Step 5: Embedding-based resolution (when configured).
        if let Some(outcome) = self
            .resolve_via_embedding(&normalized, name, &surface_name, et, summary)
            .await?
        {
            return Ok(outcome);
        }

        // Step 6: Create new entity (no embedding store, or embedding failure).
        let entity_id = self
            .store
            .upsert_entity(&surface_name, &normalized, et, summary)
            .await?;

        self.register_aliases(entity_id, &normalized, name).await?;

        Ok((entity_id, ResolutionOutcome::Created))
    }

    /// Compute embedding for an entity, incrementing `fallback_count` on failure/timeout.
    /// Returns `None` when embedding is unavailable (caller should skip vector operations).
    async fn embed_entity_text(
        &self,
        provider: &AnyProvider,
        normalized: &str,
        summary: Option<&str>,
    ) -> Option<Vec<f32>> {
        let safe_summary = truncate_to_bytes_ref(summary.unwrap_or(""), MAX_FACT_BYTES);
        let embed_text = format!("{normalized}: {safe_summary}");
        let embed_result = tokio::time::timeout(
            std::time::Duration::from_secs(EMBED_TIMEOUT_SECS),
            provider.embed(&embed_text),
        )
        .await;
        match embed_result {
            Ok(Ok(v)) => Some(v),
            Ok(Err(err)) => {
                self.fallback_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(entity_name = %normalized, error = %err,
                    "embed() failed; falling back to exact-match-only entity creation");
                None
            }
            Err(_timeout) => {
                self.fallback_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(entity_name = %normalized,
                    "embed() timed out after {}s; falling back to create new entity",
                    EMBED_TIMEOUT_SECS);
                None
            }
        }
    }

    /// Handle a candidate in the ambiguous score range by running LLM disambiguation.
    /// Returns `Ok(Some(...))` if the LLM confirms a match, `Ok(None)` to fall through to create.
    #[allow(clippy::too_many_arguments)] // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
    async fn handle_ambiguous_candidate(
        &self,
        emb_store: &EmbeddingStore,
        provider: &AnyProvider,
        payload: &std::collections::HashMap<String, serde_json::Value>,
        point_id: &str,
        score: f32,
        surface_name: &str,
        normalized: &str,
        et: EntityType,
        summary: Option<&str>,
    ) -> Result<Option<(i64, ResolutionOutcome)>, MemoryError> {
        let entity_id = payload
            .get("entity_id")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| MemoryError::GraphStore("missing entity_id in payload".into()))?;
        let existing_name = payload
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let existing_summary = payload
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        // Use the existing entity's actual type from the payload (IC-S3)
        let existing_type = payload
            .get("entity_type")
            .and_then(|v| v.as_str())
            .unwrap_or(et.as_str())
            .to_owned();
        let existing_canonical = payload.get("canonical_name").and_then(|v| v.as_str());
        let existing_summary_str = payload.get("summary").and_then(|v| v.as_str());
        match self
            .llm_disambiguate(
                provider,
                normalized,
                et.as_str(),
                summary.unwrap_or(""),
                &existing_name,
                &existing_type,
                &existing_summary,
                score,
            )
            .await
        {
            Some(true) => {
                self.merge_entity(
                    emb_store,
                    provider,
                    entity_id,
                    surface_name,
                    normalized,
                    et,
                    summary,
                    existing_canonical,
                    existing_summary_str,
                    Some(point_id),
                )
                .await?;
                Ok(Some((entity_id, ResolutionOutcome::LlmDisambiguated)))
            }
            Some(false) => Ok(None),
            None => {
                self.fallback_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(entity_name = %normalized,
                    "LLM disambiguation failed; falling back to create new entity");
                Ok(None)
            }
        }
    }

    /// Attempt embedding-based resolution. Returns `Ok(Some(...))` if resolved (early return),
    /// `Ok(None)` if no match found (caller should fall through to create), or `Err` on DB error.
    async fn resolve_via_embedding(
        &self,
        normalized: &str,
        original_name: &str,
        surface_name: &str,
        et: EntityType,
        summary: Option<&str>,
    ) -> Result<Option<(i64, ResolutionOutcome)>, MemoryError> {
        let (Some(emb_store), Some(provider)) = (self.embedding_store, self.provider) else {
            return Ok(None);
        };

        let Some(query_vec) = self.embed_entity_text(provider, normalized, summary).await else {
            return Ok(None);
        };

        let type_filter = VectorFilter {
            must: vec![FieldCondition {
                field: "entity_type".into(),
                value: FieldValue::Text(et.as_str().to_owned()),
            }],
            must_not: vec![],
        };
        let candidates = match emb_store
            .search_collection(ENTITY_COLLECTION, &query_vec, 5, Some(type_filter))
            .await
        {
            Ok(c) => c,
            Err(err) => {
                self.fallback_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(entity_name = %normalized, error = %err,
                    "Qdrant search failed; falling back to create new entity");
                return self
                    .create_with_embedding(
                        emb_store,
                        surface_name,
                        normalized,
                        original_name,
                        et,
                        summary,
                        &query_vec,
                    )
                    .await
                    .map(Some);
            }
        };

        if let Some(best) = candidates.first() {
            let score = best.score;
            if score >= self.similarity_threshold {
                let entity_id = best
                    .payload
                    .get("entity_id")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| {
                        MemoryError::GraphStore("missing entity_id in payload".into())
                    })?;
                let existing_canonical =
                    best.payload.get("canonical_name").and_then(|v| v.as_str());
                let existing_summary = best.payload.get("summary").and_then(|v| v.as_str());
                let existing_pid = Some(best.id.as_str());
                self.merge_entity(
                    emb_store,
                    provider,
                    entity_id,
                    surface_name,
                    normalized,
                    et,
                    summary,
                    existing_canonical,
                    existing_summary,
                    existing_pid,
                )
                .await?;
                return Ok(Some((
                    entity_id,
                    ResolutionOutcome::EmbeddingMatch { score },
                )));
            } else if score >= self.ambiguous_threshold
                && let Some(result) = self
                    .handle_ambiguous_candidate(
                        emb_store,
                        provider,
                        &best.payload,
                        &best.id,
                        score,
                        surface_name,
                        normalized,
                        et,
                        summary,
                    )
                    .await?
            {
                return Ok(Some(result));
            }
            // score < ambiguous_threshold or LLM said different: fall through to create with embedding
        }

        // No suitable match — create new entity and store embedding.
        self.create_with_embedding(
            emb_store,
            surface_name,
            normalized,
            original_name,
            et,
            summary,
            &query_vec,
        )
        .await
        .map(Some)
    }

    /// Create a new entity, register aliases, and store its embedding in Qdrant.
    #[allow(clippy::too_many_arguments)] // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
    async fn create_with_embedding(
        &self,
        emb_store: &EmbeddingStore,
        surface_name: &str,
        normalized: &str,
        original_name: &str,
        et: EntityType,
        summary: Option<&str>,
        query_vec: &[f32],
    ) -> Result<(i64, ResolutionOutcome), MemoryError> {
        let entity_id = self
            .store
            .upsert_entity(surface_name, normalized, et, summary)
            .await?;
        self.register_aliases(entity_id, normalized, original_name)
            .await?;
        self.store_entity_embedding(
            emb_store,
            entity_id,
            None,
            normalized,
            et,
            summary.unwrap_or(""),
            query_vec,
        )
        .await;
        Ok((entity_id, ResolutionOutcome::Created))
    }

    /// Register the normalized form and original trimmed form as aliases for an entity.
    async fn register_aliases(
        &self,
        entity_id: i64,
        normalized: &str,
        original_name: &str,
    ) -> Result<(), MemoryError> {
        self.store.add_alias(entity_id, normalized).await?;

        // Also register the original trimmed lowercased form if it differs from normalized
        // (e.g. when control chars were stripped, leaving a shorter string).
        let original_trimmed = original_name.trim().to_lowercase();
        let original_clean_str = strip_control_chars(&original_trimmed);
        let original_clean = truncate_to_bytes_ref(&original_clean_str, MAX_ENTITY_NAME_BYTES);
        if original_clean != normalized {
            self.store.add_alias(entity_id, original_clean).await?;
        }

        Ok(())
    }

    /// Merge an existing entity with new information: combine summaries, update Qdrant.
    ///
    /// `existing_canonical_name` and `existing_summary` are read from the Qdrant payload at
    /// the call site (hot path, no `SQLite` roundtrip). Pass `None` for either when the point
    /// predates the payload fields — a targeted `find_entity_by_id` read is then used as a
    /// one-time fallback (legacy transition path, removed after all points are rewritten).
    #[allow(clippy::too_many_arguments)] // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
    async fn merge_entity(
        &self,
        emb_store: &EmbeddingStore,
        provider: &AnyProvider,
        entity_id: i64,
        new_surface_name: &str,
        new_canonical_name: &str,
        entity_type: EntityType,
        new_summary: Option<&str>,
        existing_canonical_name: Option<&str>,
        existing_summary_payload: Option<&str>,
        existing_point_id: Option<&str>,
    ) -> Result<(), MemoryError> {
        // Hot path: use values from the Qdrant payload (no SQLite roundtrip).
        // Both canonical_name AND summary must be present; if either is absent the point
        // predates the payload field — fall back to a targeted SQLite read to avoid
        // silently dropping a summary that was written outside merge_entity.
        let (existing_canonical, existing_summary, existing_point_id_owned) =
            if existing_canonical_name.is_some() && existing_summary_payload.is_some() {
                (
                    existing_canonical_name
                        .unwrap_or(new_canonical_name)
                        .to_owned(),
                    existing_summary_payload.unwrap_or("").to_owned(),
                    existing_point_id.map(ToOwned::to_owned),
                )
            } else {
                // Transition-period fallback. Legacy Qdrant points pre-date the payload
                // fields; one targeted read is acceptable until the embedding is rewritten
                // on the next merge. Also used when canonical_name is present but summary
                // is absent — prevents overwriting a SQLite summary with empty string.
                let existing = self.store.find_entity_by_id(entity_id).await?;
                let canonical = existing_canonical_name.map_or_else(
                    || {
                        existing.as_ref().map_or_else(
                            || new_canonical_name.to_owned(),
                            |e| e.canonical_name.clone(),
                        )
                    },
                    ToOwned::to_owned,
                );
                let summary = existing
                    .as_ref()
                    .and_then(|e| e.summary.as_deref())
                    .unwrap_or("")
                    .to_owned();
                let pid = existing_point_id.map(ToOwned::to_owned).or_else(|| {
                    existing
                        .as_ref()
                        .and_then(|e| e.qdrant_point_id.as_deref())
                        .map(ToOwned::to_owned)
                });
                (canonical, summary, pid)
            };

        let merged_summary = if let Some(new) = new_summary {
            if !new.is_empty() && !existing_summary.is_empty() {
                let combined = format!("{existing_summary}; {new}");
                // TODO(S2): use LLM-based summary merge when summary exceeds 512 bytes
                truncate_to_bytes_ref(&combined, MAX_FACT_BYTES).to_owned()
            } else if !new.is_empty() {
                new.to_owned()
            } else {
                existing_summary.clone()
            }
        } else {
            existing_summary.clone()
        };

        let summary_opt = if merged_summary.is_empty() {
            None
        } else {
            Some(merged_summary.as_str())
        };

        // Preserve the existing display name from the payload; fall back to the incoming surface
        // name only for brand-new entities (where the payload had no "name" field).
        self.store
            .upsert_entity(
                new_surface_name,
                &existing_canonical,
                entity_type,
                summary_opt,
            )
            .await?;

        // Re-embed merged text and upsert to Qdrant
        let embed_text = format!("{new_surface_name}: {merged_summary}");
        let embed_result = tokio::time::timeout(
            std::time::Duration::from_secs(EMBED_TIMEOUT_SECS),
            provider.embed(&embed_text),
        )
        .await;

        match embed_result {
            Ok(Ok(vec)) => {
                self.store_entity_embedding(
                    emb_store,
                    entity_id,
                    existing_point_id_owned.as_deref(),
                    new_surface_name,
                    entity_type,
                    &merged_summary,
                    &vec,
                )
                .await;
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    entity_id,
                    error = %err,
                    "merge re-embed failed; Qdrant entry may be stale"
                );
            }
            Err(_) => {
                tracing::warn!(
                    entity_id,
                    "merge re-embed timed out; Qdrant entry may be stale"
                );
            }
        }

        Ok(())
    }

    /// Store an entity embedding in Qdrant and update `qdrant_point_id` in `SQLite`.
    ///
    /// When `existing_point_id` is `Some`, the existing Qdrant point is updated in-place
    /// (upsert by ID) to avoid orphaned stale points. When `None`, a new point is created.
    ///
    /// Failures are logged at warn level but do not propagate — the entity is still
    /// valid in `SQLite` even if Qdrant upsert fails.
    #[allow(clippy::too_many_arguments)] // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
    async fn store_entity_embedding(
        &self,
        emb_store: &EmbeddingStore,
        entity_id: i64,
        existing_point_id: Option<&str>,
        name: &str,
        entity_type: EntityType,
        summary: &str,
        vector: &[f32],
    ) {
        // Ensure the Qdrant collection exists exactly once per resolver lifetime.
        // All entity embeddings use the same model dimension, so the first call's
        // vector_size wins — subsequent entities share the same collection.
        // On error the cell stays unset, so the next entity retries — transient
        // network failures do not permanently disable embedding storage.
        let vector_size = u64::try_from(vector.len()).unwrap_or(384);
        let collection_ensured = Arc::clone(&self.collection_ensured);
        if let Err(err) = collection_ensured
            .get_or_try_init(|| async {
                emb_store
                    .ensure_named_collection(ENTITY_COLLECTION, vector_size)
                    .await
            })
            .await
        {
            tracing::error!(
                error = %err,
                "failed to ensure entity embedding collection; skipping Qdrant upsert"
            );
            return;
        }

        let payload = serde_json::json!({
            "entity_id": entity_id,
            // String mirror of entity_id for scroll_all enumeration (scroll_all only surfaces
            // StringValue payload fields; the i64 entity_id is preserved for existing search
            // consumers which read it directly from ScoredVectorPoint.payload).
            "entity_id_str": entity_id.to_string(),
            "canonical_name": name,
            "name": name,
            "entity_type": entity_type.as_str(),
            "summary": summary,
        });

        if let Some(point_id) = existing_point_id {
            // Reuse existing point to avoid orphaned stale points (IC-S1)
            if let Err(err) = emb_store
                .upsert_to_collection(ENTITY_COLLECTION, point_id, payload, vector.to_vec())
                .await
            {
                tracing::warn!(
                    entity_id,
                    error = %err,
                    "Qdrant upsert (existing point) failed; Qdrant entry may be stale"
                );
            }
        } else {
            match emb_store
                .store_to_collection(ENTITY_COLLECTION, payload, vector.to_vec())
                .await
            {
                Ok(point_id) => {
                    if let Err(err) = self
                        .store
                        .set_entity_qdrant_point_id(entity_id, &point_id)
                        .await
                    {
                        tracing::warn!(
                            entity_id,
                            error = %err,
                            "failed to store qdrant_point_id in SQLite"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        entity_id,
                        error = %err,
                        "Qdrant upsert failed; entity created in SQLite, qdrant_point_id remains NULL"
                    );
                }
            }
        }
    }

    /// Ask the LLM whether two entities are the same.
    ///
    /// Returns `Some(true)` for merge, `Some(false)` for separate, `None` on failure.
    #[allow(clippy::too_many_arguments)] // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
    async fn llm_disambiguate(
        &self,
        provider: &AnyProvider,
        new_name: &str,
        new_type: &str,
        new_summary: &str,
        existing_name: &str,
        existing_type: &str,
        existing_summary: &str,
        score: f32,
    ) -> Option<bool> {
        let prompt = format!(
            "New entity:\n\
             - Name: <external-data>{new_name}</external-data>\n\
             - Type: <external-data>{new_type}</external-data>\n\
             - Summary: <external-data>{new_summary}</external-data>\n\
             \n\
             Existing entity:\n\
             - Name: <external-data>{existing_name}</external-data>\n\
             - Type: <external-data>{existing_type}</external-data>\n\
             - Summary: <external-data>{existing_summary}</external-data>\n\
             \n\
             Cosine similarity: {score:.2}\n\
             \n\
             Are these the same entity? Respond with JSON: {{\"same_entity\": true}} or {{\"same_entity\": false}}"
        );

        let messages = [
            Message::from_legacy(
                Role::System,
                "You are an entity disambiguation assistant. Given a new entity mention and \
                 an existing entity from the knowledge graph, determine if they refer to the same \
                 real-world entity. Respond only with JSON.",
            ),
            Message::from_legacy(Role::User, prompt),
        ];

        let response = match provider.chat(&messages).await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, "LLM disambiguation chat failed");
                return None;
            }
        };

        // Parse JSON response, tolerating markdown code fences
        let json_str = extract_json(&response);
        match serde_json::from_str::<DisambiguationResponse>(json_str) {
            Ok(parsed) => Some(parsed.same_entity),
            Err(err) => {
                tracing::warn!(error = %err, response = %response, "failed to parse LLM disambiguation response");
                None
            }
        }
    }

    /// Resolve a batch of extracted entities concurrently.
    ///
    /// Returns a `Vec` of `(entity_id, ResolutionOutcome)` in the same order as input.
    ///
    /// # Errors
    ///
    /// Returns an error if any DB operation fails.
    ///
    /// # Panics
    ///
    /// Panics if an internal stream collection bug causes a result index to be missing.
    /// This indicates a programming error and should never occur in correct usage.
    pub async fn resolve_batch(
        &self,
        entities: &[ExtractedEntity],
    ) -> Result<Vec<(i64, ResolutionOutcome)>, MemoryError> {
        if entities.is_empty() {
            return Ok(Vec::new());
        }

        // Process up to 4 embed+resolve operations concurrently (IC-S2/PERF-01).
        let mut results: Vec<Option<(i64, ResolutionOutcome)>> = vec![None; entities.len()];

        let mut stream = stream::iter(entities.iter().enumerate().map(|(i, e)| {
            let name = e.name.clone();
            let entity_type = e.entity_type.clone();
            let summary = e.summary.clone();
            async move {
                let result = self.resolve(&name, &entity_type, summary.as_deref()).await;
                (i, result)
            }
        }))
        .buffer_unordered(4);

        while let Some((i, result)) = stream.next().await {
            match result {
                Ok(outcome) => results[i] = Some(outcome),
                Err(err) => return Err(err),
            }
        }

        Ok(results
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                r.unwrap_or_else(|| {
                    tracing::warn!(
                        index = i,
                        "resolve_batch: missing result at index — bug in stream collection"
                    );
                    panic!("resolve_batch: missing result at index {i}")
                })
            })
            .collect())
    }

    /// Resolve an extracted edge: deduplicate or supersede existing edges.
    ///
    /// - If an active edge with the same direction and relation exists with an identical fact,
    ///   returns `None` (deduplicated).
    /// - If an active edge with the same direction and relation exists with a different fact,
    ///   invalidates the old edge and inserts the new one, returning `Some(new_id)`.
    /// - If no matching edge exists, inserts a new edge and returns `Some(new_id)`.
    ///
    /// Relation and fact strings are sanitized (control chars stripped, length-capped).
    ///
    /// # Errors
    ///
    /// Returns an error if any database operation fails.
    pub async fn resolve_edge(
        &self,
        source_id: i64,
        target_id: i64,
        relation: &str,
        fact: &str,
        confidence: f32,
        episode_id: Option<MessageId>,
    ) -> Result<Option<i64>, MemoryError> {
        let relation_clean = strip_control_chars(&relation.trim().to_lowercase());
        let normalized_relation =
            truncate_to_bytes_ref(&relation_clean, MAX_RELATION_BYTES).to_owned();

        let fact_clean = strip_control_chars(fact.trim());
        let normalized_fact = truncate_to_bytes_ref(&fact_clean, MAX_FACT_BYTES).to_owned();

        // Fetch only exact-direction edges — no reverse edges to filter out
        let existing_edges = self.store.edges_exact(source_id, target_id).await?;

        let matching = existing_edges
            .iter()
            .find(|e| e.relation == normalized_relation);

        if let Some(old) = matching {
            if old.fact == normalized_fact {
                // Exact duplicate — skip
                return Ok(None);
            }
            // Same relation, different fact — supersede
            self.store.invalidate_edge(old.id).await?;
        }

        let new_id = self
            .store
            .insert_edge(
                source_id,
                target_id,
                &normalized_relation,
                &normalized_fact,
                confidence,
                episode_id,
            )
            .await?;
        Ok(Some(new_id))
    }

    /// Resolve a typed edge: deduplicate or supersede existing edges of the same type.
    ///
    /// Identical to [`Self::resolve_edge`] but includes `edge_type` in the matching key.
    /// An active edge with the same `(source, target, relation, edge_type)` and identical
    /// fact returns `None`; same relation+type with different fact is superseded.
    ///
    /// When `belief_revision` is `Some`, uses semantic contradiction detection to find edges
    /// to supersede across the same relation domain. The new fact embedding is pre-computed
    /// here (one embed call) to avoid N+1 embedding calls.
    ///
    /// This ensures that different MAGMA edge types for the same entity pair are stored
    /// independently (critic mitigation: dedup key includes `edge_type`).
    ///
    /// # Errors
    ///
    /// Returns an error if any database operation fails.
    #[allow(clippy::too_many_arguments)] // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
    pub async fn resolve_edge_typed(
        &self,
        source_id: i64,
        target_id: i64,
        relation: &str,
        fact: &str,
        confidence: f32,
        episode_id: Option<crate::types::MessageId>,
        edge_type: crate::graph::EdgeType,
        belief_revision: Option<&crate::graph::BeliefRevisionConfig>,
    ) -> Result<Option<i64>, MemoryError> {
        let relation_clean = strip_control_chars(&relation.trim().to_lowercase());
        let normalized_relation =
            truncate_to_bytes_ref(&relation_clean, MAX_RELATION_BYTES).to_owned();

        let fact_clean = strip_control_chars(fact.trim());
        let normalized_fact = truncate_to_bytes_ref(&fact_clean, MAX_FACT_BYTES).to_owned();

        let existing_edges = self.store.edges_exact(source_id, target_id).await?;

        // Exact dedup: same (relation, edge_type, fact) → skip.
        let matching = existing_edges
            .iter()
            .find(|e| e.relation == normalized_relation && e.edge_type == edge_type);

        if matching.is_some_and(|old| old.fact == normalized_fact) {
            return Ok(None);
        }

        // Determine which edges to supersede.
        let superseded_ids: Vec<i64> = if let (Some(cfg), Some(provider)) =
            (belief_revision, self.provider)
        {
            // Kumiho belief revision: embed new fact once, find semantically contradicted edges.
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                provider.embed(&normalized_fact),
            )
            .await
            {
                Ok(Ok(new_emb)) => {
                    match crate::graph::belief_revision::find_superseded_edges(
                        &existing_edges,
                        &new_emb,
                        &normalized_relation,
                        edge_type,
                        provider,
                        cfg,
                    )
                    .await
                    {
                        Ok(ids) => ids,
                        Err(err) => {
                            tracing::warn!(error = %err,
                                    "belief_revision: find_superseded_edges failed, falling back to exact match");
                            matching.map(|e| vec![e.id]).unwrap_or_default()
                        }
                    }
                }
                Ok(Err(err)) => {
                    tracing::warn!(error = %err,
                            "belief_revision: embed new fact failed, falling back to exact match");
                    matching.map(|e| vec![e.id]).unwrap_or_default()
                }
                Err(_) => {
                    tracing::warn!(
                        "belief_revision: embed new fact timed out, falling back to exact match"
                    );
                    matching.map(|e| vec![e.id]).unwrap_or_default()
                }
            }
        } else {
            // Legacy: exact (relation, edge_type) match with different fact.
            matching.map(|e| vec![e.id]).unwrap_or_default()
        };

        let new_id = self
            .store
            .insert_edge_typed(
                source_id,
                target_id,
                &normalized_relation,
                &normalized_fact,
                confidence,
                episode_id,
                edge_type,
            )
            .await?;

        // Supersede old edges with audit trail (belief revision) or plain invalidation (legacy).
        for old_id in superseded_ids {
            if belief_revision.is_some() {
                self.store
                    .invalidate_edge_with_supersession(old_id, new_id)
                    .await?;
            } else {
                self.store.invalidate_edge(old_id).await?;
            }
        }

        Ok(Some(new_id))
    }
}

/// Extract a JSON object from a string that may contain markdown code fences.
fn extract_json(s: &str) -> &str {
    let trimmed = s.trim();
    // Strip ```json ... ``` or ``` ... ```
    if let Some(inner) = trimmed.strip_prefix("```json")
        && let Some(end) = inner.rfind("```")
    {
        return inner[..end].trim();
    }
    if let Some(inner) = trimmed.strip_prefix("```")
        && let Some(end) = inner.rfind("```")
    {
        return inner[..end].trim();
    }
    // Find first '{' to last '}'
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}'))
        && start <= end
    {
        return &trimmed[start..=end];
    }
    trimmed
}

#[cfg(test)]
mod tests;
