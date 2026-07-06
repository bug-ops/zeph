// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(feature = "qdrant")]

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub use zeph_memory::SyncStats;
use zeph_memory::{Embeddable, EmbeddingRegistry, QdrantOps};

use crate::error::SkillError;
use crate::loader::SkillMeta;
use crate::matcher::{EmbedFuture, MatchResult, ScoredMatch};

const COLLECTION_NAME: &str = "zeph_skills";

const SKILL_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x7a, 0x65, 0x70, 0x68, // "zeph"
    0x2d, 0x73, 0x6b, 0x69, // "-ski"
    0x6c, 0x6c, 0x73, 0x00, // "lls\0"
    0x00, 0x00, 0x00, 0x01, // version
]);

impl Embeddable for &SkillMeta {
    fn key(&self) -> &str {
        &self.name
    }

    fn content_hash(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.name.as_bytes());
        hasher.update(self.description.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    fn embed_text(&self) -> &str {
        &self.description
    }

    fn to_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "key": self.name,
            "description": self.description,
        })
    }
}

#[derive(Clone)]
pub struct QdrantSkillMatcher {
    registry: EmbeddingRegistry,
    /// Vectors for the candidates most recently passed to [`Self::refresh_vector_cache`],
    /// keyed by skill index into the caller's `meta` slice. Populated by a bounded
    /// `get_points` request scoped to exactly those candidates, so that
    /// [`Self::skill_embedding`] can serve the RL rerank / `GoSkills` grouping paths
    /// (see issue #5786). The fetch only happens when a caller explicitly requests it —
    /// `match_skills` itself never populates this cache, to avoid paying for a Qdrant
    /// round-trip that most turns (no RL rerank, no grouping) don't need.
    last_vectors: Arc<RwLock<HashMap<usize, Vec<f32>>>>,
}

impl std::fmt::Debug for QdrantSkillMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QdrantSkillMatcher")
            .field("collection", &COLLECTION_NAME)
            .finish_non_exhaustive()
    }
}

