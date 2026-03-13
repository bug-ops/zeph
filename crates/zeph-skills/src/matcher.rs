// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::SkillError;
use crate::loader::SkillMeta;
use futures::stream::{self, StreamExt};

pub use zeph_llm::provider::EmbedFuture;

#[derive(Debug, Clone)]
pub struct ScoredMatch {
    pub index: usize,
    pub score: f32,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IntentClassification {
    pub skill_name: String,
    pub confidence: f32,
    #[serde(default)]
    pub params: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct SkillMatcher {
    embeddings: Vec<(usize, Vec<f32>)>,
}

impl SkillMatcher {
    /// Create a matcher by pre-computing embeddings for all skill descriptions.
    ///
    /// Returns `None` if all embeddings fail (caller should fall back to all skills).
    pub async fn new<F>(skills: &[&SkillMeta], embed_fn: F) -> Option<Self>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        type EmbedOutcome = (usize, String, Result<Vec<f32>, Option<zeph_llm::LlmError>>);

        // Collect raw results without logging per-skill; errors will be summarized below.
        let raw: Vec<EmbedOutcome> = stream::iter(skills.iter().enumerate())
            .map(|(i, skill)| {
                let fut = embed_fn(&skill.description);
                let name = skill.name.clone();
                async move {
                    let result = match tokio::time::timeout(Duration::from_secs(10), fut).await {
                        Ok(Ok(vec)) => Ok(vec),
                        Ok(Err(e)) => Err(Some(e)),
                        Err(_) => Err(None),
                    };
                    (i, name, result)
                }
            })
            .buffer_unordered(20)
            .collect()
            .await;

        let mut embeddings = Vec::new();
        // Captures the provider name from any EmbedUnsupported error; last-wins is fine
        // because all unsupported errors for a given provider share the same string.
        let mut unsupported_provider: Option<String> = None;
        let mut unsupported_count: usize = 0;

        for (i, name, result) in raw {
            match result {
                Ok(vec) => embeddings.push((i, vec)),
                Err(Some(zeph_llm::LlmError::EmbedUnsupported { provider })) => {
                    unsupported_provider = Some(provider);
                    unsupported_count += 1;
                }
                Err(None) => {
                    tracing::warn!("embedding timed out for skill '{name}'");
                }
                Err(Some(e)) => {
                    tracing::warn!("failed to embed skill '{name}': {e:#}");
                }
            }
        }

        if unsupported_count > 0
            && let Some(provider) = unsupported_provider
        {
            tracing::info!(
                "skill embeddings skipped: embedding not supported by {provider} \
                 ({unsupported_count} skills affected)"
            );
        }

        if embeddings.is_empty() {
            return None;
        }

        Some(Self { embeddings })
    }

    /// Match a user query against stored skill embeddings, returning the top-K scored matches
    /// ranked by cosine similarity.
    ///
    /// Returns an empty vec if the query embedding fails.
    pub async fn match_skills<F>(
        &self,
        count: usize,
        query: &str,
        limit: usize,
        embed_fn: F,
    ) -> Vec<ScoredMatch>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        let _ = count; // total skill count, unused for in-memory matcher
        let query_vec = match tokio::time::timeout(Duration::from_secs(10), embed_fn(query)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!("failed to embed query: {e:#}");
                return Vec::new();
            }
            Err(_) => {
                tracing::warn!("embedding timed out for query");
                return Vec::new();
            }
        };

        let mut scored: Vec<ScoredMatch> = self
            .embeddings
            .iter()
            .map(|(idx, emb)| ScoredMatch {
                index: *idx,
                score: cosine_similarity(&query_vec, emb),
            })
            .collect();

        scored.sort_unstable_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);

        scored
    }
}

#[derive(Debug, Clone)]
pub enum SkillMatcherBackend {
    InMemory(SkillMatcher),
    Qdrant(crate::qdrant_matcher::QdrantSkillMatcher),
}

impl SkillMatcherBackend {
    #[must_use]
    pub fn is_qdrant(&self) -> bool {
        match self {
            Self::InMemory(_) => false,
            Self::Qdrant(_) => true,
        }
    }

