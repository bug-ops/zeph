pub use zeph_memory::SyncStats;
use zeph_memory::{Embeddable, EmbeddingRegistry, QdrantOps};

use crate::error::SkillError;
use crate::loader::SkillMeta;
use crate::matcher::{EmbedFuture, ScoredMatch};

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

pub struct QdrantSkillMatcher {
    registry: EmbeddingRegistry,
}

impl std::fmt::Debug for QdrantSkillMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QdrantSkillMatcher")
            .field("collection", &COLLECTION_NAME)
            .finish_non_exhaustive()
    }
}

impl QdrantSkillMatcher {
    /// # Errors
    ///
    /// Returns an error if the Qdrant client cannot be created.
    pub fn new(qdrant_url: &str) -> Result<Self, SkillError> {
        let ops = QdrantOps::new(qdrant_url)?;
        Ok(Self {
            registry: EmbeddingRegistry::new(ops, COLLECTION_NAME, SKILL_NAMESPACE),
        })
    }

    /// Sync skill embeddings with Qdrant. Computes delta and upserts only changed skills.
    ///
    /// # Errors
    ///
    /// Returns an error if Qdrant communication fails.
    pub async fn sync<F>(
        &mut self,
        meta: &[&SkillMeta],
        embedding_model: &str,
        embed_fn: F,
    ) -> Result<SyncStats, SkillError>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        let stats = self
            .registry
            .sync(meta, embedding_model, |text| {
                let fut = embed_fn(text);
                Box::pin(async move {
                    fut.await
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                }) as zeph_memory::EmbedFuture
            })
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
                return Vec::new();
            }
        };

        results
            .into_iter()
            .filter_map(|point| {
                let name = point.payload.get("key")?.as_str()?;
                let index = meta.iter().position(|m| m.name == name)?;
                Some(ScoredMatch {
                    index,
                    score: point.score,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    #[test]
    fn construction_valid_url() {
        let result = QdrantSkillMatcher::new("http://localhost:6334");
        assert!(result.is_ok());
    }

    #[test]
    fn debug_format() {
        let matcher = QdrantSkillMatcher::new("http://localhost:6334").unwrap();
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
        let matcher = QdrantSkillMatcher::new("http://localhost:6334").unwrap();
        let metas = vec![make_meta("s", "desc")];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let embed_fn = |_: &str| -> EmbedFuture {
            Box::pin(async { Err(zeph_llm::LlmError::Other("embed failed".into())) })
        };
        let results = matcher.match_skills(&refs, "query", 5, embed_fn).await;
        assert!(results.is_empty());
    }
}
