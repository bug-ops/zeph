// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Public API types and internal API wire types for the Claude provider.

use serde::{Deserialize, Serialize};

// ── Public types ──────────────────────────────────────────────────────────────

use zeph_config::ThinkingEffort;

pub(super) struct ThinkingCapability {
    /// Requires `interleaved-thinking-2025-05-14` beta header when `tool_use` is present.
    pub needs_interleaved_beta: bool,
    /// Opus 4.6 uses `effort` instead of `budget_tokens`; `Extended` config is auto-converted.
    pub prefers_effort: bool,
}

pub(super) fn thinking_capability(model: &str) -> ThinkingCapability {
    // Sonnet 4.6 with tools needs `interleaved-thinking-2025-05-14` beta header.
    let needs_interleaved_beta = model.contains("claude-sonnet-4-6");
    let prefers_effort = model.contains("claude-opus-4-6");
    ThinkingCapability {
        needs_interleaved_beta,
        prefers_effort,
    }
}

/// Maps a `budget_tokens` value to a `ThinkingEffort` level for models that prefer effort-based thinking.
pub(super) fn budget_to_effort(budget_tokens: u32) -> ThinkingEffort {
    if budget_tokens < 5_000 {
        ThinkingEffort::Low
    } else if budget_tokens < 15_000 {
        ThinkingEffort::Medium
    } else {
        ThinkingEffort::High
    }
}

// ── Cache markers ─────────────────────────────────────────────────────────────

pub(super) const CACHE_MARKER_STABLE: &str = "<!-- cache:stable -->";
pub(super) const CACHE_MARKER_TOOLS: &str = "<!-- cache:tools -->";
pub(super) const CACHE_MARKER_VOLATILE: &str = "<!-- cache:volatile -->";

