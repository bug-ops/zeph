// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Qdrant-backed semantic tool registry for MCP tool discovery.
//!
//! [`McpToolRegistry`] syncs MCP tool descriptions to Qdrant as embedding vectors,
//! enabling semantic search ("find tools relevant to this task") across all connected
//! servers. Tool embeddings are delta-synced: only changed tools are upserted.
//!
//! This is the persistent/Qdrant path. For a lighter in-memory alternative, see
//! [`crate::semantic_index::SemanticToolIndex`].

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

/// Owned wrapper that caches the qualified name so [`Embeddable::key`] can return `&str`.
///
/// Using owned fields (no lifetime parameter) makes this type `Send`, which is required
/// for `EmbeddingRegistry::sync` to produce a `Send` future.
struct McpToolOwned {
    qualified: String,
    hash: String,
    description: String,
    server_id: String,
    tool_name: String,
    embed_text: String,
}

impl McpToolOwned {
    fn new(tool: &McpTool) -> Self {
        let qualified = tool.qualified_name();
        let hash = compute_hash(tool);
        let embed_text = format!("{}: {}", tool.name, tool.description);
        Self {
            qualified,
            hash,
            description: tool.description.clone(),
            server_id: tool.server_id.clone(),
            tool_name: tool.name.clone(),
            embed_text,
        }
    }
}

impl Embeddable for McpToolOwned {
    fn key(&self) -> &str {
        &self.qualified
    }

    fn content_hash(&self) -> String {
        self.hash.clone()
    }

    fn embed_text(&self) -> &str {
        &self.embed_text
    }

    fn to_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "key": self.qualified,
            "server_id": self.server_id,
            "tool_name": self.tool_name,
            "description": self.description,
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

/// Qdrant-backed registry for MCP tool embeddings.
///
/// Stores a semantic embedding for each MCP tool in a dedicated Qdrant collection
/// (`zeph_mcp_tools`). Embeddings are keyed by `qualified_name` (`"server_id:name"`)
/// and content-hashed so unchanged tools are never re-embedded.
///
/// # Usage pattern
///
/// ```no_run
/// use zeph_mcp::registry::McpToolRegistry;
/// use zeph_memory::QdrantOps;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let ops = QdrantOps::new("http://localhost:6334")?;
/// let mut registry = McpToolRegistry::with_ops(ops);
/// // Sync tools after connect_all():
/// // registry.sync(&tools, "nomic-embed-text", embed_fn).await?;
/// // Search for relevant tools:
/// // let hits = registry.search("read a file", 5, embed_fn).await;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
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
    /// Create a `McpToolRegistry` from a pre-built `QdrantOps` instance.
    #[must_use]
    pub fn with_ops(ops: QdrantOps) -> Self {
        Self {
            registry: EmbeddingRegistry::new(ops, COLLECTION_NAME, MCP_NAMESPACE),
        }
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
        let refs: Vec<McpToolOwned> = tools.iter().map(McpToolOwned::new).collect();
        let stats = self
            .registry
            .sync(
                &refs,
                embedding_model,
                |text| {
                    let fut = embed_fn(text);
                    Box::pin(async move {
                        fut.await
                            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                    }) as zeph_memory::EmbedFuture
                },
                None,
            )
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

    /// Search for MCP tools relevant to a natural-language query using Qdrant vector search.
    ///
    /// Returns up to `limit` tools sorted by embedding similarity to `query`.
    /// On embedding failure or Qdrant error the method logs at `WARN` and returns an
    /// empty `Vec` — the caller should fall back to the full tool list.
    ///
    /// Note: returned tools have an empty `input_schema` because Qdrant payloads only
    /// store the description fields needed for prompt construction.
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
                    output_schema: None,
                    security_meta: crate::tool::ToolSecurityMeta::default(),
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
            output_schema: None,
            security_meta: crate::tool::ToolSecurityMeta::default(),
        }
    }

    #[test]
    fn mcp_tool_owned_key() {
        let tool = make_tool("github", "create_issue");
        let r = McpToolOwned::new(&tool);
        assert_eq!(r.key(), "github:create_issue");
    }

    #[test]
    fn mcp_tool_owned_embed_text() {
        let tool = make_tool("s", "t");
        let r = McpToolOwned::new(&tool);
        assert_eq!(r.embed_text(), "t: test");
    }

    #[test]
    fn mcp_tool_owned_payload_has_key() {
        let tool = make_tool("github", "create_issue");
        let r = McpToolOwned::new(&tool);
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
            output_schema: None,
            security_meta: crate::tool::ToolSecurityMeta::default(),
        };
        let t2 = McpTool {
            server_id: "server-b".into(),
            name: "tool".into(),
            description: "test".into(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: crate::tool::ToolSecurityMeta::default(),
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

    fn make_registry(url: &str) -> McpToolRegistry {
        let ops = QdrantOps::new(url).unwrap();
        McpToolRegistry::with_ops(ops)
    }

    #[test]
    fn registry_construction_with_ops() {
        let _registry = make_registry("http://localhost:6334");
    }

    #[test]
    fn content_hash_length_is_blake3_hex() {
        let tool = make_tool("server", "tool");
        let hash = compute_hash(&tool);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn content_hash_different_name_different_hash() {
        let t1 = make_tool("s", "tool_a");
        let t2 = make_tool("s", "tool_b");
        assert_ne!(compute_hash(&t1), compute_hash(&t2));
    }

    #[tokio::test]
    async fn search_empty_registry_returns_empty() {
        let registry = make_registry("http://localhost:6334");
        let embed_fn = |_: &str| -> EmbedFuture {
            Box::pin(async { Err(zeph_llm::LlmError::Other("no qdrant".into())) })
        };
        let results = registry.search("test query", 5, embed_fn).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_with_embedding_failure_returns_empty() {
        let registry = make_registry("http://localhost:6334");
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
        let registry = make_registry("http://localhost:6334");
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![0.1, 0.2, 0.3]) }) };
        let results = registry.search("query", 0, embed_fn).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn sync_with_unreachable_qdrant_fails() {
        let mut registry = make_registry("http://127.0.0.1:1");
        let tools = vec![make_tool("server", "tool")];
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![0.1, 0.2, 0.3]) }) };
        let result = registry.sync(&tools, "test-model", embed_fn).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sync_with_empty_tools_and_unreachable_qdrant_fails() {
        let mut registry = make_registry("http://127.0.0.1:1");
        let embed_fn = |_: &str| -> EmbedFuture { Box::pin(async { Ok(vec![0.1, 0.2, 0.3]) }) };
        let result = registry.sync(&[], "test-model", embed_fn).await;
        assert!(result.is_err());
    }
}