impl QdrantSkillMatcher {
    /// Create a `QdrantSkillMatcher` from a pre-built `QdrantOps` instance.
    #[must_use]
    pub fn with_ops(ops: QdrantOps) -> Self {
        Self {
            registry: EmbeddingRegistry::new(ops, COLLECTION_NAME, SKILL_NAMESPACE),
            last_vectors: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Sync skill embeddings with Qdrant. Computes delta and upserts only changed skills.
    ///
    /// `on_progress`, when provided, is called after each successful embed+upsert with
    /// `(completed, total)` counts.
    ///
    /// # Errors
    ///
    /// Returns an error if Qdrant communication fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "skill.qdrant_sync", skip_all)
    )]
    pub async fn sync<F>(
        &mut self,
        meta: &[&SkillMeta],
        embedding_model: &str,
        embed_fn: F,
        on_progress: Option<Box<dyn Fn(usize, usize) + Send>>,
    ) -> Result<SyncStats, SkillError>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        let stats = self
            .registry
            .sync(
                meta,
                embedding_model,
                |text| {
                    let fut = embed_fn(text);
                    Box::pin(async move {
                        fut.await
                            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                    }) as zeph_memory::EmbedFuture
                },
                on_progress,
            )
            .await
            .map_err(|e| SkillError::Other(e.to_string()))?;
        tracing::info!(
            added = stats.added,
            updated = stats.updated,
            removed = stats.removed,
            unchanged = stats.unchanged,
            "skill embeddings synced"
        );
        Ok(stats)
    }

    /// Search for relevant skills using Qdrant native vector search.
    /// Returns scored matches with indices into the provided meta slice.
    ///
    /// Does **not** populate the vector cache read by [`Self::skill_embedding`] — callers that
    /// need per-skill vectors (RL rerank, `GoSkills` grouping) must call
    /// [`Self::refresh_vector_cache`] explicitly with the final candidate set once it's known
    /// (e.g. after BM25 hybrid-search fusion), so the extra `get_points` round-trip is only
    /// paid when one of those features is actually enabled.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "skill.qdrant_match", skip_all, fields(query_len = %query.len(), result_count = tracing::field::Empty))
    )]
    pub async fn match_skills<F>(
        &self,
        meta: &[&SkillMeta],
        query: &str,
        limit: usize,
        embed_fn: F,
    ) -> MatchResult
    where
        F: Fn(&str) -> EmbedFuture,
    {
        let results = match self
            .registry
            .search_raw(query, limit, |text| {
                let fut = embed_fn(text);
                Box::pin(async move {
                    fut.await
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                }) as zeph_memory::EmbedFuture
            })
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Qdrant skill search failed: {e:#}");
                return MatchResult::InfraError;
            }
        };

        let scored: Vec<ScoredMatch> = results
            .into_iter()
            .filter_map(|point| {
                let name = point.payload.get("key")?.as_str()?;
                let index = meta.iter().position(|m| m.name == name)?;
                Some(ScoredMatch {
                    index,
                    score: point.score,
                })
            })
            .collect();

        MatchResult::Scored(scored)
    }

    /// Fetch vectors for the given scored candidates via a single bounded Qdrant `get_points`
    /// round-trip and populate the cache read by [`Self::skill_embedding`].
    ///
    /// The request is scoped to exactly the candidate IDs in `scored` — callers must pass the
    /// final candidate set (e.g. after BM25 hybrid-search fusion, since fusion can introduce
    /// skill indices outside the original vector-search top-K) — never a full collection scan.
    /// Failures are logged and degrade to an empty cache: RL rerank / grouping simply stay
    /// inactive for that turn, same as today's "no embeddings available" fallback in
    /// `assembly.rs`.
    ///
    /// Only call this when a consumer of [`Self::skill_embedding`] is actually enabled (RL
    /// rerank and/or `GoSkills` grouping) — it is not called automatically by
    /// [`Self::match_skills`] so that turns using neither feature don't pay for the extra
    /// round-trip.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "skill.qdrant_refresh_vectors", skip_all, fields(candidates = scored.len()))
    )]
    pub(crate) async fn refresh_vector_cache(&self, meta: &[&SkillMeta], scored: &[ScoredMatch]) {
        let keys: Vec<String> = scored
            .iter()
            .filter_map(|m| meta.get(m.index).map(|s| s.name.clone()))
            .collect();

        let vectors_by_key = match self.registry.get_vectors_by_keys(&keys).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("Qdrant get_points for RL rerank vectors failed: {e:#}");
                HashMap::new()
            }
        };

        let mut cache = HashMap::with_capacity(vectors_by_key.len());
        for m in scored {
            if let Some(name) = meta.get(m.index).map(|s| s.name.as_str())
                && let Some(v) = vectors_by_key.get(name)
            {
                cache.insert(m.index, v.clone());
            }
        }

        if let Ok(mut guard) = self.last_vectors.write() {
            *guard = cache;
        }
    }

    /// Return the cached vector for `skill_index` from the most recent [`Self::match_skills`]
    /// call, if available.
    ///
    /// Populated by a bounded follow-up Qdrant `get_points` request scoped to the top-K
    /// candidates returned by that call. Returns `None` if `skill_index` wasn't part of the
    /// most recent result set, or if the follow-up vector fetch failed or returned a partial
    /// result — callers must treat this the same as "no embedding available".
    #[must_use]
    pub fn skill_embedding(&self, skill_index: usize) -> Option<Vec<f32>> {
        self.last_vectors.read().ok()?.get(&skill_index).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::assert_matches;

    fn make_meta(name: &str, description: &str) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: description.into(),
            ..Default::default()
        }
    }

    #[test]
    fn embeddable_key() {
        let meta = make_meta("my-skill", "desc");
        assert_eq!((&meta).key(), "my-skill");
    }

    #[test]
    fn embeddable_embed_text() {
        let meta = make_meta("skill", "A test skill");
        assert_eq!((&meta).embed_text(), "A test skill");
    }

    #[test]
    fn embeddable_content_hash_deterministic() {
        let meta = make_meta("test", "A test skill");
        assert_eq!((&meta).content_hash(), (&meta).content_hash());
    }

    #[test]
    fn embeddable_content_hash_changes_on_modification() {
        let m1 = make_meta("test", "A test skill v1");
        let m2 = make_meta("test", "A test skill v2");
        assert_ne!((&m1).content_hash(), (&m2).content_hash());
    }

    #[test]
    fn embeddable_payload_has_key_field() {
        let meta = make_meta("my-skill", "desc");
        let payload = (&meta).to_payload();
        assert_eq!(payload["key"], "my-skill");
    }

    fn make_matcher() -> QdrantSkillMatcher {
        let ops = QdrantOps::new("http://localhost:6334", None).unwrap();
        QdrantSkillMatcher::with_ops(ops)
    }

    #[test]
    fn construction_with_ops() {
        let _matcher = make_matcher();
    }

    #[test]
    fn debug_format() {
        let matcher = make_matcher();
        let dbg = format!("{matcher:?}");
        assert!(dbg.contains("QdrantSkillMatcher"));
        assert!(dbg.contains("zeph_skills"));
    }

    #[test]
    fn content_hash_different_names() {
        let m1 = make_meta("skill-a", "desc");
        let m2 = make_meta("skill-b", "desc");
        assert_ne!((&m1).content_hash(), (&m2).content_hash());
    }

    #[test]
    fn content_hash_different_descriptions() {
        let m1 = make_meta("skill", "description A");
        let m2 = make_meta("skill", "description B");
        assert_ne!((&m1).content_hash(), (&m2).content_hash());
    }

    #[test]
    fn skill_namespace_is_valid() {
        assert!(!SKILL_NAMESPACE.is_nil());
    }

    #[tokio::test]
    async fn match_skills_embed_fail_returns_empty() {
        let matcher = make_matcher();
        let metas = [make_meta("s", "desc")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let embed_fn = |_: &str| -> EmbedFuture {
            Box::pin(async { Err(zeph_llm::LlmError::Other("embed failed".into())) })
        };
        let results = matcher.match_skills(&refs, "query", 5, embed_fn).await;
        assert_matches!(results, crate::matcher::MatchResult::InfraError);
    }

    #[test]
    fn skill_embedding_none_before_any_match_skills_call() {
        let matcher = make_matcher();
        assert!(matcher.skill_embedding(0).is_none());
    }

    fn make_unreachable_matcher() -> QdrantSkillMatcher {
        let ops = QdrantOps::new("http://127.0.0.1:1", None).unwrap();
        QdrantSkillMatcher::with_ops(ops)
    }

    #[tokio::test]
    async fn refresh_vector_cache_unreachable_qdrant_degrades_to_empty() {
        // The RL rerank gate treats a missing vector the same as "backend unavailable" — a
        // failed follow-up get_points call must not panic and must leave the cache empty
        // rather than poisoning it with stale data.
        let matcher = make_unreachable_matcher();
        let metas = [make_meta("s", "desc")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let scored = vec![ScoredMatch {
            index: 0,
            score: 0.9,
        }];

        matcher.refresh_vector_cache(&refs, &scored).await;

        assert!(matcher.skill_embedding(0).is_none());
    }

    #[tokio::test]
    async fn refresh_vector_cache_empty_scored_is_noop() {
        let matcher = make_unreachable_matcher();
        let metas = [make_meta("s", "desc")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        // Empty candidate set must short-circuit before any Qdrant round-trip.
        matcher.refresh_vector_cache(&refs, &[]).await;

        assert!(matcher.skill_embedding(0).is_none());
    }
}