/// Stable agent identity section injected into Block 1 when the base prompt
/// is below the Claude cache minimum (2048 tokens for Sonnet, 4096 for Opus/Haiku).
///
/// This text is purely descriptive and never changes between requests,
/// making it ideal for padding the cacheable block.
pub(super) const AGENT_IDENTITY_PREAMBLE: &str = concat!(
    "\n## Agent Identity\n\nYou are Zeph, a lightweight AI agent built on a hybrid inference architecture.\nZeph version: ",
    env!("CARGO_PKG_VERSION"),
    r#"
Crate: zeph (binary) backed by zeph-core, zeph-llm, zeph-skills, zeph-memory,
       zeph-channels, zeph-tools, and optional feature-gated crates.

## Core Architecture

Zeph is structured as a Rust workspace with a skills-first design:

- **zeph-core**: Agent loop, configuration, channel trait, context builder,
  metrics collection, vault integration, and output redaction.
- **zeph-llm**: `LlmProvider` trait with Ollama, Claude, OpenAI, and Candle
  (GGUF/safetensors) backends, plus an EMA-based router and orchestrator.
- **zeph-skills**: SKILL.md parser (agentskills.io spec), embedding-based
  semantic matcher, hot-reload via `notify`, and self-learning evolution.
- **zeph-memory**: Dual-store with SQLite (conversation history) and Qdrant
  (semantic vector search), plus a SemanticMemory orchestrator.
- **zeph-channels**: Telegram adapter (teloxide + streaming), CLI channel.
- **zeph-tools**: `ToolExecutor` trait, `ShellExecutor`, `WebScrapeExecutor`,
  `CompositeExecutor`, and an audit trail layer.
- **zeph-mcp**: MCP client via `rmcp`, multi-server lifecycle management,
  Qdrant-backed tool registry. Feature-gated.
- **zeph-a2a**: A2A protocol client and server, agent discovery, JSON-RPC 2.0.
  Feature-gated.
- **zeph-tui**: ratatui-based TUI dashboard with real-time metrics.
  Feature-gated.
- **zeph-scheduler**: Cron-based periodic task scheduler with SQLite persistence.
  Feature-gated.
- **zeph-gateway**: HTTP gateway for webhook ingestion with bearer auth.
  Feature-gated.
- **zeph-index**: AST-based code indexing and semantic retrieval.
  Feature-gated.

## Operational Principles

Zeph operates under the following core principles:

1. **Hybrid inference**: Requests are routed to the most appropriate provider
   based on capability requirements, latency, and historical success rates.
   The EMA router tracks per-provider rolling averages and prefers high-
   performing providers. A Thompson Sampling option enables exploration.

2. **Skills-first dispatch**: Before invoking generic tool use, Zeph matches
   the user request against its skill registry. Skills are Markdown documents
   following the SKILL.md specification. The semantic matcher uses embedding
   cosine similarity to rank candidates. Self-learning updates skill weights
   based on outcome feedback.

3. **Semantic memory**: All conversations are persisted to SQLite. Semantically
   relevant past exchanges are retrieved from Qdrant and injected into the
   context window. This enables cross-session continuity without bloating
   the prompt with raw history.

4. **Tool safety**: The `ToolExecutor` trait wraps all tool invocations with
   an audit layer. Shell commands are executed in a sandboxed environment.
   Web scraping respects robots.txt. All tool outputs are redacted for
   secrets before returning to the model.

5. **Multi-channel I/O**: Zeph supports CLI, Telegram, and TUI channels.
   The `AnyChannel` enum in `main.rs` dispatches to the appropriate adapter.
   All channels implement the `Channel` trait defined in `zeph-core`.

6. **MCP integration**: The MCP client connects to external tool servers
   using the Model Context Protocol (2025-11-25 spec). Tools are discovered
   at connection time and registered in the Qdrant tool registry for semantic
   lookup. Per-server declarative policies control which tools may be called.

7. **A2A protocol**: Zeph can act as both A2A client and server, enabling
   agent-to-agent task delegation. The protocol uses JSON-RPC 2.0 transport.

8. **Vault integration**: API keys and secrets are stored in an encrypted
   vault (age encryption). The `VaultProvider` resolves secret references
   in configuration at startup.

9. **Output filtering**: A multi-stage `FilterPipeline` sanitizes LLM output
   before delivery. `CommandMatcher` variants (Exact/Prefix/Regex/Custom)
   match filter triggers. `FilterConfidence` levels (Full/Partial/Fallback)
   indicate match certainty.

10. **Observability**: All operations are instrumented with `tracing` spans
    and events. Optional OpenTelemetry export is available via the `otel`
    feature flag. The TUI dashboard surfaces live metrics.

## Configuration Model

Zeph is configured via TOML (`config.toml`) with `ZEPH_*` environment
variable overrides. The configuration tree mirrors the workspace structure:

- `[agent]`: Core agent settings, instruction files, context window limits.
- `[llm]`: Provider selection, model names, thinking mode, token budgets.
- `[llm.router]`: Routing strategy (ema/thompson), provider chain, weights.
- `[memory]`: SQLite path, Qdrant URL, embedding model, eviction policy.
- `[tools]`: Enabled tool executors, shell sandbox settings, scrape limits.
- `[mcp]`: MCP server list, each with id, url, and per-server policy.
- `[a2a]`: A2A server bind address, discovery endpoints.
- `[scheduler]`: Cron jobs, SQLite state path, sweep interval.
- `[gateway]`: HTTP bind address, bearer token, allowed webhook sources.
- `[tui]`: Refresh rate, color theme, key bindings.

## Instruction File Loading

Zeph loads instruction files at startup to augment the system prompt:

- `zeph.md` and `.zeph/zeph.md` are always loaded (provider-agnostic).
- Provider-specific files: `CLAUDE.md` for Claude, `AGENTS.md` for others.
- Additional files can be specified via `[agent.instructions] extra_files`.
- All files are subject to a 256 KiB size cap and symlink boundary checks.
- Instruction content is injected into the volatile prompt block (Block 2),
  after the environment context and before the skills catalog.

## Prompt Caching Strategy

System prompts are split into cacheable and volatile blocks using HTML
comment markers:

- `<!-- cache:stable -->`: Breakpoint after the base agent identity block.
- `<!-- cache:tools -->`: Breakpoint after the skills and tool catalog.
- `<!-- cache:volatile -->`: Separator before the volatile context block.

Blocks before the volatile marker receive `cache_control: ephemeral`.
The volatile block (environment context, instruction files, retrieved memory)
is never cached as it changes with each request.

Claude's cache hierarchy processes `tools` → `system` → `messages` in order.
The minimum cacheable size is 2048 tokens for Sonnet and 4096 for Opus/Haiku.

## Tool Execution Model

Zeph's tool execution follows a layered model designed for safety and auditability:

### Layer 1: Tool Registry
All available tools are registered in the `CompositeExecutor`. Each tool
implements the `ToolExecutor` trait, which provides:
- `name() -> &str`: Unique tool identifier used in model requests.
- `description() -> &str`: Human-readable description for the model.
- `parameters_schema() -> Value`: JSON Schema defining input parameters.
- `execute(params: Value) -> Result<ToolOutput>`: Async execution entry point.

### Layer 2: Policy Enforcement
Before any tool executes, the `FilteredToolExecutor` wrapper checks:
1. The `ToolPolicy` (AllowList, DenyList, or Unrestricted) from agent config.
2. The subagent-specific `disallowed_tools` denylist from frontmatter.
Both checks must pass for execution to proceed.

### Layer 3: Audit Trail
Every tool invocation is recorded in the audit log with:
- Timestamp (UTC, ISO 8601).
- Tool name and sanitized parameters (secrets redacted).
- Duration in milliseconds.
- Success/failure status and error message if applicable.

### Layer 4: Output Redaction
Tool output passes through the redaction pipeline before returning to the model.
The `SecurityPatterns` registry (17 compiled regexes across 6 categories) scans
for API keys, tokens, passwords, private keys, connection strings, and PII.
Matches are replaced with `[REDACTED:<category>]` placeholders.

## Memory Architecture

### SQLite Store
The `SqliteStore` provides conversation-scoped persistent memory:
- Messages are stored with role, content, timestamp, and metadata.
- Conversations are identified by UUID session identifiers.
- Full-text search is available via SQLite FTS5 extension.
- Soft-delete semantics: entries are marked with `deleted_at` rather than
  physically removed, enabling crash-safe eviction with Qdrant consistency.

### Qdrant Vector Store
The `EmbeddingStore` provides cross-session semantic memory:
- Each message is embedded using the configured embedding model.
- Vectors are stored in Qdrant with payload containing message metadata.
- Retrieval uses approximate nearest-neighbor search (HNSW index).
- Results are ranked by cosine similarity and filtered by relevance threshold.
- The `SemanticMemory` orchestrator coordinates between both stores.

### Memory Eviction
The Ebbinghaus forgetting curve policy scores entries by:
  `score = exp(-t / (S * ln(1 + n)))`
where `t` = time since last access (seconds), `S` = retention strength,
`n` = access count. Low-scoring entries are soft-deleted from SQLite first,
then removed from Qdrant in a subsequent sweep phase. This two-phase approach
ensures consistency even if the agent crashes between phases.

## Self-Learning Skill Evolution

Zeph improves skill selection over time using three complementary mechanisms:

### Feedback Detection
The `FeedbackDetector` analyzes user messages for outcome signals:
- Explicit positive signals: "great", "perfect", "that worked", etc.
- Explicit negative signals: "wrong", "that didn't work", "try again", etc.
- Implicit signals via Jaccard similarity between expected and actual output.
- Correction patterns: "I meant...", "actually...", "you should have...".

### Wilson Score Re-ranking
Skill candidates are re-ranked using Wilson score lower bound:
  `score = (p_hat + z²/2n - z*sqrt(p_hat*(1-p_hat)/n + z²/4n²)) / (1 + z²/n)`
where `p_hat` = success rate, `n` = invocation count, `z` = 1.96 (95% CI).
This penalizes skills with few invocations (high uncertainty) relative to
well-tested skills, preventing premature exploitation.

### Provider EMA Routing
The EMA router tracks per-provider rolling averages of:
- Latency (milliseconds per token).
- Success rate (non-error responses / total requests).
- Cost efficiency (output tokens / input tokens ratio).
The router selects the provider with the best composite EMA score,
with configurable decay factor alpha (default 0.1).

## Security Model

### Vault Integration
All secrets in configuration are referenced by vault identifiers:
  `${vault:key_name}` — resolved at startup from the age-encrypted vault file.
The vault file is stored at `~/.zeph/vault.age` by default.
Resolution happens once during `VaultProvider::resolve()` before any
network connections are established.

### Symlink Boundary Checks
File loading operations (instruction files, skill definitions, subagent
definitions) enforce symlink boundary checks:
- The canonical path of each file is computed via `std::fs::canonicalize()`.
- Files must reside within the project root or designated user config directories.
- Symlinks pointing outside these boundaries are rejected with an error.
- This prevents path traversal attacks via crafted symlink chains.

### Input Sanitization
All skill names, tool names, and user-provided identifiers are sanitized
before inclusion in XML-structured prompts:
- `<`, `>`, `&`, `"`, `'` are HTML-escaped.
- Null bytes and control characters are stripped.
- Names exceeding 256 bytes are truncated with a warning.

## MCP Protocol Integration

The MCP client follows the Model Context Protocol specification (2025-11-25):

### Connection Lifecycle
1. `McpManager::connect()` initiates connections to all configured servers.
2. Each server gets its own `McpClient` with an `rmcp` transport session.
3. On connect, the client calls `list_tools()` to discover available tools.
4. Discovered tools are registered in the Qdrant tool registry for semantic lookup.
5. On disconnect, tools are deregistered and the session is cleaned up.

### Tool Invocation
`McpClient::call_tool(server_id, tool_name, params)` flow:
1. `PolicyEnforcer::check()` validates against per-server policy.
2. Rate limit sliding window is updated.
3. The rmcp transport forwards the JSON-RPC `tools/call` request.
4. The response is deserialized and returned as `ToolOutput`.
5. Violations are logged via `tracing::warn!` with structured fields.

## A2A Protocol Integration

The A2A (Agent-to-Agent) protocol enables Zeph to delegate tasks to peer agents:

### As A2A Client
`A2aClient::send_task(task)` serializes a task request as JSON-RPC 2.0,
sends it to the peer agent's endpoint, and awaits the result. Tasks include
a unique UUID, description, inputs, and expected output schema.

### As A2A Server
`A2aServer::listen()` binds to the configured address and handles incoming
task requests. Each request is dispatched to the agent loop for processing.
Results are returned as JSON-RPC 2.0 responses with the original request ID.

### Agent Discovery
Agents advertise their capabilities via a discovery endpoint (`GET /.well-known/agent.json`).
The discovery document lists supported task types, input/output schemas, and
the agent's public endpoint URL.
"#
);

