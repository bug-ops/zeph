use std::collections::HashMap;

use anyhow::Context;
use qdrant_client::Qdrant;
use qdrant_client::qdrant::{
    CreateCollectionBuilder, DeletePointsBuilder, Distance, PointStruct, PointsIdsList,
    ScrollPointsBuilder, SearchPointsBuilder, UpsertPointsBuilder, VectorParamsBuilder,
    value::Kind,
};

use crate::loader::SkillMeta;
use crate::matcher::EmbedFuture;

const COLLECTION_NAME: &str = "zeph_skills";

const SKILL_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x7a, 0x65, 0x70, 0x68, // "zeph"
    0x2d, 0x73, 0x6b, 0x69, // "-ski"
    0x6c, 0x6c, 0x73, 0x00, // "lls\0"
    0x00, 0x00, 0x00, 0x01, // version
]);

#[derive(Debug, Default)]
pub struct SyncStats {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
}

pub struct QdrantSkillMatcher {
    client: Qdrant,
    collection: String,
    hashes: HashMap<String, String>,
}

impl std::fmt::Debug for QdrantSkillMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QdrantSkillMatcher")
            .field("collection", &self.collection)
            .finish_non_exhaustive()
    }
}

fn content_hash(meta: &SkillMeta) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(meta.name.as_bytes());
    hasher.update(meta.description.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn skill_point_id(skill_name: &str) -> String {
    uuid::Uuid::new_v5(&SKILL_NAMESPACE, skill_name.as_bytes()).to_string()
}

impl QdrantSkillMatcher {
    /// # Errors
    ///
    /// Returns an error if the Qdrant client cannot be created.
    pub fn new(qdrant_url: &str) -> anyhow::Result<Self> {
        let client = Qdrant::from_url(qdrant_url)
            .build()
            .context("failed to create Qdrant client")?;

        Ok(Self {
            client,
            collection: COLLECTION_NAME.into(),
            hashes: HashMap::new(),
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
    ) -> anyhow::Result<SyncStats>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        let mut stats = SyncStats::default();

        self.ensure_collection(&embed_fn).await?;

        let existing = self.scroll_all().await?;

        let mut current: HashMap<String, (String, &SkillMeta)> = HashMap::with_capacity(meta.len());
        for m in meta {
            current.insert(m.name.clone(), (content_hash(m), *m));
        }

        let model_changed = existing.values().any(|stored| {
            stored
                .get("embedding_model")
                .is_some_and(|m| m != embedding_model)
        });

        if model_changed {
            tracing::warn!("embedding model changed to '{embedding_model}', recreating collection");
            self.recreate_collection(&embed_fn).await?;
        }

        let mut points_to_upsert = Vec::new();
        for (name, (hash, m)) in &current {
            let needs_update = if let Some(stored) = existing.get(name) {
                model_changed || stored.get("content_hash").is_some_and(|h| h != hash)
            } else {
                true
            };

            if !needs_update {
                stats.unchanged += 1;
                self.hashes.insert(name.clone(), hash.clone());
                continue;
            }

            let vector = match embed_fn(&m.description).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("failed to embed skill '{name}': {e:#}");
                    continue;
                }
            };

            let point_id = skill_point_id(name);
            let payload: serde_json::Value = serde_json::json!({
                "skill_name": name,
                "description": m.description,
                "content_hash": hash,
                "embedding_model": embedding_model,
            });
            let payload_map: HashMap<String, qdrant_client::qdrant::Value> =
                serde_json::from_value(payload).context("failed to convert payload")?;

            points_to_upsert.push(PointStruct::new(point_id, vector, payload_map));

            if existing.contains_key(name) {
                stats.updated += 1;
            } else {
                stats.added += 1;
            }
            self.hashes.insert(name.clone(), hash.clone());
        }

        if !points_to_upsert.is_empty() {
            self.client
                .upsert_points(UpsertPointsBuilder::new(&self.collection, points_to_upsert))
                .await
                .context("failed to upsert skill points")?;
        }

        let orphan_ids: Vec<String> = existing
            .keys()
            .filter(|name| !current.contains_key(*name))
            .map(|name| skill_point_id(name))
            .collect();

        if !orphan_ids.is_empty() {
            stats.removed = orphan_ids.len();
            let point_ids: Vec<qdrant_client::qdrant::PointId> = orphan_ids
                .into_iter()
                .map(|id| qdrant_client::qdrant::PointId::from(id.as_str()))
                .collect();
            self.client
                .delete_points(
                    DeletePointsBuilder::new(&self.collection)
                        .points(PointsIdsList { ids: point_ids }),
                )
                .await
                .context("failed to delete orphan points")?;
        }

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
    /// Returns indices into the provided meta slice.
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
        let query_vec = match embed_fn(query).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("failed to embed query: {e:#}");
                return Vec::new();
            }
        };

        let Ok(limit_u64) = u64::try_from(limit) else {
            return Vec::new();
        };

        let results = match self
            .client
            .search_points(
                SearchPointsBuilder::new(&self.collection, query_vec, limit_u64).with_payload(true),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Qdrant search failed, returning empty: {e:#}");
                return Vec::new();
            }
        };

        results
            .result
            .into_iter()
            .filter_map(|point| {
                let name = point.payload.get("skill_name")?;
                let name_str = match &name.kind {
                    Some(Kind::StringValue(s)) => s.as_str(),
                    _ => return None,
                };
                meta.iter().position(|m| m.name == name_str)
            })
            .collect()
    }

    async fn recreate_collection<F>(&self, embed_fn: &F) -> anyhow::Result<()>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        if self.client.collection_exists(&self.collection).await? {
            self.client
                .delete_collection(&self.collection)
                .await
                .context("failed to delete collection for dimension change")?;
            tracing::info!(
                collection = &self.collection,
                "deleted collection for recreation"
            );
        }
        self.ensure_collection(embed_fn).await
    }

    async fn ensure_collection<F>(&self, embed_fn: &F) -> anyhow::Result<()>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        if self.client.collection_exists(&self.collection).await? {
            return Ok(());
        }

        let probe = embed_fn("dimension probe")
            .await
            .context("failed to probe embedding dimensions")?;
        let vector_size = u64::try_from(probe.len()).context("embedding dimension exceeds u64")?;

        self.client
            .create_collection(
                CreateCollectionBuilder::new(&self.collection)
                    .vectors_config(VectorParamsBuilder::new(vector_size, Distance::Cosine)),
            )
            .await
            .context("failed to create skills collection")?;

        tracing::info!(
            collection = &self.collection,
            dimensions = vector_size,
            "created Qdrant collection for skill embeddings"
        );

        Ok(())
    }

    async fn scroll_all(&self) -> anyhow::Result<HashMap<String, HashMap<String, String>>> {
        let mut result = HashMap::new();
        let mut offset: Option<qdrant_client::qdrant::PointId> = None;

        loop {
            let mut builder = ScrollPointsBuilder::new(&self.collection)
                .with_payload(true)
                .with_vectors(false)
                .limit(100);

            if let Some(ref off) = offset {
                builder = builder.offset(off.clone());
            }

            let response = self
                .client
                .scroll(builder)
                .await
                .context("failed to scroll skill points")?;

            for point in &response.result {
                let Some(name_val) = point.payload.get("skill_name") else {
                    continue;
                };
                let Some(Kind::StringValue(name)) = &name_val.kind else {
                    continue;
                };

                let mut fields = HashMap::new();
                for (key, val) in &point.payload {
                    if let Some(Kind::StringValue(s)) = &val.kind {
                        fields.insert(key.clone(), s.clone());
                    }
                }
                result.insert(name.clone(), fields);
            }

            match response.next_page_offset {
                Some(next) => offset = Some(next),
                None => break,
            }
        }

        Ok(result)
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
            skill_dir: PathBuf::new(),
        }
    }

    #[test]
    fn test_content_hash_deterministic() {
        let meta = make_meta("test", "A test skill");
        let h1 = content_hash(&meta);
        let h2 = content_hash(&meta);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_content_hash_changes_on_modification() {
        let m1 = make_meta("test", "A test skill v1");
        let m2 = make_meta("test", "A test skill v2");
        assert_ne!(content_hash(&m1), content_hash(&m2));
    }

    #[test]
    fn test_skill_point_id_deterministic() {
        let id1 = skill_point_id("my-skill");
        let id2 = skill_point_id("my-skill");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_skill_point_id_different_names() {
        let id1 = skill_point_id("skill-a");
        let id2 = skill_point_id("skill-b");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_sync_stats_default() {
        let stats = SyncStats::default();
        assert_eq!(stats.added, 0);
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.removed, 0);
        assert_eq!(stats.unchanged, 0);
    }

    #[test]
    fn sync_stats_debug() {
        let stats = SyncStats {
            added: 5,
            updated: 3,
            removed: 1,
            unchanged: 10,
        };
        let dbg = format!("{stats:?}");
        assert!(dbg.contains("added"));
        assert!(dbg.contains("5"));
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
    fn content_hash_different_descriptions() {
        let m1 = make_meta("skill", "description A");
        let m2 = make_meta("skill", "description B");
        assert_ne!(content_hash(&m1), content_hash(&m2));
    }

    #[test]
    fn content_hash_different_names() {
        let m1 = make_meta("skill-a", "desc");
        let m2 = make_meta("skill-b", "desc");
        assert_ne!(content_hash(&m1), content_hash(&m2));
    }

    #[test]
    fn skill_point_id_is_valid_uuid() {
        let id = skill_point_id("test-skill");
        assert!(uuid::Uuid::parse_str(&id).is_ok());
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
        let embed_fn =
            |_: &str| -> EmbedFuture { Box::pin(async { Err(anyhow::anyhow!("embed failed")) }) };
        let results = matcher.match_skills(&refs, "query", 5, embed_fn).await;
        assert!(results.is_empty());
    }
}