    pub async fn match_skills<F>(
        &self,
        meta: &[&SkillMeta],
        query: &str,
        limit: usize,
        embed_fn: F,
    ) -> Vec<ScoredMatch>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        match self {
            Self::InMemory(m) => m.match_skills(meta.len(), query, limit, embed_fn).await,
            Self::Qdrant(m) => m.match_skills(meta, query, limit, embed_fn).await,
        }
    }

    /// Sync skill embeddings. Only performs work for the Qdrant variant.
    ///
    /// # Errors
    ///
    /// Returns an error if the Qdrant sync fails.
    #[allow(clippy::unused_async)]
    pub async fn sync<F>(
        &mut self,
        meta: &[&SkillMeta],
        embedding_model: &str,
        embed_fn: F,
    ) -> Result<(), SkillError>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        match self {
            Self::InMemory(_) => {
                let _ = (meta, embedding_model, &embed_fn);
                Ok(())
            }
            Self::Qdrant(m) => {
                m.sync(meta, embedding_model, embed_fn).await?;
                Ok(())
            }
        }
    }
}

pub use zeph_memory::cosine_similarity;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-6);
    }

    fn make_meta(name: &str, description: &str) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: description.into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: Vec::new(),
            skill_dir: PathBuf::new(),
        }
    }

    fn embed_fn_mapping(text: &str) -> EmbedFuture {
        let vec = match text {
            "alpha" => vec![1.0, 0.0, 0.0],
            "beta" => vec![0.0, 1.0, 0.0],
            "gamma" => vec![0.0, 0.0, 1.0],
            "query" => vec![0.9, 0.1, 0.0],
            _ => vec![0.0, 0.0, 0.0],
        };
        Box::pin(async move { Ok(vec) })
    }

    fn embed_fn_constant(text: &str) -> EmbedFuture {
        let _ = text;
        Box::pin(async { Ok(vec![1.0, 0.0]) })
    }

    fn embed_fn_fail(text: &str) -> EmbedFuture {
        let _ = text;
        Box::pin(async { Err(zeph_llm::LlmError::Other("error".into())) })
    }

    #[tokio::test]
    async fn test_match_skills_returns_top_k() {
        let metas = [
            make_meta("a", "alpha"),
            make_meta("b", "beta"),
            make_meta("c", "gamma"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let match_results = skill_matcher
            .match_skills(refs.len(), "query", 2, embed_fn_mapping)
            .await;

        assert_eq!(match_results.len(), 2);
        assert_eq!(match_results[0].index, 0); // "a" / "alpha"
        assert_eq!(match_results[1].index, 1); // "b" / "beta"
        assert!(match_results[0].score >= match_results[1].score);
    }

    #[tokio::test]
    async fn test_match_skills_empty_skills() {
        let refs: Vec<&SkillMeta> = Vec::new();
        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await;
        assert!(matcher.is_none());
    }

    #[tokio::test]
    async fn test_match_skills_single_skill() {
        let metas = [make_meta("only", "the only skill")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let match_results = skill_matcher
            .match_skills(refs.len(), "query", 5, embed_fn_constant)
            .await;

        assert_eq!(match_results.len(), 1);
        assert_eq!(match_results[0].index, 0);
    }

    #[tokio::test]
    async fn test_matcher_new_returns_none_on_failure() {
        let metas = [make_meta("fail", "will fail")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_fail).await;
        assert!(matcher.is_none());
    }

    fn embed_fn_unsupported(text: &str) -> EmbedFuture {
        let _ = text;
        Box::pin(async {
            Err(zeph_llm::LlmError::EmbedUnsupported {
                provider: "claude".into(),
            })
        })
    }

    #[tokio::test]
    async fn test_matcher_new_returns_none_when_all_unsupported() {
        let metas = [
            make_meta("a", "alpha"),
            make_meta("b", "beta"),
            make_meta("c", "gamma"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        // All embeddings fail with EmbedUnsupported — matcher must return None
        // and must not produce 3 individual warnings (only 1 summary).
        let matcher = SkillMatcher::new(&refs, embed_fn_unsupported).await;
        assert!(matcher.is_none());
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn test_unsupported_emits_single_info_not_per_skill() {
        let metas = [
            make_meta("a", "alpha"),
            make_meta("b", "beta"),
            make_meta("c", "gamma"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let _ = SkillMatcher::new(&refs, embed_fn_unsupported).await;

        // Summary log must be present from the correct module.
        assert!(logs_contain(
            "zeph_skills::matcher: skill embeddings skipped"
        ));
        // Must be INFO level, not WARN — prevents regression to warn!.
        assert!(!logs_contain(
            "WARN zeph_skills::matcher: skill embeddings skipped"
        ));
        // Per-skill EmbedUnsupported must NOT be logged individually (the fix for #1387).
        assert!(!logs_contain("failed to embed skill"));
    }

    #[tokio::test]
    async fn test_matcher_new_partial_unsupported_falls_back_to_supported() {
        let metas = [make_meta("good", "alpha"), make_meta("bad", "bad skill")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let embed_fn = |text: &str| -> EmbedFuture {
            if text == "alpha" {
                Box::pin(async { Ok(vec![1.0, 0.0]) })
            } else {
                Box::pin(async {
                    Err(zeph_llm::LlmError::EmbedUnsupported {
                        provider: "claude".into(),
                    })
                })
            }
        };

        let matcher = SkillMatcher::new(&refs, embed_fn).await.unwrap();
        assert_eq!(matcher.embeddings.len(), 1);
        assert_eq!(matcher.embeddings[0].0, 0);
    }

    #[tokio::test]
    async fn test_matcher_skips_failed_embeddings() {
        let metas = [
            make_meta("good", "good skill"),
            make_meta("bad", "bad skill"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let embed_fn = |text: &str| -> EmbedFuture {
            if text == "bad skill" {
                Box::pin(async { Err(zeph_llm::LlmError::Other("embed failed".into())) })
            } else {
                Box::pin(async { Ok(vec![1.0, 0.0]) })
            }
        };

        let matcher = SkillMatcher::new(&refs, embed_fn).await.unwrap();
        assert_eq!(matcher.embeddings.len(), 1);
        assert_eq!(matcher.embeddings[0].0, 0);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![1.0, 2.0];
        let b = vec![0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn test_match_skills_returns_all_when_k_larger() {
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let match_results = skill_matcher
            .match_skills(refs.len(), "query", 100, embed_fn_constant)
            .await;

        assert_eq!(match_results.len(), 2);
    }

    #[tokio::test]
    async fn test_match_skills_query_embed_fails() {
        let metas = [make_meta("a", "alpha")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let match_results = skill_matcher
            .match_skills(refs.len(), "query", 5, embed_fn_fail)
            .await;

        assert!(match_results.is_empty());
    }

    #[test]
    fn cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_empty_vectors() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_both_zero() {
        let a = vec![0.0, 0.0];
        let b = vec![0.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_parallel() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![2.0, 4.0, 6.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn match_skills_limit_zero() {
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let match_results = skill_matcher
            .match_skills(refs.len(), "query", 0, embed_fn_constant)
            .await;

        assert!(match_results.is_empty());
    }

    #[tokio::test]
    async fn match_skills_preserves_ranking() {
        let metas = [
            make_meta("far", "gamma"),
            make_meta("close", "alpha"),
            make_meta("mid", "beta"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let match_results = skill_matcher
            .match_skills(refs.len(), "query", 3, embed_fn_mapping)
            .await;

        assert_eq!(match_results.len(), 3);
        assert_eq!(match_results[0].index, 1); // "close" / "alpha" is closest to "query"
    }

    #[test]
    fn matcher_backend_in_memory_is_not_qdrant() {
        let matcher = SkillMatcher {
            embeddings: vec![(0, vec![1.0, 0.0])],
        };
        let backend = SkillMatcherBackend::InMemory(matcher);
        assert!(!backend.is_qdrant());
    }

    #[tokio::test]
    async fn backend_in_memory_sync_is_noop() {
        let matcher = SkillMatcher { embeddings: vec![] };
        let mut backend = SkillMatcherBackend::InMemory(matcher);
        let metas: Vec<&SkillMeta> = vec![];
        let result = backend.sync(&metas, "model", embed_fn_constant).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn backend_in_memory_match_skills() {
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let inner = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let backend = SkillMatcherBackend::InMemory(inner);
        let matches = backend
            .match_skills(&refs, "query", 5, embed_fn_constant)
            .await;
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn matcher_debug() {
        let matcher = SkillMatcher {
            embeddings: vec![(0, vec![1.0])],
        };
        let dbg = format!("{matcher:?}");
        assert!(dbg.contains("SkillMatcher"));
    }

    #[test]
    fn backend_debug() {
        let matcher = SkillMatcher { embeddings: vec![] };
        let backend = SkillMatcherBackend::InMemory(matcher);
        let dbg = format!("{backend:?}");
        assert!(dbg.contains("InMemory"));
    }

    #[test]
    fn scored_match_clone_and_debug() {
        let sm = ScoredMatch {
            index: 0,
            score: 0.95,
        };
        let cloned = sm.clone();
        assert_eq!(cloned.index, 0);
        assert!((cloned.score - 0.95).abs() < f32::EPSILON);
        let dbg = format!("{sm:?}");
        assert!(dbg.contains("ScoredMatch"));
    }

    #[test]
    fn intent_classification_deserialize() {
        let json = r#"{"skill_name":"git","confidence":0.9,"params":{"branch":"main"}}"#;
        let ic: IntentClassification = serde_json::from_str(json).unwrap();
        assert_eq!(ic.skill_name, "git");
        assert!((ic.confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(ic.params.get("branch").unwrap(), "main");
    }

    #[test]
    fn intent_classification_deserialize_without_params() {
        let json = r#"{"skill_name":"test","confidence":0.5}"#;
        let ic: IntentClassification = serde_json::from_str(json).unwrap();
        assert_eq!(ic.skill_name, "test");
        assert!(ic.params.is_empty());
    }

    #[test]
    fn intent_classification_json_schema() {
        let schema = schemars::schema_for!(IntentClassification);
        let json = serde_json::to_string(&schema).unwrap();
        assert!(json.contains("skill_name"));
        assert!(json.contains("confidence"));
    }

    #[test]
    fn intent_classification_rejects_missing_required_fields() {
        let json = r#"{"confidence":0.5}"#;
        let result: Result<IntentClassification, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn scored_match_delta_threshold_zero_disables_disambiguation() {
        // With threshold = 0.0 the condition `(scores[0] - scores[1]) < threshold`
        // evaluates to `delta < 0.0`. For any pair of sorted (descending) scores the
        // delta is always >= 0.0, so this threshold effectively disables disambiguation.
        let threshold = 0.0_f32;

        let high = ScoredMatch {
            index: 0,
            score: 0.90,
        };
        let low = ScoredMatch {
            index: 1,
            score: 0.89,
        };
        let delta = high.score - low.score; // 0.01

        assert!(
            delta >= 0.0,
            "delta between sorted scores is always non-negative"
        );
        assert!(
            delta >= threshold,
            "with threshold=0.0 disambiguation must NOT be triggered"
        );
    }

    #[test]
    fn scored_match_delta_at_threshold_boundary() {
        let threshold = 0.05_f32;

        // delta clearly above threshold => not ambiguous
        let high = ScoredMatch {
            index: 0,
            score: 0.90,
        };
        let low = ScoredMatch {
            index: 1,
            score: 0.80,
        };
        assert!((high.score - low.score) >= threshold);

        // delta clearly below threshold => ambiguous
        let close = ScoredMatch {
            index: 2,
            score: 0.89,
        };
        assert!((high.score - close.score) < threshold);
    }

    #[tokio::test]
    async fn match_skills_returns_scores() {
        let metas = [make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let skill_matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let match_results = skill_matcher
            .match_skills(refs.len(), "query", 2, embed_fn_mapping)
            .await;

        assert_eq!(match_results.len(), 2);
        assert!(match_results[0].score > 0.0);
        assert!(match_results[0].score >= match_results[1].score);
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn scored_match_score_preserved(index in 0usize..100, score in -1.0f32..=1.0) {
            let m = ScoredMatch { index, score };
            // score stored exactly as provided; f32 round-trip is identity
            assert!((m.score - score).abs() < f32::EPSILON);
            assert_eq!(m.index, index);
        }

        #[test]
        fn cosine_similarity_within_bounds(
            a in proptest::collection::vec(-1.0f32..=1.0, 1..10),
            b in proptest::collection::vec(-1.0f32..=1.0, 1..10),
        ) {
            if a.len() == b.len() {
                let result = cosine_similarity(&a, &b);
                // cosine similarity is in [-1, 1], allow small floating-point slack
                assert!((-1.01..=1.01).contains(&result), "got {result}");
            }
        }
    }
}
