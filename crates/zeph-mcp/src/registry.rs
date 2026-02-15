use std::collections::HashMap;

use qdrant_client::qdrant::{PointStruct, value::Kind};
use zeph_memory::QdrantOps;

use crate::error::McpError;
use crate::tool::McpTool;

pub use zeph_llm::provider::EmbedFuture;

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
    ops: QdrantOps,
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
    pub fn new(qdrant_url: &str) -> Result<Self, McpError> {
        let ops = QdrantOps::new(qdrant_url)?;

        Ok(Self {
            ops,
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
    ) -> Result<SyncStats, McpError>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        let mut stats = SyncStats::default();

        self.ensure_collection(&embed_fn).await?;

        let existing = self.ops.scroll_all(&self.collection, "tool_key").await?;

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
            let payload = serde_json::json!({
                "tool_key": key,
                "server_id": tool.server_id,
                "tool_name": tool.name,
                "description": tool.description,
                "content_hash": hash,
                "embedding_model": embedding_model,
            });
            let payload_map = QdrantOps::json_to_payload(payload)?;

            points_to_upsert.push(PointStruct::new(point_id, vector, payload_map));

            if existing.contains_key(key) {
                stats.updated += 1;
            } else {
                stats.added += 1;
            }
            self.hashes.insert(key.clone(), hash.clone());
        }

        if !points_to_upsert.is_empty() {
            self.ops.upsert(&self.collection, points_to_upsert).await?;
        }

        let orphan_ids: Vec<qdrant_client::qdrant::PointId> = existing
            .keys()
            .filter(|key| !current.contains_key(*key))
            .map(|key| qdrant_client::qdrant::PointId::from(tool_point_id(key).as_str()))
            .collect();

        if !orphan_ids.is_empty() {
            stats.removed = orphan_ids.len();
            self.ops.delete_by_ids(&self.collection, orphan_ids).await?;
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
            .ops
            .search(&self.collection, query_vec, limit_u64, None)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("Qdrant MCP tool search failed: {e:#}");
                return Vec::new();
            }
        };

        results
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

    async fn recreate_collection<F>(&self, embed_fn: &F) -> Result<(), McpError>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        if self.ops.collection_exists(&self.collection).await? {
            self.ops.delete_collection(&self.collection).await?;
            tracing::info!(
                collection = &self.collection,
                "deleted MCP tools collection for recreation"
            );
        }
        self.ensure_collection(embed_fn).await
    }

    async fn ensure_collection<F>(&self, embed_fn: &F) -> Result<(), McpError>
    where
        F: Fn(&str) -> EmbedFuture,
    {
        if self.ops.collection_exists(&self.collection).await? {
            return Ok(());
        }

        let probe = embed_fn("dimension probe")
            .await
            .map_err(|e| McpError::Embedding(e.to_string()))?;
        let vector_size = u64::try_from(probe.len())?;

        self.ops
            .ensure_collection(&self.collection, vector_size)
            .await?;

        tracing::info!(
            collection = &self.collection,
            dimensions = vector_size,
            "created Qdrant collection for MCP tool embeddings"
        );

        Ok(())
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

    #[test]
    fn sync_stats_debug() {
        let stats = SyncStats {
            added: 1,
            updated: 2,
            removed: 3,
            unchanged: 4,
        };
        let dbg = format!("{stats:?}");
        assert!(dbg.contains("added"));
        assert!(dbg.contains("1"));
    }

    #[test]
    fn registry_new_valid_url() {
        let result = McpToolRegistry::new("http://localhost:6334");
        assert!(result.is_ok());
    }

    #[test]
    fn registry_debug() {
        let registry = McpToolRegistry::new("http://localhost:6334").unwrap();
        let dbg = format!("{registry:?}");
        assert!(dbg.contains("McpToolRegistry"));
        assert!(dbg.contains("zeph_mcp_tools"));
    }

    #[test]
    fn content_hash_different_server_same_name() {
        let t1 = McpTool {
            server_id: "server-a".into(),
            name: "tool".into(),
            description: "test".into(),
            input_schema: serde_json::json!({}),
        };
        let t2 = McpTool {
            server_id: "server-b".into(),
            name: "tool".into(),
            description: "test".into(),
            input_schema: serde_json::json!({}),
        };
        assert_ne!(content_hash(&t1), content_hash(&t2));
    }

    #[test]
    fn content_hash_different_schema() {
        let t1 = make_tool("s", "t");
        let mut t2 = make_tool("s", "t");
        t2.input_schema = serde_json::json!({"type": "object"});
        assert_ne!(content_hash(&t1), content_hash(&t2));
    }

    #[test]
    fn tool_point_id_is_valid_uuid() {
        let id = tool_point_id("test:tool");
        assert!(uuid::Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn mcp_namespace_is_valid_uuid() {
        assert!(!MCP_NAMESPACE.is_nil());
    }

    #[test]
    fn extract_string_present() {
        let mut payload = HashMap::new();
        payload.insert(
            "key".into(),
            qdrant_client::qdrant::Value {
                kind: Some(Kind::StringValue("hello".into())),
            },
        );
        assert_eq!(extract_string(&payload, "key"), Some("hello".into()));
    }

    #[test]
    fn extract_string_missing_key() {
        let payload: HashMap<String, qdrant_client::qdrant::Value> = HashMap::new();
        assert_eq!(extract_string(&payload, "missing"), None);
    }

    #[test]
    fn extract_string_non_string_value() {
        let mut payload = HashMap::new();
        payload.insert(
            "key".into(),
            qdrant_client::qdrant::Value {
                kind: Some(Kind::IntegerValue(42)),
            },
        );
        assert_eq!(extract_string(&payload, "key"), None);
    }

    #[test]
    fn extract_string_none_kind() {
        let mut payload = HashMap::new();
        payload.insert("key".into(), qdrant_client::qdrant::Value { kind: None });
        assert_eq!(extract_string(&payload, "key"), None);
    }

    #[tokio::test]
    async fn search_empty_registry_returns_empty() {
        let registry = McpToolRegistry::new("http://localhost:6334").unwrap();
        let embed_fn = |_: &str| -> crate::registry::EmbedFuture {
            Box::pin(async { Err(zeph_llm::LlmError::Other("no qdrant".into())) })
        };
        let results = registry.search("test query", 5, embed_fn).await;
        assert!(results.is_empty());
    }

    #[test]
    fn registry_new_with_invalid_url_fails() {
        let result = McpToolRegistry::new("not a valid url");
        assert!(result.is_err());
    }

    #[test]
    fn content_hash_length_is_blake3_hex() {
        let tool = make_tool("server", "tool");
        let hash = content_hash(&tool);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn content_hash_same_input_same_hash() {
        let t1 = McpTool {
            server_id: "s".into(),
            name: "t".into(),
            description: "desc".into(),
            input_schema: serde_json::json!({"type": "string"}),
        };
        let t2 = McpTool {
            server_id: "s".into(),
            name: "t".into(),
            description: "desc".into(),
            input_schema: serde_json::json!({"type": "string"}),
        };
        assert_eq!(content_hash(&t1), content_hash(&t2));
    }

    #[test]
    fn content_hash_different_name_different_hash() {
        let t1 = make_tool("s", "tool_a");
        let t2 = make_tool("s", "tool_b");
        assert_ne!(content_hash(&t1), content_hash(&t2));
    }

    #[test]
    fn tool_point_id_format_uuid_v5() {
        let id = tool_point_id("github:create_issue");
        let parsed = uuid::Uuid::parse_str(&id).unwrap();
        assert_eq!(parsed.get_version_num(), 5);
    }

    #[test]
    fn tool_point_id_consistent_across_calls() {
        let key = "server:tool_name";
        let ids: Vec<String> = (0..10).map(|_| tool_point_id(key)).collect();
        for id in &ids {
            assert_eq!(id, &ids[0]);
        }
    }

    #[test]
    fn collection_name_constant() {
        assert_eq!(COLLECTION_NAME, "zeph_mcp_tools");
    }

    #[tokio::test]
    async fn search_with_embedding_failure_returns_empty() {
        let registry = McpToolRegistry::new("http://localhost:6334").unwrap();
        let embed_fn = |_: &str| -> crate::registry::EmbedFuture {
            Box::pin(async {
                Err(zeph_llm::LlmError::Other(
                    "embedding model not loaded".into(),
                ))
            })
        };
        let results = registry.search("search query", 10, embed_fn).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_with_zero_limit() {
        let registry = McpToolRegistry::new("http://localhost:6334").unwrap();
        let embed_fn = |_: &str| -> crate::registry::EmbedFuture {
            Box::pin(async { Ok(vec![0.1, 0.2, 0.3]) })
        };
        let results = registry.search("query", 0, embed_fn).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn sync_with_unreachable_qdrant_fails() {
        let mut registry = McpToolRegistry::new("http://127.0.0.1:1").unwrap();
        let tools = vec![make_tool("server", "tool")];
        let embed_fn = |_: &str| -> crate::registry::EmbedFuture {
            Box::pin(async { Ok(vec![0.1, 0.2, 0.3]) })
        };
        let result = registry.sync(&tools, "test-model", embed_fn).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sync_with_empty_tools_and_unreachable_qdrant_fails() {
        let mut registry = McpToolRegistry::new("http://127.0.0.1:1").unwrap();
        let embed_fn = |_: &str| -> crate::registry::EmbedFuture {
            Box::pin(async { Ok(vec![0.1, 0.2, 0.3]) })
        };
        let result = registry.sync(&[], "test-model", embed_fn).await;
        assert!(result.is_err());
    }

    #[test]
    fn extract_string_from_double_value_returns_none() {
        let mut payload = HashMap::new();
        payload.insert(
            "key".into(),
            qdrant_client::qdrant::Value {
                kind: Some(Kind::DoubleValue(3.14)),
            },
        );
        assert_eq!(extract_string(&payload, "key"), None);
    }

    #[test]
    fn extract_string_from_bool_value_returns_none() {
        let mut payload = HashMap::new();
        payload.insert(
            "key".into(),
            qdrant_client::qdrant::Value {
                kind: Some(Kind::BoolValue(true)),
            },
        );
        assert_eq!(extract_string(&payload, "key"), None);
    }

    #[test]
    fn content_hash_empty_description() {
        let tool = McpTool {
            server_id: "s".into(),
            name: "t".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
        };
        let hash = content_hash(&tool);
        assert!(!hash.is_empty());
    }

    #[test]
    fn tool_point_id_empty_key() {
        let id = tool_point_id("");
        assert!(uuid::Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn sync_stats_all_fields_settable() {
        let stats = SyncStats {
            added: 10,
            updated: 20,
            removed: 5,
            unchanged: 100,
        };
        assert_eq!(stats.added, 10);
        assert_eq!(stats.updated, 20);
        assert_eq!(stats.removed, 5);
        assert_eq!(stats.unchanged, 100);
    }
}
