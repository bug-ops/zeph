use std::collections::HashMap;

use anyhow::Context;
use qdrant_client::Qdrant;
use qdrant_client::qdrant::{
    CreateCollectionBuilder, DeletePointsBuilder, Distance, PointStruct, PointsIdsList,
    ScrollPointsBuilder, SearchPointsBuilder, UpsertPointsBuilder, VectorParamsBuilder,
    value::Kind,
};

use crate::tool::McpTool;

pub type EmbedFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<f32>>> + Send>>;

const COLLECTION_NAME: &str = "zeph_mcp_tools";

const MCP_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x7a, 0x65, 0x70, 0x68, // "zeph"
    0x2d, 0x6d, 0x63, 0x70, // "-mcp"
    0x2d, 0x74, 0x6f, 0x6f, // "-too"
    0x6c, 0x73, 0x00, 0x01, // "ls\0\x01"
]);

#[derive(Debug, Default)]
pub struct SyncStats {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
}

pub struct McpToolRegistry {
    client: Qdrant,
    collection: String,
    hashes: HashMap<String, String>,
}

impl std::fmt::Debug for McpToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolRegistry")
            .field("collection", &self.collection)
            .finish_non_exhaustive()
    }
}

fn content_hash(tool: &McpTool) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(tool.server_id.as_bytes());
    hasher.update(tool.name.as_bytes());
    hasher.update(tool.description.as_bytes());
    hasher.update(tool.input_schema.to_string().as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn tool_point_id(tool_key: &str) -> String {
    uuid::Uuid::new_v5(&MCP_NAMESPACE, tool_key.as_bytes()).to_string()
}

impl McpToolRegistry {
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

    /// Sync MCP tool embeddings with Qdrant. Computes delta and upserts only changed tools.
    ///
    /// # Errors
    ///
    /// Returns an error if Qdrant communication fails.
    pub async fn sync<F>(
        &mut self,
        tools: &[McpTool],
        embedding_model: &str,
        embed_fn: F,
    ) -> anyhow::Result<SyncStats>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        let mut stats = SyncStats::default();

        self.ensure_collection(&embed_fn).await?;

        let existing = self.scroll_all().await?;

        let mut current: HashMap<String, (String, &McpTool)> = HashMap::with_capacity(tools.len());
        for tool in tools {
            let key = tool.qualified_name();
            current.insert(key, (content_hash(tool), tool));
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
        for (key, (hash, tool)) in &current {
            let needs_update = if let Some(stored) = existing.get(key) {
                model_changed || stored.get("content_hash").is_some_and(|h| h != hash)
            } else {
                true
            };

            if !needs_update {
                stats.unchanged += 1;
                self.hashes.insert(key.clone(), hash.clone());
                continue;
            }

            let vector = match embed_fn(&tool.description).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("failed to embed tool '{key}': {e:#}");
                    continue;
                }
            };

            let point_id = tool_point_id(key);
            let payload: serde_json::Value = serde_json::json!({
                "tool_key": key,
                "server_id": tool.server_id,
                "tool_name": tool.name,
                "description": tool.description,
                "content_hash": hash,
                "embedding_model": embedding_model,
            });
            let payload_map: HashMap<String, qdrant_client::qdrant::Value> =
                serde_json::from_value(payload).context("failed to convert payload")?;

            points_to_upsert.push(PointStruct::new(point_id, vector, payload_map));

            if existing.contains_key(key) {
                stats.updated += 1;
            } else {
                stats.added += 1;
            }
            self.hashes.insert(key.clone(), hash.clone());
        }

        if !points_to_upsert.is_empty() {
            self.client
                .upsert_points(UpsertPointsBuilder::new(&self.collection, points_to_upsert))
                .await
                .context("failed to upsert MCP tool points")?;
        }

        let orphan_ids: Vec<String> = existing
            .keys()
            .filter(|key| !current.contains_key(*key))
            .map(|key| tool_point_id(key))
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
                .context("failed to delete orphan MCP tool points")?;
        }

        tracing::info!(
            added = stats.added,
            updated = stats.updated,
            removed = stats.removed,
            unchanged = stats.unchanged,
            "MCP tool embeddings synced"
        );

        Ok(stats)
    }

    /// Search for relevant MCP tools using Qdrant vector search.
    pub async fn search<F>(&self, query: &str, limit: usize, embed_fn: F) -> Vec<McpTool>
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
                tracing::warn!("Qdrant MCP tool search failed: {e:#}");
                return Vec::new();
            }
        };

        results
            .result
            .into_iter()
            .filter_map(|point| {
                let server_id = extract_string(&point.payload, "server_id")?;
                let name = extract_string(&point.payload, "tool_name")?;
                let description = extract_string(&point.payload, "description").unwrap_or_default();
                Some(McpTool {
                    server_id,
                    name,
                    description,
                    input_schema: serde_json::Value::Object(serde_json::Map::new()),
                })
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
                .context("failed to delete MCP tools collection for recreation")?;
            tracing::info!(
                collection = &self.collection,
                "deleted MCP tools collection for recreation"
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
            .context("failed to create MCP tools collection")?;

        tracing::info!(
            collection = &self.collection,
            dimensions = vector_size,
            "created Qdrant collection for MCP tool embeddings"
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
                .context("failed to scroll MCP tool points")?;

            for point in &response.result {
                let Some(key_val) = point.payload.get("tool_key") else {
                    continue;
                };
                let Some(Kind::StringValue(key)) = &key_val.kind else {
                    continue;
                };

                let mut fields = HashMap::new();
                for (k, val) in &point.payload {
                    if let Some(Kind::StringValue(s)) = &val.kind {
                        fields.insert(k.clone(), s.clone());
                    }
                }
                result.insert(key.clone(), fields);
            }

            match response.next_page_offset {
                Some(next) => offset = Some(next),
                None => break,
            }
        }

        Ok(result)
    }
}

fn extract_string(
    payload: &HashMap<String, qdrant_client::qdrant::Value>,
    key: &str,
) -> Option<String> {
    let val = payload.get(key)?;
    match &val.kind {
        Some(Kind::StringValue(s)) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(server: &str, name: &str) -> McpTool {
        McpTool {
            server_id: server.into(),
            name: name.into(),
            description: "test".into(),
            input_schema: serde_json::json!({}),
        }
    }

    #[test]
    fn content_hash_deterministic() {
        let tool = make_tool("github", "create_issue");
        let h1 = content_hash(&tool);
        let h2 = content_hash(&tool);
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_changes_on_modification() {
        let t1 = make_tool("github", "create_issue");
        let mut t2 = make_tool("github", "create_issue");
        t2.description = "modified".into();
        assert_ne!(content_hash(&t1), content_hash(&t2));
    }

    #[test]
    fn tool_point_id_deterministic() {
        let id1 = tool_point_id("github:create_issue");
        let id2 = tool_point_id("github:create_issue");
        assert_eq!(id1, id2);
    }

    #[test]
    fn tool_point_id_different_keys() {
        let id1 = tool_point_id("github:create_issue");
        let id2 = tool_point_id("fs:read_file");
        assert_ne!(id1, id2);
    }

    #[test]
    fn sync_stats_default() {
        let stats = SyncStats::default();
        assert_eq!(stats.added, 0);
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.removed, 0);
        assert_eq!(stats.unchanged, 0);
    }
}