// ── Context management ────────────────────────────────────────────────────────

/// Request field for Claude server-side context management (compact-2026-01-12 beta).
///
/// Note: the API does not accept a top-level `"type"` discriminator on this object — only
/// `trigger` and `pause_after_compaction` are allowed.
#[derive(Serialize, Clone, Debug)]
pub(super) struct ContextManagement {
    pub trigger: ContextManagementTrigger,
    pub pause_after_compaction: bool,
}

#[derive(Serialize, Clone, Debug)]
pub(super) struct ContextManagementTrigger {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub value: u32,
}

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug)]
pub(super) struct SystemContentBlock {
    #[serde(rename = "type")]
    pub block_type: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub(super) enum CacheType {
    Ephemeral,
}

use zeph_config::CacheTtl;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(super) struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: CacheType,
    /// Extended TTL for the cached prefix. Omitted when `None` (default ~5 min ephemeral).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CacheTtl>,
}

/// Serialization-only parameter for Claude's `thinking` request field.
#[derive(Serialize)]
pub(super) struct ThinkingParam {
    #[serde(rename = "type")]
    pub thinking_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// Serialization-only parameter for Claude's `output_config` request field.
/// Used to convey the effort level for adaptive thinking.
#[derive(Serialize)]
pub(super) struct OutputConfig {
    pub effort: ThinkingEffort,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(super) struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum AnthropicContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    /// Server-side compaction block returned by the Claude API (compact-2026-01-12 beta).
    /// Must be preserved verbatim and sent back in subsequent turns.
    Compaction {
        summary: String,
    },
}

#[derive(Serialize, Debug)]
pub(super) struct StructuredApiMessage {
    pub role: String,
    pub content: StructuredContent,
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
pub(super) enum StructuredContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Serialize)]
pub(super) struct ApiMessage<'a> {
    pub role: &'a str,
    pub content: &'a str,
}

