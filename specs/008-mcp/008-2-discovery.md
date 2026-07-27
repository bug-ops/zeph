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

At server startup, fetch and register tools via `tools/list` and scan for name collisions:

```rust
pub struct ToolCollision {
    pub sanitized_id: String,       // Collision identifier
    pub server_a: String,            // First server ID
    pub qualified_a: String,         // Qualified name: "server_a:tool_name"
    pub trust_a: McpTrustLevel,      // Trust level of first server
    pub server_b: String,            // Second server ID
    pub qualified_b: String,         // Qualified name: "server_b:tool_name"
    pub trust_b: McpTrustLevel,      // Trust level of second server
}

/// Detect tool name collisions across all registered tools.
///
/// The `trust_map` provides trust levels for each server (missing servers default to `Untrusted`).
pub fn detect_collisions<S: BuildHasher>(
    tools: &[McpTool],
    trust_map: &HashMap<String, McpTrustLevel, S>,
) -> Vec<ToolCollision> {
    // Scan all tools for matching sanitized IDs
    // Report each collision with both servers' trust levels
    // Used to assess risk of ambiguous tool dispatch
}
```

Collision handling:
- Collisions are logged at registration but do NOT block tool loading
- Disambiguation happens at tool invocation time via server prefix (e.g., `filesystem:read_file` vs `http:read_file`)
- Both `trust_a` and `trust_b` are recorded to assess data-flow risk via policy enforcement

## Per-Message Tool Pruning Cache

`PruningCache` (`crates/zeph-mcp/src/pruning.rs`) caches LLM-selected relevant tools per turn:

```rust
pub struct PruningCache {
    // Fields are private; external interface is only new() and reset()
}

pub struct PruningParams {
    /// Maximum number of MCP tools to include after pruning
    pub max_tools: usize,
    /// Minimum number of tools below which pruning is skipped (use all)
    pub min_tools_to_prune: usize,
    /// Tool names that are always included regardless of relevance ranking
    pub always_include: Vec<String>,
}

/// Prune tools using LLM-based ranking (single-slot cache).
/// 
/// Call signature varies by discovery strategy; for LLM pruning:
pub async fn prune_tools_cached<P: LlmProvider>(
    cache: &mut PruningCache,
    all_tools: &[McpTool],
    task_context: &str,
    params: &PruningParams,
    provider: &P,
) -> Result<Vec<McpTool>, PruningError> {
    // Single-slot cache: check (message_hash, tool_list_hash) key
    // On miss: call LLM "which of these tools are relevant to: {task_context}?"
    // Reduces token overhead: ~100 tools down to ~10 relevant ones
}
```

Cache semantics:
- **Single-slot**: only one `(message_hash, tool_list_hash)` pair cached at a time
- **Keyed on content+catalog**: if either changes, cache invalidates
- **LLM-populated**: invokes a model-provider to rank tools by relevance
- **Always-include pinned**: `always_include` tool names bypass relevance filtering

**Configuration**:
```toml
[mcp.pruning]
enabled = false                # Enable per-message tool pruning (default: false)
max_tools = 15                 # Max tools returned after pruning
min_tools_to_prune = 10        # Skip pruning if fewer than this many available
pruning_provider = ""          # LLM provider for pruning (empty = use default)
# always_include = ["critical_tool"]  # Tools always present

[mcp.tool_discovery]
strategy = "none"              # "none" (default, no pruning), "llm" (LLM ranking), "embedding" (semantic)
top_k = 10                     # Number of top tools to include per query (embedding strategy only)
min_similarity = 0.2           # Minimum cosine similarity threshold (embedding strategy only)
embedding_provider = ""        # LLM provider for embeddings (empty = use default)
```

## Semantic Tool Indexing (Optional)

`SemanticToolIndex` (`crates/zeph-mcp/src/semantic_index.rs`) provides embedding-based ranking as an alternative to LLM-based pruning:

```rust
pub enum ToolDiscoveryStrategy {
    /// No pruning; all tools provided to the LLM
    None,
    /// Ask LLM which tools are relevant (via pruning_provider)
    Llm,
    /// Embedding-based semantic ranking (requires embedding model)
    Embedding,
}

pub struct SemanticToolIndex { /* fields private */ }

impl SemanticToolIndex {
    /// Build an embedding-based tool index.
    pub async fn build<F>(
        tools: &[McpTool],
        embed_fn: &F,
    ) -> Result<Self, SemanticIndexError>
    where
        F: Fn(&str) -> zeph_llm::provider::EmbedFuture + Send + Sync,
    {
        // Compute and cache embeddings for all tool descriptions
    }

    /// Select relevant tools by semantic similarity to a query embedding.
    pub fn select(
        &self,
        query_embedding: &[f32],
        top_k: usize,
        min_similarity: f32,
        always_include: &[String],  // Tool names always included
    ) -> Vec<McpTool> {
        // Rank by cosine similarity; return top-k + always_include
    }
}
```

**Embeddings** are computed once at startup and cached in memory. Query embedding is computed on-demand from the task context.

## Tool Attestation

When `expected_tools` list is declared in server config, the registry validates against schema drift:

```rust
pub type ToolFingerprint = String;  // Blake3 hex digest

#[non_exhaustive]
pub enum AttestationResult {
    /// All actual tools match the operator-declared expected set.
    Verified {
        fingerprints: HashMap<String, ToolFingerprint>,
    },
    /// Server returned tools not in `expected_tools`.
    Unexpected {
        unexpected_tools: Vec<String>,
        fingerprints: HashMap<String, ToolFingerprint>,
    },
    /// No `expected_tools` declared — attestation skipped.
    Unconfigured,
}

/// Attestation at tool registration time (sync, no Result).
pub fn attest_tools(
    tools: &[McpTool],
    expected_tools: &[String],
    previous_fingerprints: Option<&HashMap<String, ToolFingerprint>>,
) -> AttestationResult {
    // Compare actual tool set against expected_tools
    // Compute fingerprints; detect schema drift from previous session
}

/// Compute Blake3 fingerprint of a tool's schema (name + description + input_schema)
pub fn fingerprint_tool(tool: &McpTool) -> ToolFingerprint {
    // Returns hex digest string
}
```

**Attestation outcomes**:
- **Verified**: all actual tools in expected set; fingerprints match previous → no warnings
- **Unexpected**: actual tools not in expected set → warning logged (tool still usable)
- **Unconfigured**: no expected_tools declared → attestation skipped
- **Schema drift**: fingerprints differ from previous session → warning logged

Attestation does NOT block tool loading; it aids operators in detecting catalog changes between deployments.

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
