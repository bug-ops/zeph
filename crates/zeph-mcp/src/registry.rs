pub use zeph_memory::SyncStats;
use zeph_memory::{Embeddable, EmbeddingRegistry, QdrantOps};

pub use zeph_llm::provider::EmbedFuture;

use crate::error::McpError;
use crate::tool::McpTool;

const COLLECTION_NAME: &str = "zeph_mcp_tools";

const MCP_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x7a, 0x65, 0x70, 0x68, // "zeph"
    0x2d, 0x6d, 0x63, 0x70, // "-mcp"
    0x2d, 0x74, 0x6f, 0x6f, // "-too"
    0x6c, 0x73, 0x00, 0x01, // "ls\0\x01"
]);

/// Wrapper that caches the qualified name so [`Embeddable::key`] can return `&str`.
struct McpToolRef<'a> {
    tool: &'a McpTool,
    qualified: String,
    hash: String,
}

impl<'a> McpToolRef<'a> {
    fn new(tool: &'a McpTool) -> Self {
        let qualified = tool.qualified_name();
        let hash = compute_hash(tool);
        Self {
            tool,
            qualified,
            hash,
        }
    }
}

impl Embeddable for McpToolRef<'_> {
    fn key(&self) -> &str {
        &self.qualified
    }

    fn content_hash(&self) -> String {
        self.hash.clone()
    }

    fn embed_text(&self) -> &str {
        &self.tool.description
    }

    fn to_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "key": self.qualified,
            "server_id": self.tool.server_id,
            "tool_name": self.tool.name,
            "description": self.tool.description,
        })
    }
}

fn compute_hash(tool: &McpTool) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(tool.server_id.as_bytes());
    hasher.update(tool.name.as_bytes());
    hasher.update(tool.description.as_bytes());
    hasher.update(tool.input_schema.to_string().as_bytes());
    hasher.finalize().to_hex().to_string()
}

pub struct McpToolRegistry {
    registry: EmbeddingRegistry,
}

impl std::fmt::Debug for McpToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolRegistry")
            .field("collection", &COLLECTION_NAME)
            .finish_non_exhaustive()
    }
}

impl McpToolRegistry {
    /// # Errors
    ///
    /// Returns an error if the Qdrant client cannot be created.
    pub fn new(qdrant_url: &str) -> Result<Self, McpError> {
        let ops = QdrantOps::new(qdrant_url)?;
        Ok(Self {
            registry: EmbeddingRegistry::new(ops, COLLECTION_NAME, MCP_NAMESPACE),
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
        let refs: Vec<McpToolRef<'_>> = tools.iter().map(McpToolRef::new).collect();
        let stats = self
            .registry
            .sync(&refs, embedding_model, |text| {
                let fut = embed_fn(text);
                Box::pin(async move {
                    fut.await
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                }) as zeph_memory::EmbedFuture
            })
            .await
            .map_err(|e| McpError::Embedding(e.to_string()))?;
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
                tracing::warn!("Qdrant MCP tool search failed: {e:#}");
                return Vec::new();
            }
        };

        results
            .into_iter()
            .filter_map(|point| {
                let server_id = point.payload.get("server_id")?.as_str()?.to_owned();
                let name = point.payload.get("tool_name")?.as_str()?.to_owned();
                let description = point
                    .payload
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                Some(McpTool {
                    server_id,
                    name,
                    description,
                    input_schema: serde_json::Value::Object(serde_json::Map::new()),
                })
            })
            .collect()
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
    fn mcp_tool_ref_key() {
        let tool = make_tool("github", "create_issue");
        let r = McpToolRef::new(&tool);
        assert_eq!(r.key(), "github:create_issue");
    }

    #[test]
    fn mcp_tool_ref_embed_text() {
        let tool = make_tool("s", "t");
        let r = McpToolRef::new(&tool);
        assert_eq!(r.embed_text(), "test");
    }

    #[test]
    fn mcp_tool_ref_payload_has_key() {
        let tool = make_tool("github", "create_issue");
        let r = McpToolRef::new(&tool);
        let payload = r.to_payload();
        assert_eq!(payload["key"], "github:create_issue");
    }

    #[test]
    fn content_hash_deterministic() {
        let tool = make_tool("github", "create_issue");
        let h1 = compute_hash(&tool);
        let h2 = compute_hash(&tool);
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_changes_on_modification() {
        let t1 = make_tool("github", "create_issue");
        let mut t2 = make_tool("github", "create_issue");
        t2.description = "modified".into();
        assert_ne!(compute_hash(&t1), compute_hash(&t2));
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
        assert_ne!(compute_hash(&t1), compute_hash(&t2));
    }

    #[test]
    fn content_hash_different_schema() {
        let t1 = make_tool("s", "t");
        let mut t2 = make_tool("s", "t");
        t2.input_schema = serde_json::json!({"type": "object"});
        assert_ne!(compute_hash(&t1), compute_hash(&t2));
    }

    #[test]
    fn sync_stats_default() {
        let stats = SyncStats::default();
        assert_eq!(stats.added, 0);
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
    fn registry_new_with_invalid_url_fails() {
        let result = McpToolRegistry::new("not a valid url");
        assert!(result.is_err());
    }

    #[test]
    fn mcp_namespace_is_valid_uuid() {
        assert!(!MCP_NAMESPACE.is_nil());
    }

    #[test]
    fn content_hash_length_is_blake3_hex() {
        let tool = make_tool("server", "tool");
        let hash = compute_hash(&tool);
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
        assert_eq!(compute_hash(&t1), compute_hash(&t2));
    }

    #[test]
    fn content_hash_different_name_different_hash() {
        let t1 = make_tool("s", "tool_a");
        let t2 = make_tool("s", "tool_b");
        assert_ne!(compute_hash(&t1), compute_hash(&t2));
    }

    #[test]
    fn collection_name_constant() {
        assert_eq!(COLLECTION_NAME, "zeph_mcp_tools");
    }

    #[tokio::test]
    async fn search_empty_registry_returns_empty() {
        let registry = McpToolRegistry::new("http://localhost:6334").unwrap();
        let embed_fn = |_: &str| -> EmbedFuture {
            Box::pin(async { Err(zeph_llm::LlmError::Other("no qdrant".into())) })
        };
        let results = registry.search("test query", 5, embed_fn).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_with_embedding_failure_returns_empty() {
        let registry = McpToolRegistry::new("http://localhost:6334").unwrap();
        let embed_fn = |_: &str| -> EmbedFuture {
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
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![0.1, 0.2, 0.3]) }) };
        let results = registry.search("query", 0, embed_fn).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn sync_with_unreachable_qdrant_fails() {
        let mut registry = McpToolRegistry::new("http://127.0.0.1:1").unwrap();
        let tools = vec![make_tool("server", "tool")];
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![0.1, 0.2, 0.3]) }) };
        let result = registry.sync(&tools, "test-model", embed_fn).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sync_with_empty_tools_and_unreachable_qdrant_fails() {
        let mut registry = McpToolRegistry::new("http://127.0.0.1:1").unwrap();
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![0.1, 0.2, 0.3]) }) };
        let result = registry.sync(&[], "test-model", embed_fn).await;
        assert!(result.is_err());
    }
}
