---
aliases:
  - MCP Tool Discovery
  - Semantic Tool Filtering
  - Tool Collision Detection
  - Per-Message Tool Pruning
tags:
  - sdd
  - spec
  - mcp
  - protocol
  - tools
created: 2026-04-10
status: complete
related:
  - "[[008-mcp/spec]]"
  - "[[008-1-lifecycle]]"
  - "[[008-3-security]]"
  - "[[006-tools]]"
---

# Spec: MCP Tool Discovery & Pruning

Semantic tool discovery, per-message pruning cache, collision detection, tool filtering.

## Overview

MCP servers expose hundreds of tools across multiple categories. Zeph discovers these at startup and applies semantic filtering per-message to reduce token overhead and prevent tool confusion.

## Key Invariants

**Always:**
- All server tools registered with full schema (name, description, input_schema)
- Tool collision detection triggers on registration: same name from multiple servers
- Per-message tool set pruned based on context relevance (embedding similarity)
- Tool discovery errors are non-fatal: server remains active even if tool listing fails

**Never:**
- Include duplicate tool names (same name from multiple servers) without disambiguation
- Pass full tool registry to LLM (always apply semantic pruning)
- Cache tool descriptions without versioning (invalidate on server restart)

## Tool Registration & Collision Detection

At server startup, fetch and register tools:

```rust
async fn discover_tools(&self, server: &McpServer) -> Result<Vec<ToolDefinition>> {
    // 1. Request tool list from server
    let tools = server.connection.list_tools().await?;
    
    // 2. Validate and enrich schemas
    let mut discovered = Vec::new();
    for tool in tools {
        match validate_json_schema(&tool.input_schema) {
            Ok(schema) => {
                let enriched = ToolDefinition {
                    id: format!("{}:{}", server.name, tool.name),
                    name: tool.name.clone(),
                    description: tool.description,
                    input_schema: schema,
                    server_id: server.id.clone(),
                };
                discovered.push(enriched);
            }
            Err(e) => {
                log::warn!("Invalid tool schema for {}.{}: {}", server.name, tool.name, e);
                // Continue; don't fail entire discovery
            }
        }
    }
    
    // 3. Detect collisions
    self.detect_collisions(&discovered)?;
    
    Ok(discovered)
}

fn detect_collisions(&self, tools: &[ToolDefinition]) -> Result<()> {
    let mut by_name: HashMap<&str, Vec<&ToolDefinition>> = HashMap::new();
    
    for tool in tools {
        by_name.entry(&tool.name)
            .or_insert_with(Vec::new)
            .push(tool);
    }
    
    for (name, defs) in by_name {
        if defs.len() > 1 {
            log::warn!(
                "Tool name collision: '{}' defined in {}",
                name,
                defs.iter()
                    .map(|d| d.server_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            
            // Auto-rename to disambiguate: "tool_name" → "server1:tool_name"
            // (Handled at invocation time)
        }
    }
    
    Ok(())
}
```

## Per-Message Tool Pruning

Semantic filtering reduces LLM token overhead:

```rust
pub struct ToolPruningCache {
    // Query embedding → recommended tool IDs (cached)
    cache: Arc<Mutex<lru::LruCache<String, Vec<String>>>>,
    embedding_provider: Arc<LlmProvider>,
    tool_embeddings: Arc<HashMap<String, Vec<f32>>>,  // cached at startup
    top_k: usize,  // default: 10
}

impl ToolPruningCache {
    async fn prune_tools_for_context(
        &self,
        context: &str,      // user query + recent history
        all_tools: &[ToolDefinition],
    ) -> Result<Vec<&ToolDefinition>> {
        // 1. Generate query embedding
        let query_embedding = self.embedding_provider
            .embed_text(context)
            .await?;
        
        // 2. Check cache
        let cache_key = format!("{:?}", &query_embedding[..10]);  // simple hash
        if let Some(cached) = self.cache.lock().get(&cache_key) {
            let pruned: Vec<_> = cached.iter()
                .filter_map(|id| all_tools.iter().find(|t| &t.id == id))
                .collect();
            return Ok(pruned);
        }
        
        // 3. Semantic similarity: dot product with tool embeddings
        let mut scores: Vec<(usize, f32)> = all_tools
            .iter()
            .enumerate()
            .map(|(i, tool)| {
                let embedding = self.tool_embeddings.get(&tool.id)
                    .map(|e| dot_product(&query_embedding, e))
                    .unwrap_or(0.0);
                (i, embedding)
            })
            .collect();
        
        // 4. Select top-K by score
        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let selected: Vec<String> = scores
            .into_iter()
            .take(self.top_k)
            .map(|(i, _)| all_tools[i].id.clone())
            .collect();
        
        // 5. Cache result
        self.cache.lock().put(cache_key, selected.clone());
        
        let pruned: Vec<_> = selected.iter()
            .filter_map(|id| all_tools.iter().find(|t| &t.id == id))
            .collect();
        
        Ok(pruned)
    }
}
```

## Tool Embedding Precomputation

Cache at startup for fast lookup:

```rust
async fn precompute_tool_embeddings(
    tools: &[ToolDefinition],
    provider: &LlmProvider,
) -> Result<HashMap<String, Vec<f32>>> {
    let mut embeddings = HashMap::new();
    
    // Embed tool description + input schema summary
    for tool in tools {
        let text = format!(
            "Tool: {}\nDescription: {}\nInputs: {:?}",
            tool.name,
            tool.description,
            tool.input_schema.properties.keys().collect::<Vec<_>>(),
        );
        
        let embedding = provider.embed_text(&text).await?;
        embeddings.insert(tool.id.clone(), embedding);
    }
    
    Ok(embeddings)
}
```

## Configuration

```toml
[mcp.discovery]
enabled = true

# Per-message pruning
pruning_enabled = true
top_k_tools = 10              # keep top 10 most relevant
cache_size = 1000             # LRU cache entries

# Collision handling
collision_mode = "disambiguate"  # or "error", "first"
```

## Integration Points

- [[008-1-lifecycle]] — Tool discovery runs after server startup
- [[008-3-security]] — Pruning cache prevents tool injection via descriptions
- [[006-tools]] — Pruned tool subset sent to ToolExecutor
- [[003-llm-providers]] — Embedding provider used for semantic filtering

## See Also

- [[008-mcp/spec]] — Parent
- [[008-1-lifecycle]] — Server initialization
- [[008-3-security]] — Injection defense during pruning
- [[006-tools]] — Tool execution after pruning
