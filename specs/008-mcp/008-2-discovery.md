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
  - "[[006-tools/spec]]"
---

# Spec: MCP Tool Discovery & Pruning

Semantic tool discovery, per-message pruning cache, collision detection, tool filtering.

## Overview

MCP servers expose hundreds of tools across multiple categories. Zeph discovers these at startup via `tools/list`, applies sanitization, and selects relevant tools per-message to reduce token overhead and prevent tool confusion.

## Key Invariants

**Always:**
- All server tools registered with full schema (name, description, input_schema)
- Tool collision detection triggers on registration: log and disambiguate same names from multiple servers
- Per-message tool set pruned via LLM call to reduce token overhead
- Tool list cache invalidated on server reconnection
- Pruning cache is single-slot: keyed on `(message_hash, tool_list_hash)`

**Never:**
- Pass full tool registry to LLM without semantic pruning
- Serve tools from collided names without disambiguation (e.g., "tool_x" from two servers ambiguous)
- Cache tool definitions after server restart without re-fetching

## Tool Registration & Collision Detection

At server startup, fetch and register tools via `tools/list`:

```rust
pub struct ToolCollision {
    pub name: String,               // Tool name
    pub servers: Vec<String>,       // Server IDs that define it
}

impl McpManager {
    /// Register tools from a server; detect collisions.
    pub async fn register_tools(&self, server_id: &str) -> Result<Vec<ToolCollision>> {
        let tools = /* fetch via tools/list */;
        
        let collisions = detect_collisions::<std::collections::hash_map::RandomState>(
            self.tool_registry.all_tools(),
            &tools,
        );
        
        if !collisions.is_empty() {
            tracing::warn!("Tool collisions detected: {:?}", collisions);
        }
        
        self.tool_registry.add_tools(&tools);
        Ok(collisions)
    }
}

/// Detect tool name collisions.
pub fn detect_collisions<S: BuildHasher>(
    existing: &[McpTool],
    new_tools: &[McpTool],
) -> Vec<ToolCollision> {
    // Returns list of name collisions across existing + new tools
}
```

Collision handling:
- Collisions are logged but do NOT block registration
- Disambiguation happens at tool invocation time via server prefix (e.g., `filesystem:read_file` vs `http:read_file`)
- Attestation (if `expected_tools` list provided) detects unexpected schema drifts

## Per-Message Tool Pruning Cache

`PruningCache` (`crates/zeph-mcp/src/pruning.rs`) selects relevant tools per LLM turn:

```rust
pub struct PruningCache { /* ... */ }

pub struct PruningParams {
    pub message_content_hash: u64,       // Hash of LLM message context
    pub tool_list_hash: u64,             // Hash of available tools
    pub strategy: ToolDiscoveryStrategy, // LLM-based or semantic
}

impl PruningCache {
    /// Get relevant tools for this turn (LLM-selected).
    pub async fn get_tools_for_turn(
        &mut self,
        params: &PruningParams,
        all_tools: &[McpTool],
    ) -> Result<Vec<McpTool>> {
        // Single-slot cache: if (message_hash, tool_list_hash) matches cached entry, return cached tools
        // Otherwise: call LLM to select relevant tools, cache result, return
        
        // Cache invocation: ask LLM "which of these tools are relevant to the user's query?"
        // Reduces token overhead: full schema of ~100 tools down to ~10 relevant ones
    }
}
```

Cache semantics:
- **Single-slot**: only one `(message_hash, tool_list_hash)` pair cached at a time
- **Keyed on message+tools**: if either message content or tool catalog changes, cache miss
- **Populated via LLM**: `ToolDiscoveryStrategy::Llm` calls an LLM to rank tools by relevance
- **Cleared on server reconnection**: tool list hash changes, cache invalidates automatically

Config:
```toml
[mcp]
pruning_enabled = true              # Enable per-message tool pruning
pruning_strategy = "llm"             # Or "semantic" (embedding-based)
max_tools_per_turn = 10              # Max tools returned by pruning
```

## Semantic Tool Indexing (Optional)

`SemanticToolIndex` (`crates/zeph-mcp/src/semantic_index.rs`) provides embedding-based ranking as an alternative to LLM-based pruning:

```rust
pub enum ToolDiscoveryStrategy {
    Llm,        // Ask LLM which tools are relevant (default)
    Semantic,   // Use cosine similarity on embeddings
}

pub struct SemanticToolIndex { /* ... */ }

impl SemanticToolIndex {
    /// Rank tools by semantic similarity to a query.
    pub fn search_tools(
        &self,
        query: &str,
        all_tools: &[McpTool],
    ) -> Result<Vec<(McpTool, f32)>> {
        // Compute embedding for query
        // Compute cosine similarity with each tool's embedding
        // Return sorted by relevance score
    }
}
```

Embeddings are computed once per tool at startup and cached in memory. No external index required.

## Tool Attestation

When `expected_tools` list is declared in server config, the registry validates against schema drift:

```rust
pub struct AttestationResult {
    pub missing: Vec<String>,        // Tools in expected_tools not found
    pub unexpected: Vec<String>,     // Tools found but not in expected_tools
    pub schema_changed: Vec<String>, // Tools present but schema differs
}

pub async fn attest_tools(
    server_id: &str,
    expected: &[String],
    actual: &[McpTool],
) -> Result<AttestationResult> {
    // Compare tool set and schemas; report mismatches
}
```

Attestation warnings are logged but do NOT block tool usage. This helps detect when a server's tool catalog unexpectedly changes between deployments.

## Tool Fingerprinting

`ToolFingerprint` computes a content hash for tool definitions to enable change detection:

```rust
pub struct ToolFingerprint {
    pub name: String,
    pub schema_hash: u64,            // Hash of input_schema JSON
    pub description_hash: u64,       // Hash of description text
}

pub fn compute_fingerprint(tool: &McpTool) -> ToolFingerprint { /* ... */ }
```

Used by attestation and trust scoring to detect parameter/description drift.

## Tool Sanitization

All tool definitions (names, descriptions, input schemas) pass through `sanitize_tools()` before registration to scrub injection patterns — see [[008-3-security]].

## Integration Points

- [[008-1-lifecycle]] — Tools fetched during startup; cache invalidated on reconnection
- [[008-3-security]] — Sanitization applied to tool descriptions and parameter schemas
- [[006-tools/spec]] — Tool dispatch via `ToolExecutor` trait

## See Also

- [[008-mcp/spec]] — Parent
- [[008-1-lifecycle]] — Server connection lifecycle
- [[008-3-security]] — Tool sanitization and security checks
