use crate::error::SkillError;
use crate::loader::SkillMeta;

pub use zeph_llm::provider::EmbedFuture;

#[derive(Debug)]
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
        let mut embeddings = Vec::with_capacity(skills.len());

        for (i, skill) in skills.iter().enumerate() {
            match embed_fn(&skill.description).await {
                Ok(vec) => embeddings.push((i, vec)),
                Err(e) => tracing::warn!("failed to embed skill '{}': {e:#}", skill.name),
            }
        }

        if embeddings.is_empty() {
            return None;
        }

        Some(Self { embeddings })
    }

    /// Match a user query against stored skill embeddings, returning the top-K indices
    /// ranked by cosine similarity.
    ///
    /// Returns an empty vec if the query embedding fails.
    pub async fn match_skills<F>(
        &self,
        count: usize,
        query: &str,
        limit: usize,
        embed_fn: F,
    ) -> Vec<usize>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        let _ = count; // total skill count, unused for in-memory matcher
        let query_vec = match embed_fn(query).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to embed query: {e:#}");
                return Vec::new();
            }
        };

        let mut scored: Vec<(usize, f32)> = self
            .embeddings
            .iter()
            .map(|(idx, emb)| (*idx, cosine_similarity(&query_vec, emb)))
            .collect();

        scored.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        scored.into_iter().map(|(idx, _)| idx).collect()
    }
}

#[derive(Debug)]
pub enum SkillMatcherBackend {
    InMemory(SkillMatcher),
    #[cfg(feature = "qdrant")]
    Qdrant(crate::qdrant_matcher::QdrantSkillMatcher),
}

impl SkillMatcherBackend {
    #[must_use]
    pub fn is_qdrant(&self) -> bool {
        match self {
            Self::InMemory(_) => false,
            #[cfg(feature = "qdrant")]
            Self::Qdrant(_) => true,
        }
    }

    pub async fn match_skills<F>(
        &self,
        meta: &[&SkillMeta],
        query: &str,
        limit: usize,
        embed_fn: F,
    ) -> Vec<usize>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        match self {
            Self::InMemory(m) => m.match_skills(meta.len(), query, limit, embed_fn).await,
            #[cfg(feature = "qdrant")]
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
            #[cfg(feature = "qdrant")]
            Self::Qdrant(m) => {
                m.sync(meta, embedding_model, embed_fn).await?;
                Ok(())
            }
        }
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;

    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        return 0.0;
    }

    dot / denom
}

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
        let metas = vec![
            make_meta("a", "alpha"),
            make_meta("b", "beta"),
            make_meta("c", "gamma"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let matched = matcher
            .match_skills(refs.len(), "query", 2, embed_fn_mapping)
            .await;

        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0], 0); // "a" / "alpha"
        assert_eq!(matched[1], 1); // "b" / "beta"
    }

    #[tokio::test]
    async fn test_match_skills_empty_skills() {
        let refs: Vec<&SkillMeta> = Vec::new();
        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await;
        assert!(matcher.is_none());
    }

    #[tokio::test]
    async fn test_match_skills_single_skill() {
        let metas = vec![make_meta("only", "the only skill")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let matched = matcher
            .match_skills(refs.len(), "query", 5, embed_fn_constant)
            .await;

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0], 0);
    }

    #[tokio::test]
    async fn test_matcher_new_returns_none_on_failure() {
        let metas = vec![make_meta("fail", "will fail")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let matcher = SkillMatcher::new(&refs, embed_fn_fail).await;
        assert!(matcher.is_none());
    }

    #[tokio::test]
    async fn test_matcher_skips_failed_embeddings() {
        let metas = vec![
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
        assert_eq!(sim, 0.0);
    }

    #[tokio::test]
    async fn test_match_skills_returns_all_when_k_larger() {
        let metas = vec![make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let matched = matcher
            .match_skills(refs.len(), "query", 100, embed_fn_constant)
            .await;

        assert_eq!(matched.len(), 2);
    }

    #[tokio::test]
    async fn test_match_skills_query_embed_fails() {
        let metas = vec![make_meta("a", "alpha")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let matched = matcher
            .match_skills(refs.len(), "query", 5, embed_fn_fail)
            .await;

        assert!(matched.is_empty());
    }

    #[test]
    fn cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_empty_vectors() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_both_zero() {
        let a = vec![0.0, 0.0];
        let b = vec![0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
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
        let metas = vec![make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let matcher = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let matched = matcher
            .match_skills(refs.len(), "query", 0, embed_fn_constant)
            .await;

        assert!(matched.is_empty());
    }

    #[tokio::test]
    async fn match_skills_preserves_ranking() {
        let metas = vec![
            make_meta("far", "gamma"),
            make_meta("close", "alpha"),
            make_meta("mid", "beta"),
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let matcher = SkillMatcher::new(&refs, embed_fn_mapping).await.unwrap();
        let matched = matcher
            .match_skills(refs.len(), "query", 3, embed_fn_mapping)
            .await;

        assert_eq!(matched.len(), 3);
        assert_eq!(matched[0], 1); // "close" / "alpha" is closest to "query"
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
        let metas = vec![make_meta("a", "alpha"), make_meta("b", "beta")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();

        let inner = SkillMatcher::new(&refs, embed_fn_constant).await.unwrap();
        let backend = SkillMatcherBackend::InMemory(inner);
        let matched = backend
            .match_skills(&refs, "query", 5, embed_fn_constant)
            .await;
        assert_eq!(matched.len(), 2);
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
}