#[derive(Deserialize)]
pub(super) struct ApiResponse {
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
pub(super) struct ContentBlock {
    pub text: String,
}

#[derive(Deserialize)]
pub(super) struct ToolApiResponse {
    pub content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<ApiUsage>,
}

#[derive(Deserialize, Debug)]
#[allow(clippy::struct_field_names)]
pub(super) struct ApiUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

// ── Request body types ────────────────────────────────────────────────────────

#[derive(Serialize)]
pub(super) struct RequestBody<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemContentBlock>>,
    pub messages: &'a [ApiMessage<'a>],
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<ContextManagement>,
}

#[derive(Serialize)]
pub(super) struct VisionRequestBody<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemContentBlock>>,
    pub messages: &'a [StructuredApiMessage],
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<ContextManagement>,
}

#[derive(Serialize)]
pub(super) struct ToolRequestBody<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemContentBlock>>,
    pub messages: &'a [StructuredApiMessage],
    pub tools: &'a [serde_json::Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<ContextManagement>,
}

#[derive(Serialize)]
pub(super) struct AnthropicTool<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub input_schema: &'a serde_json::Value,
}

#[derive(Serialize)]
pub(super) struct TypedToolRequestBody<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemContentBlock>>,
    pub messages: &'a [StructuredApiMessage],
    pub tools: &'a [AnthropicTool<'a>],
    pub tool_choice: ToolChoice<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<ContextManagement>,
}

#[derive(Serialize)]
pub(super) struct ToolChoice<'a> {
    pub r#type: &'a str,
    pub name: &'a str,
}

// ── Validation ────────────────────────────────────────────────────────────────

pub(super) const MIN_MAX_TOKENS_WITH_THINKING: u32 = 16_000;
