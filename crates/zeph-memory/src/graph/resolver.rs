// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use dashmap::DashMap;
use futures::stream::{self, StreamExt as _};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, Role};

use super::store::GraphStore;
use super::types::EntityType;
use crate::embedding_store::EmbeddingStore;
use crate::error::MemoryError;
use crate::graph::extractor::ExtractedEntity;
use crate::types::MessageId;
use crate::vector_store::{FieldCondition, FieldValue, VectorFilter};

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

/// Strip ASCII control characters and Unicode `BiDi` override codepoints.
fn strip_control_chars(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && !matches!(*c as u32, 0x202A..=0x202E | 0x2066..=0x2069))
        .collect()
}

/// Truncate a string to at most `max_bytes` bytes at a valid UTF-8 char boundary.
fn truncate_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut boundary = max_bytes;
    while !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}

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
}

impl<'a> EntityResolver<'a> {
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
        let normalized = truncate_to_bytes(&cleaned, MAX_ENTITY_NAME_BYTES).to_owned();
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
        let safe_summary = truncate_to_bytes(summary.unwrap_or(""), MAX_FACT_BYTES);
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
    #[allow(clippy::too_many_arguments)]
    async fn handle_ambiguous_candidate(
        &self,
        emb_store: &EmbeddingStore,
        provider: &AnyProvider,
        payload: &std::collections::HashMap<String, serde_json::Value>,
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
                self.merge_entity(
                    emb_store,
                    provider,
                    entity_id,
                    surface_name,
                    normalized,
                    et,
                    summary,
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
    #[allow(clippy::too_many_arguments)]
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
        let original_clean = truncate_to_bytes(&original_clean_str, MAX_ENTITY_NAME_BYTES);
        if original_clean != normalized {
            self.store.add_alias(entity_id, original_clean).await?;
        }

        Ok(())
    }

    /// Merge an existing entity with new information: combine summaries, update Qdrant.
    #[allow(clippy::too_many_arguments)]
    async fn merge_entity(
        &self,
        emb_store: &EmbeddingStore,
        provider: &AnyProvider,
        entity_id: i64,
        new_surface_name: &str,
        new_canonical_name: &str,
        entity_type: EntityType,
        new_summary: Option<&str>,
    ) -> Result<(), MemoryError> {
        // TODO(PERF-03): The Qdrant payload already contains name/summary at the call site;
        // pass them in as parameters to eliminate this extra SQLite roundtrip per merge.
        let existing = self.store.find_entity_by_id(entity_id).await?;
        let existing_summary = existing
            .as_ref()
            .and_then(|e| e.summary.as_deref())
            .unwrap_or("");

        let merged_summary = if let Some(new) = new_summary {
            if !new.is_empty() && !existing_summary.is_empty() {
                let combined = format!("{existing_summary}; {new}");
                // TODO(S2): use LLM-based summary merge when summary exceeds 512 bytes
                truncate_to_bytes(&combined, MAX_FACT_BYTES).to_owned()
            } else if !new.is_empty() {
                new.to_owned()
            } else {
                existing_summary.to_owned()
            }
        } else {
            existing_summary.to_owned()
        };

        let summary_opt = if merged_summary.is_empty() {
            None
        } else {
            Some(merged_summary.as_str())
        };

        // Update the EXISTING entity's summary (keep its canonical_name, update surface display name).
        let existing_canonical = existing.as_ref().map_or_else(
            || new_canonical_name.to_owned(),
            |e| e.canonical_name.clone(),
        );
        let existing_name_owned = existing
            .as_ref()
            .map_or_else(|| new_surface_name.to_owned(), |e| e.name.clone());
        self.store
            .upsert_entity(
                &existing_name_owned,
                &existing_canonical,
                entity_type,
                summary_opt,
            )
            .await?;

        // Retrieve existing qdrant_point_id to reuse it (avoids orphaned stale points, IC-S1)
        let existing_point_id = existing
            .as_ref()
            .and_then(|e| e.qdrant_point_id.as_deref())
            .map(ToOwned::to_owned);

        // Re-embed merged text and upsert to Qdrant
        let embed_text = format!("{existing_name_owned}: {merged_summary}");
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
                    existing_point_id.as_deref(),
                    &existing_name_owned,
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
    #[allow(clippy::too_many_arguments)]
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
        // TODO(PERF-05): ensure_named_collection() is called on every store_entity_embedding()
        // invocation, generating one Qdrant network roundtrip per entity in a batch. Cache this
        // result at resolver construction time via `std::sync::OnceLock<bool>` to call it once.
        let vector_size = u64::try_from(vector.len()).unwrap_or(384);
        if let Err(err) = emb_store
            .ensure_named_collection(ENTITY_COLLECTION, vector_size)
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
    #[allow(clippy::too_many_arguments)]
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
        let normalized_relation = truncate_to_bytes(&relation_clean, MAX_RELATION_BYTES).to_owned();

        let fact_clean = strip_control_chars(fact.trim());
        let normalized_fact = truncate_to_bytes(&fact_clean, MAX_FACT_BYTES).to_owned();

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
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::in_memory_store::InMemoryVectorStore;
    use crate::sqlite::SqliteStore;

    async fn setup() -> GraphStore {
        let store = SqliteStore::new(":memory:").await.unwrap();
        GraphStore::new(store.pool().clone())
    }

    async fn setup_with_embedding() -> (GraphStore, Arc<EmbeddingStore>) {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();
        let mem_store = Box::new(InMemoryVectorStore::new());
        let emb = Arc::new(EmbeddingStore::with_store(mem_store, pool));
        let gs = GraphStore::new(sqlite.pool().clone());
        (gs, emb)
    }

    fn make_mock_provider_with_embedding(embedding: Vec<f32>) -> zeph_llm::mock::MockProvider {
        let mut p = zeph_llm::mock::MockProvider::default();
        p.embedding = embedding;
        p.supports_embeddings = true;
        p
    }

    // ── Existing tests (resolve() with no embedding store — exact match only) ──

    #[tokio::test]
    async fn resolve_creates_new_entity() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let (id, outcome) = resolver
            .resolve("alice", "person", Some("a person"))
            .await
            .unwrap();
        assert!(id > 0);
        assert_eq!(outcome, ResolutionOutcome::Created);
    }

    #[tokio::test]
    async fn resolve_updates_existing_entity() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let (id1, _) = resolver.resolve("alice", "person", None).await.unwrap();
        let (id2, outcome) = resolver
            .resolve("alice", "person", Some("updated summary"))
            .await
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(outcome, ResolutionOutcome::ExactMatch);

        let entity = gs
            .find_entity("alice", EntityType::Person)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.summary.as_deref(), Some("updated summary"));
    }

    #[tokio::test]
    async fn resolve_unknown_type_falls_back_to_concept() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let (id, _) = resolver
            .resolve("my_thing", "unknown_type", None)
            .await
            .unwrap();
        assert!(id > 0);

        // Verify it was stored as Concept
        let entity = gs
            .find_entity("my_thing", EntityType::Concept)
            .await
            .unwrap();
        assert!(entity.is_some());
    }

    #[tokio::test]
    async fn resolve_empty_name_returns_error() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let result_empty = resolver.resolve("", "concept", None).await;
        assert!(result_empty.is_err());
        assert!(matches!(
            result_empty.unwrap_err(),
            MemoryError::GraphStore(_)
        ));

        let result_whitespace = resolver.resolve("   ", "concept", None).await;
        assert!(result_whitespace.is_err());
    }

    #[tokio::test]
    async fn resolve_case_insensitive() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let (id1, _) = resolver.resolve("Rust", "language", None).await.unwrap();
        let (id2, outcome) = resolver.resolve("rust", "language", None).await.unwrap();
        assert_eq!(
            id1, id2,
            "'Rust' and 'rust' should resolve to the same entity"
        );
        assert_eq!(outcome, ResolutionOutcome::ExactMatch);
    }

    #[tokio::test]
    async fn resolve_edge_inserts_new() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let src = gs
            .upsert_entity("src", "src", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("tgt", "tgt", EntityType::Concept, None)
            .await
            .unwrap();

        let result = resolver
            .resolve_edge(src, tgt, "uses", "src uses tgt", 0.9, None)
            .await
            .unwrap();
        assert!(result.is_some());
        assert!(result.unwrap() > 0);
    }

    #[tokio::test]
    async fn resolve_edge_deduplicates_identical() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let src = gs
            .upsert_entity("a", "a", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("b", "b", EntityType::Concept, None)
            .await
            .unwrap();

        let first = resolver
            .resolve_edge(src, tgt, "uses", "a uses b", 0.9, None)
            .await
            .unwrap();
        assert!(first.is_some());

        let second = resolver
            .resolve_edge(src, tgt, "uses", "a uses b", 0.9, None)
            .await
            .unwrap();
        assert!(second.is_none(), "identical edge should be deduplicated");
    }

    #[tokio::test]
    async fn resolve_edge_supersedes_contradictory() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let src = gs
            .upsert_entity("x", "x", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("y", "y", EntityType::Concept, None)
            .await
            .unwrap();

        let first_id = resolver
            .resolve_edge(src, tgt, "prefers", "x prefers y (old)", 0.8, None)
            .await
            .unwrap()
            .unwrap();

        let second_id = resolver
            .resolve_edge(src, tgt, "prefers", "x prefers y (new)", 0.9, None)
            .await
            .unwrap()
            .unwrap();

        assert_ne!(first_id, second_id, "superseded edge should have a new ID");

        // Old edge should be invalidated
        let active_count = gs.active_edge_count().await.unwrap();
        assert_eq!(active_count, 1, "only new edge should be active");
    }

    #[tokio::test]
    async fn resolve_edge_direction_sensitive() {
        // A->B "uses" should not interfere with B->A "uses" dedup
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let a = gs
            .upsert_entity("node_a", "node_a", EntityType::Concept, None)
            .await
            .unwrap();
        let b = gs
            .upsert_entity("node_b", "node_b", EntityType::Concept, None)
            .await
            .unwrap();

        // Insert A->B
        let id1 = resolver
            .resolve_edge(a, b, "uses", "A uses B", 0.9, None)
            .await
            .unwrap();
        assert!(id1.is_some());

        // Insert B->A with different fact — should NOT invalidate A->B (different direction)
        let id2 = resolver
            .resolve_edge(b, a, "uses", "B uses A (different direction)", 0.9, None)
            .await
            .unwrap();
        assert!(id2.is_some());

        // Both edges should still be active
        let active_count = gs.active_edge_count().await.unwrap();
        assert_eq!(active_count, 2, "both directional edges should be active");
    }

    #[tokio::test]
    async fn resolve_edge_normalizes_relation_case() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let src = gs
            .upsert_entity("p", "p", EntityType::Concept, None)
            .await
            .unwrap();
        let tgt = gs
            .upsert_entity("q", "q", EntityType::Concept, None)
            .await
            .unwrap();

        // Insert with uppercase relation
        let id1 = resolver
            .resolve_edge(src, tgt, "Uses", "p uses q", 0.9, None)
            .await
            .unwrap();
        assert!(id1.is_some());

        // Insert with lowercase relation — same normalized relation, same fact → deduplicate
        let id2 = resolver
            .resolve_edge(src, tgt, "uses", "p uses q", 0.9, None)
            .await
            .unwrap();
        assert!(id2.is_none(), "normalized relations should deduplicate");
    }

    // ── IC-01: entity_type lowercased before parse ────────────────────────────

    #[tokio::test]
    async fn resolve_entity_type_uppercase_parsed_correctly() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // "Person" (title case from LLM) should parse as EntityType::Person, not fall back to Concept
        let (id, _) = resolver
            .resolve("test_entity", "Person", None)
            .await
            .unwrap();
        assert!(id > 0);

        let entity = gs
            .find_entity("test_entity", EntityType::Person)
            .await
            .unwrap();
        assert!(entity.is_some(), "entity should be stored as Person type");
    }

    #[tokio::test]
    async fn resolve_entity_type_all_caps_parsed_correctly() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let (id, _) = resolver.resolve("my_lang", "LANGUAGE", None).await.unwrap();
        assert!(id > 0);

        let entity = gs
            .find_entity("my_lang", EntityType::Language)
            .await
            .unwrap();
        assert!(entity.is_some(), "entity should be stored as Language type");
    }

    // ── SEC-GRAPH-01: entity name length cap ──────────────────────────────────

    #[tokio::test]
    async fn resolve_truncates_long_entity_name() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let long_name = "a".repeat(1024);
        let (id, _) = resolver.resolve(&long_name, "concept", None).await.unwrap();
        assert!(id > 0);

        // Entity should exist with a truncated name (512 bytes)
        let entity = gs
            .find_entity(&"a".repeat(512), EntityType::Concept)
            .await
            .unwrap();
        assert!(entity.is_some(), "truncated name should be stored");
    }

    // ── SEC-GRAPH-02: control character stripping ─────────────────────────────

    #[tokio::test]
    async fn resolve_strips_control_chars_from_name() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // Name with null byte and a BiDi override
        let name_with_ctrl = "rust\x00lang";
        let (id, _) = resolver
            .resolve(name_with_ctrl, "language", None)
            .await
            .unwrap();
        assert!(id > 0);

        // Stored name should have control chars removed
        let entity = gs
            .find_entity("rustlang", EntityType::Language)
            .await
            .unwrap();
        assert!(
            entity.is_some(),
            "control chars should be stripped from stored name"
        );
    }

    #[tokio::test]
    async fn resolve_strips_bidi_overrides_from_name() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // U+202E is RIGHT-TO-LEFT OVERRIDE — a BiDi spoof character
        let name_with_bidi = "rust\u{202E}lang";
        let (id, _) = resolver
            .resolve(name_with_bidi, "language", None)
            .await
            .unwrap();
        assert!(id > 0);

        let entity = gs
            .find_entity("rustlang", EntityType::Language)
            .await
            .unwrap();
        assert!(entity.is_some(), "BiDi override chars should be stripped");
    }

    // ── Helper unit tests for sanitization functions ──────────────────────────

    #[test]
    fn strip_control_chars_removes_ascii_controls() {
        assert_eq!(strip_control_chars("hello\x00world"), "helloworld");
        assert_eq!(strip_control_chars("tab\there"), "tabhere");
        assert_eq!(strip_control_chars("new\nline"), "newline");
    }

    #[test]
    fn strip_control_chars_removes_bidi() {
        let bidi = "\u{202E}spoof";
        assert_eq!(strip_control_chars(bidi), "spoof");
    }

    #[test]
    fn strip_control_chars_preserves_normal_unicode() {
        assert_eq!(strip_control_chars("привет мир"), "привет мир");
        assert_eq!(strip_control_chars("日本語"), "日本語");
    }

    #[test]
    fn truncate_to_bytes_exact_boundary() {
        let s = "hello";
        assert_eq!(truncate_to_bytes(s, 5), "hello");
        assert_eq!(truncate_to_bytes(s, 3), "hel");
    }

    #[test]
    fn truncate_to_bytes_respects_utf8_boundary() {
        // "é" is 2 bytes in UTF-8 — truncating at 1 byte should give ""
        let s = "élan";
        let truncated = truncate_to_bytes(s, 1);
        assert!(s.is_char_boundary(truncated.len()));
    }

    // ── New tests: embedding-based resolution ─────────────────────────────────

    #[tokio::test]
    async fn resolve_with_embedding_store_score_above_threshold_merges() {
        let (gs, emb) = setup_with_embedding().await;
        // Pre-insert an existing entity (different name to avoid exact match).
        // "python programming lang" is in Qdrant; we resolve "python scripting lang"
        // which embeds to the identical vector → cosine similarity = 1.0 > 0.85 → merge.
        let existing_id = gs
            .upsert_entity(
                "python programming lang",
                "python programming lang",
                EntityType::Language,
                Some("a programming language"),
            )
            .await
            .unwrap();

        let mock_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
        emb.ensure_named_collection(ENTITY_COLLECTION, 4)
            .await
            .unwrap();
        let payload = serde_json::json!({
            "entity_id": existing_id,
            "name": "python programming lang",
            "entity_type": "language",
            "summary": "a programming language",
        });
        let point_id = emb
            .store_to_collection(ENTITY_COLLECTION, payload, mock_vec.clone())
            .await
            .unwrap();
        gs.set_entity_qdrant_point_id(existing_id, &point_id)
            .await
            .unwrap();

        // Mock provider returns the same vector for any text → cosine similarity = 1.0 > 0.85
        let provider = make_mock_provider_with_embedding(mock_vec);
        let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider)
            .with_thresholds(0.85, 0.70);

        // Resolve a different name — no exact match, embedding match wins
        let (id, outcome) = resolver
            .resolve(
                "python scripting lang",
                "language",
                Some("scripting language"),
            )
            .await
            .unwrap();

        assert_eq!(id, existing_id, "should return existing entity ID on merge");
        assert!(
            matches!(outcome, ResolutionOutcome::EmbeddingMatch { score } if score > 0.85),
            "outcome should be EmbeddingMatch with score > 0.85, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn resolve_with_embedding_store_score_below_ambiguous_creates_new() {
        let (gs, emb) = setup_with_embedding().await;
        // Insert existing entity with orthogonal vector
        let existing_id = gs
            .upsert_entity("java", "java", EntityType::Language, Some("java language"))
            .await
            .unwrap();

        // Existing uses [1,0,0,0]; new entity will embed to [0,1,0,0] (orthogonal, score=0)
        emb.ensure_named_collection(ENTITY_COLLECTION, 4)
            .await
            .unwrap();
        let payload = serde_json::json!({
            "entity_id": existing_id,
            "name": "java",
            "entity_type": "language",
            "summary": "java language",
        });
        emb.store_to_collection(ENTITY_COLLECTION, payload, vec![1.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();

        // Mock returns orthogonal vector → score = 0.0 < 0.70
        let provider = make_mock_provider_with_embedding(vec![0.0, 1.0, 0.0, 0.0]);
        let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider)
            .with_thresholds(0.85, 0.70);

        let (id, outcome) = resolver
            .resolve("kotlin", "language", Some("kotlin language"))
            .await
            .unwrap();

        assert_ne!(id, existing_id, "orthogonal entity should create new");
        assert_eq!(outcome, ResolutionOutcome::Created);
    }

    #[tokio::test]
    async fn resolve_with_embedding_failure_falls_back_to_create() {
        // Use a mock with supports_embeddings=false — embed() returns EmbedUnsupported error,
        // which triggers the fallback path (create new entity).
        let sqlite2 = SqliteStore::new(":memory:").await.unwrap();
        let pool2 = sqlite2.pool().clone();
        let mem2 = Box::new(InMemoryVectorStore::new());
        let emb2 = Arc::new(EmbeddingStore::with_store(mem2, pool2));
        let gs2 = GraphStore::new(sqlite2.pool().clone());

        let mut mock = zeph_llm::mock::MockProvider::default();
        mock.supports_embeddings = false;
        let any_provider = zeph_llm::any::AnyProvider::Mock(mock);

        let resolver = EntityResolver::new(&gs2)
            .with_embedding_store(&emb2)
            .with_provider(&any_provider);

        let (id, outcome) = resolver
            .resolve("testentity", "concept", Some("summary"))
            .await
            .unwrap();
        assert!(id > 0);
        assert_eq!(outcome, ResolutionOutcome::Created);
    }

    #[tokio::test]
    async fn resolve_fallback_increments_counter() {
        let (gs, emb) = setup_with_embedding().await;

        // Provider with embed that fails (supports_embeddings=false → EmbedUnsupported error)
        let mut mock = zeph_llm::mock::MockProvider::default();
        mock.supports_embeddings = false;
        let any_provider = zeph_llm::any::AnyProvider::Mock(mock);

        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider);

        let fallback_count = resolver.fallback_count();

        // First call: embed fails → fallback
        resolver.resolve("entity_a", "concept", None).await.unwrap();

        assert_eq!(
            fallback_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "fallback counter should be 1 after embed failure"
        );
    }

    #[tokio::test]
    async fn resolve_batch_processes_multiple_entities() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let entities = vec![
            ExtractedEntity {
                name: "rust".into(),
                entity_type: "language".into(),
                summary: Some("systems language".into()),
            },
            ExtractedEntity {
                name: "python".into(),
                entity_type: "language".into(),
                summary: None,
            },
            ExtractedEntity {
                name: "cargo".into(),
                entity_type: "tool".into(),
                summary: Some("rust build tool".into()),
            },
        ];

        let results = resolver.resolve_batch(&entities).await.unwrap();
        assert_eq!(results.len(), 3);
        for (id, outcome) in &results {
            assert!(*id > 0);
            assert_eq!(*outcome, ResolutionOutcome::Created);
        }
    }

    #[tokio::test]
    async fn resolve_batch_empty_returns_empty() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let results = resolver.resolve_batch(&[]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn merge_combines_summaries() {
        let (gs, emb) = setup_with_embedding().await;
        // Use a different name for the existing entity so exact match doesn't trigger.
        // "mergetest v1" is stored with embedding; we then resolve "mergetest v2" which
        // embeds to the same vector → similarity = 1.0 > threshold → merge.
        let existing_id = gs
            .upsert_entity(
                "mergetest v1",
                "mergetest v1",
                EntityType::Concept,
                Some("first summary"),
            )
            .await
            .unwrap();

        let mock_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
        emb.ensure_named_collection(ENTITY_COLLECTION, 4)
            .await
            .unwrap();
        let payload = serde_json::json!({
            "entity_id": existing_id,
            "name": "mergetest v1",
            "entity_type": "concept",
            "summary": "first summary",
        });
        let point_id = emb
            .store_to_collection(ENTITY_COLLECTION, payload, mock_vec.clone())
            .await
            .unwrap();
        gs.set_entity_qdrant_point_id(existing_id, &point_id)
            .await
            .unwrap();

        let provider = make_mock_provider_with_embedding(mock_vec);
        let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider)
            .with_thresholds(0.85, 0.70);

        // Resolve "mergetest v2" — no exact match, but embedding is identical → merge
        let (id, outcome) = resolver
            .resolve("mergetest v2", "concept", Some("second summary"))
            .await
            .unwrap();

        assert_eq!(id, existing_id);
        assert!(matches!(outcome, ResolutionOutcome::EmbeddingMatch { .. }));

        // Verify the merged summary was updated on the existing entity
        let entity = gs
            .find_entity("mergetest v1", EntityType::Concept)
            .await
            .unwrap()
            .unwrap();
        let summary = entity.summary.unwrap_or_default();
        assert!(
            summary.contains("first summary") && summary.contains("second summary"),
            "merged summary should contain both: got {summary:?}"
        );
    }

    #[tokio::test]
    async fn merge_preserves_older_entity_id() {
        let (gs, emb) = setup_with_embedding().await;
        // "legacy entity" stored with embedding; "legacy entity variant" has same vector → merge
        let existing_id = gs
            .upsert_entity(
                "legacy entity",
                "legacy entity",
                EntityType::Concept,
                Some("old info"),
            )
            .await
            .unwrap();

        let mock_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
        emb.ensure_named_collection(ENTITY_COLLECTION, 4)
            .await
            .unwrap();
        let payload = serde_json::json!({
            "entity_id": existing_id,
            "name": "legacy entity",
            "entity_type": "concept",
            "summary": "old info",
        });
        emb.store_to_collection(ENTITY_COLLECTION, payload, mock_vec.clone())
            .await
            .unwrap();

        let provider = make_mock_provider_with_embedding(mock_vec);
        let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider)
            .with_thresholds(0.85, 0.70);

        let (returned_id, _) = resolver
            .resolve("legacy entity variant", "concept", Some("new info"))
            .await
            .unwrap();

        assert_eq!(
            returned_id, existing_id,
            "older entity ID should be preserved on merge"
        );
    }

    #[tokio::test]
    async fn entity_type_filter_prevents_cross_type_merge() {
        let (gs, emb) = setup_with_embedding().await;

        // Insert a Person named "python"
        let person_id = gs
            .upsert_entity(
                "python",
                "python",
                EntityType::Person,
                Some("a person named python"),
            )
            .await
            .unwrap();

        let mock_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
        emb.ensure_named_collection(ENTITY_COLLECTION, 4)
            .await
            .unwrap();
        let payload = serde_json::json!({
            "entity_id": person_id,
            "name": "python",
            "entity_type": "person",
            "summary": "a person named python",
        });
        emb.store_to_collection(ENTITY_COLLECTION, payload, mock_vec.clone())
            .await
            .unwrap();

        let provider = make_mock_provider_with_embedding(mock_vec);
        let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider)
            .with_thresholds(0.85, 0.70);

        // Resolve "python" as Language — should NOT merge with the Person entity
        let (lang_id, outcome) = resolver
            .resolve("python", "language", Some("python language"))
            .await
            .unwrap();

        // The entity_type filter should prevent merging person "python" with language "python"
        // Check: either created new or an exact match was found under language type
        assert_ne!(
            lang_id, person_id,
            "language entity should not merge with person entity"
        );
        // The entity_type filter causes no embedding candidate to survive the type filter,
        // so resolution falls back to creating a new entity.
        assert_eq!(outcome, ResolutionOutcome::Created);
    }

    #[tokio::test]
    async fn custom_thresholds_respected() {
        let (gs, emb) = setup_with_embedding().await;
        // With a very high threshold (1.0), even identical vectors won't merge
        // (they'd score exactly 1.0 which is NOT > 1.0, so... let's use 0.5 threshold
        // and verify score below 0.5 creates new)
        let existing_id = gs
            .upsert_entity(
                "threshold_test",
                "threshold_test",
                EntityType::Concept,
                Some("base"),
            )
            .await
            .unwrap();

        let existing_vec = vec![1.0_f32, 0.0, 0.0, 0.0];
        emb.ensure_named_collection(ENTITY_COLLECTION, 4)
            .await
            .unwrap();
        let payload = serde_json::json!({
            "entity_id": existing_id,
            "name": "threshold_test",
            "entity_type": "concept",
            "summary": "base",
        });
        emb.store_to_collection(ENTITY_COLLECTION, payload, existing_vec)
            .await
            .unwrap();

        // Orthogonal vector → score = 0.0
        let provider = make_mock_provider_with_embedding(vec![0.0, 1.0, 0.0, 0.0]);
        let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

        // With thresholds 0.50/0.30, score=0 is below 0.30 → create new
        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider)
            .with_thresholds(0.50, 0.30);

        let (id, outcome) = resolver
            .resolve("new_concept", "concept", Some("different"))
            .await
            .unwrap();

        assert_ne!(id, existing_id);
        assert_eq!(outcome, ResolutionOutcome::Created);
    }

    #[tokio::test]
    async fn resolve_outcome_exact_match_no_embedding_store() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        resolver.resolve("existing", "concept", None).await.unwrap();
        let (_, outcome) = resolver.resolve("existing", "concept", None).await.unwrap();
        assert_eq!(outcome, ResolutionOutcome::ExactMatch);
    }

    #[tokio::test]
    async fn extract_json_strips_markdown_fences() {
        let with_fence = "```json\n{\"same_entity\": true}\n```";
        let extracted = extract_json(with_fence);
        let parsed: DisambiguationResponse = serde_json::from_str(extracted).unwrap();
        assert!(parsed.same_entity);

        let without_fence = "{\"same_entity\": false}";
        let extracted2 = extract_json(without_fence);
        let parsed2: DisambiguationResponse = serde_json::from_str(extracted2).unwrap();
        assert!(!parsed2.same_entity);
    }

    // Helper: build a MockProvider with embeddings enabled, given vector, and queued chat responses.
    fn make_mock_with_embedding_and_chat(
        embedding: Vec<f32>,
        chat_responses: Vec<String>,
    ) -> zeph_llm::mock::MockProvider {
        let mut p = zeph_llm::mock::MockProvider::with_responses(chat_responses);
        p.embedding = embedding;
        p.supports_embeddings = true;
        p
    }

    // Seed an existing entity into both SQLite and InMemoryVectorStore at a known vector.
    async fn seed_entity_with_vector(
        gs: &GraphStore,
        emb: &Arc<EmbeddingStore>,
        name: &str,
        entity_type: EntityType,
        summary: &str,
        vector: Vec<f32>,
    ) -> i64 {
        let id = gs
            .upsert_entity(name, name, entity_type, Some(summary))
            .await
            .unwrap();
        emb.ensure_named_collection(ENTITY_COLLECTION, u64::try_from(vector.len()).unwrap())
            .await
            .unwrap();
        let payload = serde_json::json!({
            "entity_id": id,
            "name": name,
            "entity_type": entity_type.as_str(),
            "summary": summary,
        });
        let point_id = emb
            .store_to_collection(ENTITY_COLLECTION, payload, vector)
            .await
            .unwrap();
        gs.set_entity_qdrant_point_id(id, &point_id).await.unwrap();
        id
    }

    // ── GAP-1: ambiguous score + LLM says same_entity=true → LlmDisambiguated ─

    #[tokio::test]
    async fn resolve_ambiguous_score_llm_says_merge() {
        // existing entity at [1,0,0,0]; new entity embeds to [1,1,0,0] → cosine ≈ 0.707
        // thresholds: similarity=0.85, ambiguous=0.50 → score 0.707 is in [0.50, 0.85)
        let (gs, emb) = setup_with_embedding().await;
        let existing_id = seed_entity_with_vector(
            &gs,
            &emb,
            "goroutine",
            EntityType::Concept,
            "go concurrency primitive",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;

        // LLM responds with same_entity=true → should merge
        let provider = make_mock_with_embedding_and_chat(
            vec![1.0, 1.0, 0.0, 0.0],
            vec![r#"{"same_entity": true}"#.to_owned()],
        );
        let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider)
            .with_thresholds(0.85, 0.50);

        let (id, outcome) = resolver
            .resolve("goroutine concurrency", "concept", Some("go concurrency"))
            .await
            .unwrap();

        assert_eq!(
            id, existing_id,
            "should return existing entity ID on LLM merge"
        );
        assert_eq!(outcome, ResolutionOutcome::LlmDisambiguated);
    }

    // ── GAP-2: ambiguous score + LLM says same_entity=false → Created ──────────

    #[tokio::test]
    async fn resolve_ambiguous_score_llm_says_different() {
        let (gs, emb) = setup_with_embedding().await;
        let existing_id = seed_entity_with_vector(
            &gs,
            &emb,
            "channel",
            EntityType::Concept,
            "go channel",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;

        // LLM responds with same_entity=false → should create new entity
        let provider = make_mock_with_embedding_and_chat(
            vec![1.0, 1.0, 0.0, 0.0],
            vec![r#"{"same_entity": false}"#.to_owned()],
        );
        let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider)
            .with_thresholds(0.85, 0.50);

        let (id, outcome) = resolver
            .resolve("network channel", "concept", Some("networking channel"))
            .await
            .unwrap();

        assert_ne!(
            id, existing_id,
            "LLM-rejected match should create new entity"
        );
        assert_eq!(outcome, ResolutionOutcome::Created);
    }

    // ── GAP-3: ambiguous score + LLM chat fails → fallback counter incremented ─

    #[tokio::test]
    async fn resolve_ambiguous_score_llm_failure_increments_fallback() {
        let (gs, emb) = setup_with_embedding().await;
        let existing_id = seed_entity_with_vector(
            &gs,
            &emb,
            "mutex",
            EntityType::Concept,
            "mutual exclusion lock",
            vec![1.0, 0.0, 0.0, 0.0],
        )
        .await;

        // fail_chat=true → provider.chat() returns Err → None from llm_disambiguate
        let mut provider = make_mock_with_embedding_and_chat(vec![1.0, 1.0, 0.0, 0.0], vec![]);
        provider.fail_chat = true;
        let any_provider = zeph_llm::any::AnyProvider::Mock(provider);

        let resolver = EntityResolver::new(&gs)
            .with_embedding_store(&emb)
            .with_provider(&any_provider)
            .with_thresholds(0.85, 0.50);

        let fallback_count = resolver.fallback_count();

        let (id, outcome) = resolver
            .resolve("mutex lock", "concept", Some("synchronization primitive"))
            .await
            .unwrap();

        // LLM failure → fallback to create new
        assert_ne!(
            id, existing_id,
            "LLM failure should create new entity (fallback)"
        );
        assert_eq!(outcome, ResolutionOutcome::Created);
        assert_eq!(
            fallback_count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "fallback counter should be incremented on LLM chat failure"
        );
    }

    // ── Canonicalization / alias tests ────────────────────────────────────────

    #[tokio::test]
    async fn resolve_creates_entity_with_canonical_name() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let (id, _) = resolver.resolve("Rust", "language", None).await.unwrap();
        assert!(id > 0);
        let entity = gs
            .find_entity("rust", EntityType::Language)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entity.canonical_name, "rust");
    }

    #[tokio::test]
    async fn resolve_adds_alias_on_create() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);
        let (id, _) = resolver.resolve("Rust", "language", None).await.unwrap();
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert!(
            !aliases.is_empty(),
            "new entity should have at least one alias"
        );
        assert!(aliases.iter().any(|a| a.alias_name == "rust"));
    }

    #[tokio::test]
    async fn resolve_reuses_entity_by_alias() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // Create entity and register an alias
        let (id1, _) = resolver.resolve("rust", "language", None).await.unwrap();
        gs.add_alias(id1, "rust-lang").await.unwrap();

        // Resolve using the alias — should return the same entity
        let (id2, _) = resolver
            .resolve("rust-lang", "language", None)
            .await
            .unwrap();
        assert_eq!(
            id1, id2,
            "'rust-lang' alias should resolve to same entity as 'rust'"
        );
    }

    #[tokio::test]
    async fn resolve_alias_match_respects_entity_type() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // "python" as a Language
        let (lang_id, _) = resolver.resolve("python", "language", None).await.unwrap();

        // "python" as a Tool should create a separate entity (different type)
        let (tool_id, _) = resolver.resolve("python", "tool", None).await.unwrap();
        assert_ne!(
            lang_id, tool_id,
            "same name with different type should be separate entities"
        );
    }

    #[tokio::test]
    async fn resolve_preserves_existing_aliases() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        let (id, _) = resolver.resolve("rust", "language", None).await.unwrap();
        gs.add_alias(id, "rust-lang").await.unwrap();

        // Upserting same entity should not remove prior aliases
        resolver
            .resolve("rust", "language", Some("updated"))
            .await
            .unwrap();
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert!(
            aliases.iter().any(|a| a.alias_name == "rust-lang"),
            "prior alias must be preserved"
        );
    }

    #[tokio::test]
    async fn resolve_original_form_registered_as_alias() {
        let gs = setup().await;
        let resolver = EntityResolver::new(&gs);

        // "  Rust  " — original trimmed lowercased form is "rust", same as normalized
        // So only one alias should be registered (no duplicate)
        let (id, _) = resolver
            .resolve("  Rust  ", "language", None)
            .await
            .unwrap();
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert!(aliases.iter().any(|a| a.alias_name == "rust"));
    }

    #[tokio::test]
    async fn resolve_entity_with_many_aliases() {
        let gs = setup().await;
        let id = gs
            .upsert_entity("bigentity", "bigentity", EntityType::Concept, None)
            .await
            .unwrap();
        for i in 0..100 {
            gs.add_alias(id, &format!("alias-{i}")).await.unwrap();
        }
        let aliases = gs.aliases_for_entity(id).await.unwrap();
        assert_eq!(aliases.len(), 100);

        // Fuzzy search should still work via alias
        let results = gs.find_entities_fuzzy("alias-50", 10).await.unwrap();
        assert!(results.iter().any(|e| e.id == id));
    }
}
