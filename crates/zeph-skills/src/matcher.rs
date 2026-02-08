use std::future::Future;
use std::pin::Pin;

use crate::loader::Skill;

/// Type alias for boxed embed futures to work around async closure lifetime issues.
pub type EmbedFuture = Pin<Box<dyn Future<Output = anyhow::Result<Vec<f32>>> + Send>>;

pub struct SkillMatcher {
    embeddings: Vec<(usize, Vec<f32>)>,
}

impl SkillMatcher {
    /// Create a matcher by pre-computing embeddings for all skill descriptions.
    ///
    /// Returns `None` if all embeddings fail (caller should fall back to all skills).
    pub async fn new<F>(skills: &[Skill], embed_fn: F) -> Option<Self>
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

    /// Match a user query against stored skill embeddings, returning the top-K skills
    /// ranked by cosine similarity.
    ///
    /// Returns an empty vec if the query embedding fails.
    pub async fn match_skills<'a, F>(
        &self,
        skills: &'a [Skill],
        query: &str,
        limit: usize,
        embed_fn: F,
    ) -> Vec<&'a Skill>
    where
        F: Fn(&str) -> EmbedFuture,
    {
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

        scored
            .into_iter()
            .filter_map(|(idx, _)| skills.get(idx))
            .collect()
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

    fn make_skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            body: String::new(),
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
        Box::pin(async { Err(anyhow::anyhow!("error")) })
    }

    #[tokio::test]
    async fn test_match_skills_returns_top_k() {
        let skills = vec![
            make_skill("a", "alpha"),
            make_skill("b", "beta"),
            make_skill("c", "gamma"),
        ];

        let matcher = SkillMatcher::new(&skills, embed_fn_mapping).await.unwrap();
        let matched = matcher
            .match_skills(&skills, "query", 2, embed_fn_mapping)
            .await;

        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].name, "a");
        assert_eq!(matched[1].name, "b");
    }

    #[tokio::test]
    async fn test_match_skills_empty_skills() {
        let skills: Vec<Skill> = Vec::new();
        let matcher = SkillMatcher::new(&skills, embed_fn_constant).await;
        assert!(matcher.is_none());
    }

    #[tokio::test]
    async fn test_match_skills_single_skill() {
        let skills = vec![make_skill("only", "the only skill")];

        let matcher = SkillMatcher::new(&skills, embed_fn_constant).await.unwrap();
        let matched = matcher
            .match_skills(&skills, "query", 5, embed_fn_constant)
            .await;

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "only");
    }

    #[tokio::test]
    async fn test_matcher_new_returns_none_on_failure() {
        let skills = vec![make_skill("fail", "will fail")];
        let matcher = SkillMatcher::new(&skills, embed_fn_fail).await;
        assert!(matcher.is_none());
    }

    #[tokio::test]
    async fn test_matcher_skips_failed_embeddings() {
        let skills = vec![
            make_skill("good", "good skill"),
            make_skill("bad", "bad skill"),
        ];

        let embed_fn = |text: &str| -> EmbedFuture {
            if text == "bad skill" {
                Box::pin(async { Err(anyhow::anyhow!("embed failed")) })
            } else {
                Box::pin(async { Ok(vec![1.0, 0.0]) })
            }
        };

        let matcher = SkillMatcher::new(&skills, embed_fn).await.unwrap();
        assert_eq!(matcher.embeddings.len(), 1);
        assert_eq!(matcher.embeddings[0].0, 0);
    }
}
