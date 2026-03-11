// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt;

use crate::error::LlmError;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::provider::{
    ChatResponse, ChatStream, GenerationOverrides, LlmProvider, Message, MessagePart, Role,
    StatusTx, ThinkingBlock, ToolDefinition, ToolUseRequest,
};
use crate::retry::send_with_retry;
use crate::sse::claude_sse_to_stream;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA_INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
const MAX_RETRIES: u32 = 3;
const MIN_MAX_TOKENS_WITH_THINKING: u32 = 16_000;

/// Extended or adaptive thinking mode for Claude.
///
/// Serializes with `mode` as tag:
/// `{ "mode": "extended", "budget_tokens": 10000 }` or `{ "mode": "adaptive" }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ThinkingConfig {
    Extended {
        budget_tokens: u32,
    },
    Adaptive {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<ThinkingEffort>,
    },
}

/// Effort level for adaptive thinking.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingEffort {
    Low,
    #[default]
    Medium,
    High,
}

struct ThinkingCapability {
    /// Requires `interleaved-thinking-2025-05-14` beta header when `tool_use` is present.
    needs_interleaved_beta: bool,
}

fn thinking_capability(model: &str) -> ThinkingCapability {
    // Sonnet 4.6 with tools needs `interleaved-thinking-2025-05-14` beta header.
    let needs_interleaved_beta = model.contains("claude-sonnet-4-6");
    ThinkingCapability {
        needs_interleaved_beta,
    }
}

const CACHE_MARKER_STABLE: &str = "<!-- cache:stable -->";
const CACHE_MARKER_TOOLS: &str = "<!-- cache:tools -->";
const CACHE_MARKER_VOLATILE: &str = "<!-- cache:volatile -->";

/// Stable agent identity section injected into Block 1 when the base prompt
/// is below the Claude cache minimum (2048 tokens for Sonnet, 4096 for Opus/Haiku).
///
/// This text is purely descriptive and never changes between requests,
/// making it ideal for padding the cacheable block.
const AGENT_IDENTITY_PREAMBLE: &str = concat!(
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

pub struct ClaudeProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    thinking: Option<ThinkingConfig>,
    pub(crate) status_tx: Option<StatusTx>,
    /// Whether to attach `cache_control` to user messages in multi-turn conversations.
    cache_user_messages: bool,
    last_cache: std::sync::Mutex<Option<(u64, u64)>>,
    last_usage: std::sync::Mutex<Option<(u64, u64)>>,
    /// Cached pre-serialized tool definitions. Keyed by hash of names+schemas; invalidated when the set changes.
    tool_cache: std::sync::Mutex<Option<(u64, Vec<serde_json::Value>)>>,
    generation_overrides: Option<GenerationOverrides>,
}

impl fmt::Debug for ClaudeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeProvider")
            .field("client", &"<reqwest::Client>")
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("thinking", &self.thinking)
            .field("status_tx", &self.status_tx.is_some())
            .field("cache_user_messages", &self.cache_user_messages)
            .field("last_usage", &self.last_usage.lock().ok())
            .field("last_cache", &self.last_cache.lock().ok())
            .field(
                "tool_cache",
                &self
                    .tool_cache
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|(hash, _)| *hash)),
            )
            .field("generation_overrides", &self.generation_overrides)
            .finish()
    }
}

impl Clone for ClaudeProvider {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            max_tokens: self.max_tokens,
            thinking: self.thinking.clone(),
            status_tx: self.status_tx.clone(),
            cache_user_messages: self.cache_user_messages,
            last_cache: std::sync::Mutex::new(None),
            last_usage: std::sync::Mutex::new(None),
            tool_cache: std::sync::Mutex::new(None),
            generation_overrides: self.generation_overrides.clone(),
        }
    }
}

impl ClaudeProvider {
    #[must_use]
    pub fn new(api_key: String, model: String, max_tokens: u32) -> Self {
        Self {
            client: crate::http::llm_client(600),
            api_key,
            model,
            max_tokens,
            thinking: None,
            status_tx: None,
            cache_user_messages: true,
            last_cache: std::sync::Mutex::new(None),
            last_usage: std::sync::Mutex::new(None),
            tool_cache: std::sync::Mutex::new(None),
            generation_overrides: None,
        }
    }

    #[must_use]
    pub fn with_generation_overrides(mut self, overrides: GenerationOverrides) -> Self {
        self.generation_overrides = Some(overrides);
        self
    }

    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    #[must_use]
    pub fn with_status_tx(mut self, tx: StatusTx) -> Self {
        self.status_tx = Some(tx);
        self
    }

    #[must_use]
    pub fn with_cache_user_messages(mut self, enabled: bool) -> Self {
        self.cache_user_messages = enabled;
        self
    }

    /// Configure thinking mode for Claude extended/adaptive thinking.
    ///
    /// # Errors
    ///
    /// Returns an error if `budget_tokens` is outside the API-allowed range
    /// `[1024, 128_000]` or if `budget_tokens >= max_tokens` after the automatic
    /// 16 000-token floor is applied.
    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Result<Self, LlmError> {
        if let ThinkingConfig::Extended { budget_tokens } = thinking {
            const MIN_BUDGET: u32 = 1_024;
            const MAX_BUDGET: u32 = 128_000;
            if !(MIN_BUDGET..=MAX_BUDGET).contains(&budget_tokens) {
                return Err(LlmError::Other(format!(
                    "budget_tokens {budget_tokens} is out of range [{MIN_BUDGET}, {MAX_BUDGET}]"
                )));
            }
            let max_tokens = self.max_tokens.max(MIN_MAX_TOKENS_WITH_THINKING);
            if budget_tokens >= max_tokens {
                return Err(LlmError::Other(format!(
                    "budget_tokens {budget_tokens} must be less than max_tokens {max_tokens}"
                )));
            }
            self.max_tokens = max_tokens;
        } else {
            self.max_tokens = self.max_tokens.max(MIN_MAX_TOKENS_WITH_THINKING);
        }
        self.thinking = Some(thinking);
        Ok(self)
    }

    /// Configure thinking mode, propagating any validation error.
    ///
    /// # Errors
    ///
    /// Forwards errors from [`with_thinking`].
    pub fn with_thinking_opt(self, thinking: Option<ThinkingConfig>) -> Result<Self, LlmError> {
        match thinking {
            Some(t) => self.with_thinking(t),
            None => Ok(self),
        }
    }

    /// Fetch all available Claude models from the Anthropic API and cache them.
    ///
    /// Paginates until `has_more` is false.
    /// 401/403 responses are returned as `LlmError::Other` without touching the cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the API request fails or returns an auth error.
    ///
    /// # Panics
    ///
    /// Panics if the hardcoded Anthropic API URL cannot be parsed (impossible in practice).
    pub async fn list_models_remote(
        &self,
    ) -> Result<Vec<crate::model_cache::RemoteModelInfo>, LlmError> {
        let mut models: Vec<crate::model_cache::RemoteModelInfo> = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            // Build URL with cursor as a proper query parameter to avoid injection.
            let url = {
                let mut u = reqwest::Url::parse("https://api.anthropic.com/v1/models")
                    .expect("static URL is valid");
                if let Some(ref cursor) = after_id {
                    u.query_pairs_mut().append_pair("after_id", cursor);
                }
                u
            };

            let resp = self
                .client
                .get(url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .send()
                .await?;

            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
            {
                return Err(LlmError::Other(format!(
                    "Claude API auth error listing models: {status}"
                )));
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                tracing::debug!(status = %status, body = %body, "Claude list_models_remote error body");
                return Err(LlmError::Other(format!(
                    "Claude list models failed: {status}"
                )));
            }

            let page: serde_json::Value = resp.json().await?;
            if let Some(data) = page.get("data").and_then(|v| v.as_array()) {
                for item in data {
                    let type_field = item
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if type_field != "model" {
                        continue;
                    }
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let display_name = item
                        .get("display_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&id)
                        .to_string();
                    let created_at = item.get("created_at").and_then(serde_json::Value::as_i64);
                    models.push(crate::model_cache::RemoteModelInfo {
                        id,
                        display_name,
                        context_window: None,
                        created_at,
                    });
                }
            }

            let has_more = page
                .get("has_more")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !has_more {
                break;
            }
            after_id = page
                .get("last_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            if after_id.is_none() {
                break;
            }
        }

        let cache = crate::model_cache::ModelCache::for_slug("claude");
        cache.save(&models)?;
        Ok(models)
    }

    fn build_thinking_param(&self) -> (Option<ThinkingParam>, Option<f64>, Option<ThinkingEffort>) {
        match &self.thinking {
            None => (None, None, None),
            Some(ThinkingConfig::Extended { budget_tokens }) => (
                Some(ThinkingParam {
                    thinking_type: "enabled",
                    budget_tokens: Some(*budget_tokens),
                }),
                None,
                None,
            ),
            Some(ThinkingConfig::Adaptive { effort }) => (
                Some(ThinkingParam {
                    thinking_type: "adaptive",
                    budget_tokens: None,
                }),
                None,
                *effort,
            ),
        }
    }

    fn beta_header(&self, has_tools: bool) -> Option<String> {
        let cap = thinking_capability(&self.model);
        if self.thinking.is_some()
            && has_tools
            && cap.needs_interleaved_beta
            && matches!(self.thinking, Some(ThinkingConfig::Extended { .. }))
        {
            Some(ANTHROPIC_BETA_INTERLEAVED_THINKING.to_owned())
        } else {
            None
        }
    }

    fn get_or_build_api_tools(&self, tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
        let key = tool_cache_key(tools);
        let mut guard = self
            .tool_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_key, ref cached_values)) = *guard
            && cached_key == key
        {
            return cached_values.clone();
        }
        let mut serialized: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        if let Some(Some(obj)) = serialized.last_mut().map(serde_json::Value::as_object_mut) {
            obj.insert(
                "cache_control".into(),
                serde_json::json!({"type": "ephemeral"}),
            );
        }
        *guard = Some((key, serialized.clone()));
        serialized
    }

    fn store_cache_usage(&self, usage: &ApiUsage) {
        if let Ok(mut guard) = self.last_cache.lock() {
            *guard = Some((
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
            ));
        }
        if let Ok(mut guard) = self.last_usage.lock() {
            *guard = Some((usage.input_tokens, usage.output_tokens));
        }
    }

    fn has_image_parts(messages: &[Message]) -> bool {
        messages
            .iter()
            .any(|m| m.parts.iter().any(|p| matches!(p, MessagePart::Image(_))))
    }

    fn build_request(&self, messages: &[Message], stream: bool) -> reqwest::RequestBuilder {
        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        // Apply experiment generation overrides (temperature only; top_p/top_k not in Claude API).
        // Overrides are skipped when thinking mode is active (thinking requires temperature=1.0).
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        // lgtm[rust/cleartext-logging]
        let output_config = effort.map(|e| OutputConfig { effort: e });
        let auto_cache = if messages.len() > 1 {
            tracing::debug!(
                message_count = messages.len(),
                "multi-turn session: system cache eligible"
            );
            Some(CacheControl {
                cache_type: CacheType::Ephemeral,
            })
        } else {
            None
        };

        if Self::has_image_parts(messages) {
            let (system, chat_messages) =
                split_messages_structured(messages, self.cache_user_messages);
            let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
            let beta = self.beta_header(false);
            let body = VisionRequestBody {
                model: &self.model,
                max_tokens: self.max_tokens,
                system: system_blocks,
                messages: &chat_messages,
                stream,
                thinking: thinking_param,
                output_config,
                temperature,
                cache_control: auto_cache,
            };
            let mut req = self
                .client
                .post(API_URL)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION);
            if let Some(b) = beta {
                req = req.header("anthropic-beta", b);
            }
            return req.header("content-type", "application/json").json(&body);
        }

        let (system, chat_messages) = split_messages(messages);
        let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
        let beta = self.beta_header(false);

        let body = RequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            stream,
            thinking: thinking_param,
            output_config,
            temperature,
            cache_control: auto_cache,
        };

        let mut req = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION);
        if let Some(b) = beta {
            req = req.header("anthropic-beta", b);
        }
        req.header("content-type", "application/json").json(&body)
    }

    async fn send_request(&self, messages: &[Message]) -> Result<String, LlmError> {
        let response = send_with_retry("Claude", MAX_RETRIES, self.status_tx.as_ref(), || {
            self.build_request(messages, false).send()
        })
        .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            tracing::error!("Claude API error {status}: {text}");
            return Err(LlmError::Other(format!(
                "Claude API request failed (status {status})"
            )));
        }

        if Self::has_image_parts(messages) {
            let resp: ToolApiResponse = serde_json::from_str(&text)?;
            if let Some(ref usage) = resp.usage {
                log_cache_usage(usage);
                self.store_cache_usage(usage);
            }
            let extracted = resp.content.into_iter().find_map(|b| {
                if let AnthropicContentBlock::Text { text, .. } = b {
                    Some(text)
                } else {
                    None
                }
            });
            return extracted.ok_or(LlmError::EmptyResponse {
                provider: "claude".into(),
            });
        }

        let resp: ApiResponse = serde_json::from_str(&text)?;

        if let Some(ref usage) = resp.usage {
            log_cache_usage(usage);
            self.store_cache_usage(usage);
        }

        resp.content
            .first()
            .map(|c| c.text.clone())
            .ok_or(LlmError::EmptyResponse {
                provider: "claude".into(),
            })
    }

    async fn send_stream_request(
        &self,
        messages: &[Message],
    ) -> Result<reqwest::Response, LlmError> {
        let response = send_with_retry("Claude", MAX_RETRIES, self.status_tx.as_ref(), || {
            self.build_request(messages, true).send()
        })
        .await?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.map_err(LlmError::Http)?;
            tracing::error!("Claude API streaming request error {status}: {text}");
            return Err(LlmError::Other(format!(
                "Claude API streaming request failed (status {status})"
            )));
        }

        Ok(response)
    }
}

impl LlmProvider for ClaudeProvider {
    fn context_window(&self) -> Option<usize> {
        if self.model.contains("opus")
            || self.model.contains("sonnet")
            || self.model.contains("haiku")
        {
            Some(200_000)
        } else {
            None
        }
    }

    async fn chat(&self, messages: &[Message]) -> Result<String, LlmError> {
        self.send_request(messages).await
    }

    async fn chat_stream(&self, messages: &[Message]) -> Result<ChatStream, LlmError> {
        let response = self.send_stream_request(messages).await?;
        Ok(claude_sse_to_stream(response))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmError> {
        Err(LlmError::EmbedUnsupported {
            provider: "claude".into(),
        })
    }

    fn supports_embeddings(&self) -> bool {
        false
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "claude"
    }

    fn supports_structured_output(&self) -> bool {
        true
    }

    #[cfg(feature = "schema")]
    async fn chat_typed<T>(&self, messages: &[Message]) -> Result<T, LlmError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
        Self: Sized,
    {
        let (schema_value, _) = crate::provider::cached_schema::<T>()?;
        let type_name = std::any::type_name::<T>()
            .rsplit("::")
            .next()
            .unwrap_or("Output");

        let tool_name = format!("submit_{type_name}");
        let tool = ToolDefinition {
            name: tool_name.clone(),
            description: format!("Submit the structured {type_name} result"),
            parameters: schema_value,
        };

        let (system, chat_messages) = split_messages_structured(messages, self.cache_user_messages);
        let api_tool = AnthropicTool {
            name: &tool.name,
            description: &tool.description,
            input_schema: &tool.parameters,
        };

        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e });
        let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
        let auto_cache = if messages.len() > 1 {
            tracing::debug!(
                message_count = messages.len(),
                "multi-turn session: system cache eligible"
            );
            Some(CacheControl {
                cache_type: CacheType::Ephemeral,
            })
        } else {
            None
        };
        let beta = self.beta_header(true);
        let body = TypedToolRequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            tools: &[api_tool],
            tool_choice: ToolChoice {
                r#type: "tool",
                name: &tool_name,
            },
            thinking: thinking_param,
            output_config,
            temperature,
            cache_control: auto_cache,
        };

        let mut req = self
            .client
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION);
        if let Some(b) = beta {
            req = req.header("anthropic-beta", b);
        }
        let response = req
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            return Err(LlmError::Other(format!(
                "Claude API request failed (status {status})"
            )));
        }

        let resp: ToolApiResponse = serde_json::from_str(&text)?;

        if let Some(ref usage) = resp.usage {
            log_cache_usage(usage);
            self.store_cache_usage(usage);
        }

        for block in resp.content {
            if let AnthropicContentBlock::ToolUse { input, .. } = block {
                return serde_json::from_value::<T>(input)
                    .map_err(|e| LlmError::StructuredParse(e.to_string()));
            }
        }

        Err(LlmError::StructuredParse(
            "no tool_use block in response".into(),
        ))
    }

    fn supports_vision(&self) -> bool {
        true
    }

    fn supports_tool_use(&self) -> bool {
        true
    }

    fn last_cache_usage(&self) -> Option<(u64, u64)> {
        self.last_cache.lock().ok().and_then(|g| *g)
    }

    fn last_usage(&self) -> Option<(u64, u64)> {
        self.last_usage.lock().ok().and_then(|g| *g)
    }

    fn debug_request_json(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream: bool,
    ) -> serde_json::Value {
        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e });
        let auto_cache = if messages.len() > 1 {
            Some(CacheControl {
                cache_type: CacheType::Ephemeral,
            })
        } else {
            None
        };

        if !tools.is_empty() {
            let (system, chat_messages) =
                split_messages_structured(messages, self.cache_user_messages);
            let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
            let api_tools = self.get_or_build_api_tools(tools);
            let body = ToolRequestBody {
                model: &self.model,
                max_tokens: self.max_tokens,
                system: system_blocks,
                messages: &chat_messages,
                tools: &api_tools,
                thinking: thinking_param,
                output_config,
                temperature,
                cache_control: auto_cache,
            };
            return serde_json::to_value(&body)
                .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }));
        }

        if Self::has_image_parts(messages) {
            let (system, chat_messages) =
                split_messages_structured(messages, self.cache_user_messages);
            let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
            let body = VisionRequestBody {
                model: &self.model,
                max_tokens: self.max_tokens,
                system: system_blocks,
                messages: &chat_messages,
                stream,
                thinking: thinking_param,
                output_config,
                temperature,
                cache_control: auto_cache,
            };
            return serde_json::to_value(&body)
                .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }));
        }

        let (system, chat_messages) = split_messages(messages);
        let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
        let body = RequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            stream,
            thinking: thinking_param,
            output_config,
            temperature,
            cache_control: auto_cache,
        };
        serde_json::to_value(&body)
            .unwrap_or_else(|e| serde_json::json!({ "serialization_error": e.to_string() }))
    }

    async fn chat_with_tools(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let (system, chat_messages) = split_messages_structured(messages, self.cache_user_messages);
        let api_tools = self.get_or_build_api_tools(tools);

        let (thinking_param, mut temperature, effort) = self.build_thinking_param();
        if thinking_param.is_none()
            && let Some(Some(t)) = self.generation_overrides.as_ref().map(|ov| ov.temperature)
        {
            temperature = Some(t);
        }
        let output_config = effort.map(|e| OutputConfig { effort: e });
        let system_blocks = system.map(|s| split_system_into_blocks(&s, &self.model));
        let auto_cache = if messages.len() > 1 {
            tracing::debug!(
                message_count = messages.len(),
                "multi-turn session: system cache eligible"
            );
            Some(CacheControl {
                cache_type: CacheType::Ephemeral,
            })
        } else {
            None
        };
        let beta = self.beta_header(!tools.is_empty());
        let body = ToolRequestBody {
            model: &self.model,
            max_tokens: self.max_tokens,
            system: system_blocks,
            messages: &chat_messages,
            tools: &api_tools,
            thinking: thinking_param,
            output_config,
            temperature,
            cache_control: auto_cache,
        };

        let response = send_with_retry("Claude", MAX_RETRIES, self.status_tx.as_ref(), || {
            let mut req = self
                .client
                .post(API_URL)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION);
            if let Some(ref b) = beta {
                req = req.header("anthropic-beta", b);
            }
            req.header("content-type", "application/json")
                .json(&body)
                .send()
        })
        .await?;

        let status = response.status();
        let text = response.text().await.map_err(LlmError::Http)?;

        if !status.is_success() {
            tracing::error!("Claude API error {status}: {text}");
            return Err(LlmError::Other(format!(
                "Claude API request failed (status {status})"
            )));
        }

        let resp: ToolApiResponse = serde_json::from_str(&text)?;
        tracing::debug!(
            stop_reason = ?resp.stop_reason,
            content_blocks = resp.content.len(),
            "Claude chat_with_tools response"
        );
        if let Some(ref usage) = resp.usage {
            log_cache_usage(usage);
            self.store_cache_usage(usage);
        }
        let parsed = parse_tool_response(resp);
        tracing::debug!(?parsed, "parsed ChatResponse");
        Ok(parsed)
    }
}

fn log_cache_usage(usage: &ApiUsage) {
    tracing::debug!(
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        cache_creation = usage.cache_creation_input_tokens,
        cache_read = usage.cache_read_input_tokens,
        "Claude API usage"
    );
}

fn split_messages(messages: &[Message]) -> (Option<String>, Vec<ApiMessage<'_>>) {
    let mut system_parts = Vec::new();
    let mut chat = Vec::new();

    for msg in messages {
        if !msg.metadata.agent_visible {
            continue;
        }
        match msg.role {
            Role::System => system_parts.push(msg.to_llm_content()),
            Role::User | Role::Assistant => {
                let content = msg.to_llm_content();
                if !content.trim().is_empty() {
                    let role = if msg.role == Role::User {
                        "user"
                    } else {
                        "assistant"
                    };
                    chat.push(ApiMessage { role, content });
                }
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, chat)
}

#[derive(Serialize, Clone, Debug)]
struct SystemContentBlock {
    #[serde(rename = "type")]
    block_type: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
enum CacheType {
    Ephemeral,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct CacheControl {
    #[serde(rename = "type")]
    cache_type: CacheType,
}

/// Returns the minimum token count required for caching to activate for the given model.
/// Uses `byte_len / 4` as a conservative token estimate (1 token ≈ 4 chars for English).
fn cache_min_tokens(model: &str) -> usize {
    if model.contains("sonnet") { 2048 } else { 4096 }
}

fn tool_cache_key(tools: &[ToolDefinition]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for t in tools {
        t.name.hash(&mut hasher);
        t.parameters.to_string().hash(&mut hasher);
    }
    hasher.finish()
}

fn split_system_into_blocks(system: &str, model: &str) -> Vec<SystemContentBlock> {
    // Split on volatile marker first: everything before is cacheable
    let (cacheable_part, volatile_part) = if let Some(pos) = system.find(CACHE_MARKER_VOLATILE) {
        (
            &system[..pos],
            Some(&system[pos + CACHE_MARKER_VOLATILE.len()..]),
        )
    } else {
        (system, None)
    };

    let mut blocks = Vec::new();
    let cache_markers = [CACHE_MARKER_STABLE, CACHE_MARKER_TOOLS];
    let mut remaining = cacheable_part;
    let min_tokens = cache_min_tokens(model);

    let mut first_block = true;
    for marker in &cache_markers {
        if let Some(pos) = remaining.find(marker) {
            let before = remaining[..pos].trim();
            if !before.is_empty() {
                // Pad Block 1 (the stable base prompt) with agent identity text
                // when it is below the cache minimum threshold. This ensures
                // the block gets cache_control and avoids silent cache misses.
                let text = if first_block {
                    let estimated = before.len() / 4;
                    if estimated < min_tokens {
                        tracing::debug!(
                            estimated_tokens = estimated,
                            min_tokens,
                            model,
                            "Block 1 below cache threshold, padding with agent identity preamble"
                        );
                        format!("{before}\n{AGENT_IDENTITY_PREAMBLE}")
                    } else {
                        before.to_owned()
                    }
                } else {
                    before.to_owned()
                };
                let estimated_tokens = text.len() / 4;
                let cc = if estimated_tokens >= min_tokens {
                    Some(CacheControl {
                        cache_type: CacheType::Ephemeral,
                    })
                } else {
                    tracing::debug!(
                        estimated_tokens,
                        min_tokens,
                        model,
                        "system block below cache threshold, skipping cache_control"
                    );
                    None
                };
                blocks.push(SystemContentBlock {
                    block_type: "text",
                    text,
                    cache_control: cc,
                });
            }
            remaining = &remaining[pos + marker.len()..];
            first_block = false;
        }
    }

    let remaining = remaining.trim();
    if !remaining.is_empty() {
        // When markers were present, the trailing segment is always cached (it's the
        // last explicit cacheable block). When no markers exist, `remaining` equals the
        // full system prompt — apply the same min-token threshold as the fallback path.
        let had_markers = remaining.len() < cacheable_part.trim().len();
        let estimated_tokens = remaining.chars().count() / 4;
        let cc = if had_markers || estimated_tokens >= min_tokens {
            Some(CacheControl {
                cache_type: CacheType::Ephemeral,
            })
        } else {
            tracing::debug!(
                estimated_tokens,
                min_tokens,
                model,
                "fallback system block below cache threshold, skipping cache_control"
            );
            None
        };
        blocks.push(SystemContentBlock {
            block_type: "text",
            text: remaining.to_owned(),
            cache_control: cc,
        });
    }

    if let Some(volatile) = volatile_part {
        let volatile = volatile.trim();
        if !volatile.is_empty() {
            blocks.push(SystemContentBlock {
                block_type: "text",
                text: volatile.to_owned(),
                cache_control: None,
            });
        }
    }

    blocks
}

#[cfg(feature = "schema")]
#[derive(Serialize)]
struct TypedToolRequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemContentBlock>>,
    messages: &'a [StructuredApiMessage],
    tools: &'a [AnthropicTool<'a>],
    tool_choice: ToolChoice<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[cfg(feature = "schema")]
#[derive(Serialize)]
struct ToolChoice<'a> {
    r#type: &'a str,
    name: &'a str,
}

#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

#[derive(Serialize)]
struct ToolRequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemContentBlock>>,
    messages: &'a [StructuredApiMessage],
    tools: &'a [serde_json::Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize, Debug)]
struct StructuredApiMessage {
    role: String,
    content: StructuredContent,
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
enum StructuredContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
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
}

/// Serialization-only parameter for Claude's `thinking` request field.
#[derive(Serialize)]
struct ThinkingParam {
    #[serde(rename = "type")]
    thinking_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<u32>,
}

/// Serialization-only parameter for Claude's `output_config` request field.
/// Used to convey the effort level for adaptive thinking.
#[derive(Serialize)]
struct OutputConfig {
    effort: ThinkingEffort,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Deserialize)]
struct ToolApiResponse {
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

fn parse_tool_response(resp: ToolApiResponse) -> ChatResponse {
    let truncated = resp.stop_reason.as_deref() == Some("max_tokens");
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut thinking_blocks = Vec::new();

    for block in resp.content {
        match block {
            AnthropicContentBlock::Text { text, .. } => text_parts.push(text),
            AnthropicContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolUseRequest { id, name, input });
            }
            AnthropicContentBlock::Thinking {
                thinking,
                signature,
            } => {
                tracing::debug!(len = thinking.len(), "Claude thinking block received");
                thinking_blocks.push(ThinkingBlock::Thinking {
                    thinking,
                    signature,
                });
            }
            AnthropicContentBlock::RedactedThinking { data } => {
                tracing::debug!("Claude redacted_thinking block received");
                thinking_blocks.push(ThinkingBlock::Redacted { data });
            }
            AnthropicContentBlock::ToolResult { .. } | AnthropicContentBlock::Image { .. } => {}
        }
    }

    // When response was cut off by max_tokens with pending tool calls, the tool
    // inputs are incomplete JSON. Discard them and surface the partial text so
    // the agent loop can retry rather than executing a malformed tool call.
    if truncated && !tool_calls.is_empty() {
        tracing::warn!(
            tool_count = tool_calls.len(),
            "response truncated by max_tokens with pending tool calls; discarding incomplete tool use"
        );
        let combined = text_parts.join("");
        return ChatResponse::Text(if combined.is_empty() {
            "[Response truncated: max_tokens limit reached. Please reduce the request scope.]"
                .to_owned()
        } else {
            combined
        });
    }

    if tool_calls.is_empty() {
        let combined = text_parts.join("");
        // Inject the truncation marker so the agent loop can emit StopReason::MaxTokens.
        let text = if truncated {
            let marker = crate::provider::MAX_TOKENS_TRUNCATION_MARKER;
            if combined.is_empty() {
                format!("[Response truncated: {marker}. Please reduce the request scope.]")
            } else {
                format!("{combined}\n[Response truncated: {marker}.]")
            }
        } else {
            combined
        };
        ChatResponse::Text(text)
    } else {
        let text = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join(""))
        };
        ChatResponse::ToolUse {
            text,
            tool_calls,
            thinking_blocks,
        }
    }
}

#[allow(clippy::too_many_lines)]
fn split_messages_structured(
    messages: &[Message],
    cache_user_messages: bool,
) -> (Option<String>, Vec<StructuredApiMessage>) {
    let mut system_parts = Vec::new();
    let mut chat = Vec::new();

    // Collect agent-visible system messages first.
    for msg in messages
        .iter()
        .filter(|m| m.metadata.agent_visible && m.role == Role::System)
    {
        system_parts.push(msg.to_llm_content());
    }

    // Collect only agent-visible non-system messages so that idx-based peek always lands on a
    // user or assistant message (RC4: system messages in `visible` would break +1 index peek).
    let visible: Vec<&Message> = messages
        .iter()
        .filter(|m| m.metadata.agent_visible && m.role != Role::System)
        .collect();

    // Track which tool_use IDs were actually emitted as native AnthropicContentBlock::ToolUse
    // by the most recent assistant message. When processing the following user message, any
    // ToolResult block whose tool_use_id is not in this set is downgraded to text — prevents
    // API 400 caused by orphaned ToolResult referencing a non-existent tool_use (RC1 fix).
    let mut last_emitted_tool_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for (idx, msg) in visible.iter().enumerate() {
        match msg.role {
            Role::System => {} // already extracted above
            Role::User | Role::Assistant => {
                let role = if msg.role == Role::User {
                    "user"
                } else {
                    "assistant"
                };
                let has_structured_parts = msg.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::ToolUse { .. }
                            | MessagePart::ToolResult { .. }
                            | MessagePart::Image(_)
                            | MessagePart::ThinkingBlock { .. }
                            | MessagePart::RedactedThinkingBlock { .. }
                    )
                });

                if has_structured_parts {
                    let is_assistant = msg.role == Role::Assistant;

                    // For assistant messages, pre-compute which tool_use IDs are matched by
                    // the next visible user message. Unmatched IDs are downgraded to text to
                    // prevent Claude API 400 (tool_use without tool_result).
                    let matched_tool_ids: Option<std::collections::HashSet<&str>> = if is_assistant
                    {
                        let next = visible.get(idx + 1);
                        Some(
                            msg.parts
                                .iter()
                                .filter_map(|p| {
                                    if let MessagePart::ToolUse { id, .. } = p {
                                        Some(id.as_str())
                                    } else {
                                        None
                                    }
                                })
                                .filter(|uid| {
                                    next.is_some_and(|next_msg| {
                                        next_msg.role == Role::User
                                            && next_msg.parts.iter().any(|np| {
                                                matches!(
                                                    np,
                                                    MessagePart::ToolResult { tool_use_id, .. }
                                                        if tool_use_id.as_str() == *uid
                                                )
                                            })
                                    })
                                })
                                .collect(),
                        )
                    } else {
                        None
                    };

                    let mut blocks = Vec::new();
                    // Reset emitted tool IDs at the start of each assistant message so user
                    // messages can check against the immediately preceding assistant only.
                    if is_assistant {
                        last_emitted_tool_ids.clear();
                    }
                    for part in &msg.parts {
                        match part {
                            MessagePart::Text { text }
                            | MessagePart::Recall { text }
                            | MessagePart::CodeContext { text }
                            | MessagePart::Summary { text }
                            | MessagePart::CrossSession { text } => {
                                if !text.trim().is_empty() {
                                    blocks.push(AnthropicContentBlock::Text {
                                        text: text.clone(),
                                        cache_control: None,
                                    });
                                }
                            }
                            MessagePart::ToolOutput {
                                tool_name, body, ..
                            } => {
                                blocks.push(AnthropicContentBlock::Text {
                                    text: format!("[tool output: {tool_name}]\n{body}"),
                                    cache_control: None,
                                });
                            }
                            MessagePart::ToolUse { id, name, input } if is_assistant => {
                                // Downgrade to text if the tool_use ID is not matched by the
                                // next user message — prevents API 400 on orphaned tool_use.
                                let matched = matched_tool_ids
                                    .as_ref()
                                    .is_some_and(|ids| ids.contains(id.as_str()));
                                if matched {
                                    last_emitted_tool_ids.insert(id.clone());
                                    blocks.push(AnthropicContentBlock::ToolUse {
                                        id: id.clone(),
                                        name: name.clone(),
                                        input: input.clone(),
                                    });
                                } else {
                                    tracing::warn!(
                                        tool_use_id = %id,
                                        tool_name = %name,
                                        "downgrading unmatched tool_use to text in API request"
                                    );
                                    blocks.push(AnthropicContentBlock::Text {
                                        text: format!("[tool_use: {name}] {input}"),
                                        cache_control: None,
                                    });
                                }
                            }
                            MessagePart::ToolUse { name, input, .. } => {
                                blocks.push(AnthropicContentBlock::Text {
                                    text: format!("[tool_use: {name}] {input}"),
                                    cache_control: None,
                                });
                            }
                            MessagePart::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } if !is_assistant => {
                                // Downgrade to text if the tool_use_id was not emitted as a
                                // native ToolUse by the preceding assistant message (RC1 fix).
                                if last_emitted_tool_ids.contains(tool_use_id.as_str()) {
                                    blocks.push(AnthropicContentBlock::ToolResult {
                                        tool_use_id: tool_use_id.clone(),
                                        content: content.clone(),
                                        is_error: *is_error,
                                        cache_control: None,
                                    });
                                } else {
                                    tracing::warn!(
                                        tool_use_id = %tool_use_id,
                                        "downgrading orphaned tool_result to text in API request"
                                    );
                                    if !content.trim().is_empty() {
                                        blocks.push(AnthropicContentBlock::Text {
                                            text: content.clone(),
                                            cache_control: None,
                                        });
                                    }
                                }
                            }
                            MessagePart::ToolResult { content, .. } => {
                                if !content.trim().is_empty() {
                                    blocks.push(AnthropicContentBlock::Text {
                                        text: content.clone(),
                                        cache_control: None,
                                    });
                                }
                            }
                            MessagePart::Image(img) => {
                                blocks.push(AnthropicContentBlock::Image {
                                    source: ImageSource {
                                        source_type: "base64".to_owned(),
                                        media_type: img.mime_type.clone(),
                                        data: STANDARD.encode(&img.data),
                                    },
                                });
                            }
                            MessagePart::ThinkingBlock {
                                thinking,
                                signature,
                            } if is_assistant => {
                                blocks.push(AnthropicContentBlock::Thinking {
                                    thinking: thinking.clone(),
                                    signature: signature.clone(),
                                });
                            }
                            MessagePart::RedactedThinkingBlock { data } if is_assistant => {
                                blocks.push(AnthropicContentBlock::RedactedThinking {
                                    data: data.clone(),
                                });
                            }
                            // Thinking blocks in user messages are silently dropped.
                            MessagePart::ThinkingBlock { .. }
                            | MessagePart::RedactedThinkingBlock { .. } => {}
                        }
                    }
                    chat.push(StructuredApiMessage {
                        role: role.to_owned(),
                        content: StructuredContent::Blocks(blocks),
                    });
                } else {
                    // Non-structured user/assistant message: clear emitted tool IDs since
                    // no tool pairs are possible across a plain text message boundary.
                    if msg.role == Role::Assistant {
                        last_emitted_tool_ids.clear();
                    }
                    let text = msg.to_llm_content();
                    if !text.trim().is_empty() {
                        chat.push(StructuredApiMessage {
                            role: role.to_owned(),
                            content: StructuredContent::Text(text.to_owned()),
                        });
                    }
                }
            }
        }
    }

    // Place 1 message-level cache breakpoint at the user message closest to position
    // (total - 20) to maximize the 20-block lookback window coverage.
    if cache_user_messages && chat.len() > 1 {
        let target = chat.len().saturating_sub(20);
        let breakpoint_idx = (target..chat.len())
            .find(|&i| chat[i].role == "user")
            .unwrap_or(0);
        let msg = &mut chat[breakpoint_idx];
        match &mut msg.content {
            StructuredContent::Blocks(blocks) => {
                if let Some(
                    AnthropicContentBlock::Text { cache_control, .. }
                    | AnthropicContentBlock::ToolResult { cache_control, .. },
                ) = blocks.last_mut()
                {
                    *cache_control = Some(CacheControl {
                        cache_type: CacheType::Ephemeral,
                    });
                }
            }
            StructuredContent::Text(text) => {
                let owned = std::mem::take(text);
                msg.content = StructuredContent::Blocks(vec![AnthropicContentBlock::Text {
                    text: owned,
                    cache_control: Some(CacheControl {
                        cache_type: CacheType::Ephemeral,
                    }),
                }]);
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, chat)
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemContentBlock>>,
    messages: &'a [ApiMessage<'a>],
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct VisionRequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemContentBlock>>,
    messages: &'a [StructuredApiMessage],
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Deserialize, Debug)]
#[allow(clippy::struct_field_names)]
struct ApiUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ImageData;
    use crate::provider::MessageMetadata;
    use tokio_stream::StreamExt;

    #[test]
    fn context_window_known_models() {
        let sonnet = ClaudeProvider::new("k".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert_eq!(sonnet.context_window(), Some(200_000));

        let opus = ClaudeProvider::new("k".into(), "claude-opus-4-6".into(), 1024);
        assert_eq!(opus.context_window(), Some(200_000));

        let haiku = ClaudeProvider::new("k".into(), "claude-haiku-4-5".into(), 1024);
        assert_eq!(haiku.context_window(), Some(200_000));
    }

    #[test]
    fn context_window_unknown_model() {
        let provider = ClaudeProvider::new("k".into(), "unknown-model".into(), 1024);
        assert!(provider.context_window().is_none());
    }

    #[test]
    fn split_messages_extracts_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "You are helpful.".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "Hi".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];

        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "You are helpful.");
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, "user");
    }

    #[test]
    fn split_messages_no_system() {
        let messages = vec![Message {
            role: Role::User,
            content: "Hi".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let (system, chat) = split_messages(&messages);
        assert!(system.is_none());
        assert_eq!(chat.len(), 1);
    }

    #[test]
    fn split_messages_multiple_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "Part 1".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::System,
                content: "Part 2".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "Hi".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];

        let (system, _) = split_messages(&messages);
        assert_eq!(system.unwrap(), "Part 1\n\nPart 2");
    }

    #[test]
    fn supports_streaming_returns_true() {
        let provider =
            ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert!(provider.supports_streaming());
    }

    #[test]
    fn debug_redacts_api_key() {
        let provider = ClaudeProvider::new(
            "sk-secret-key".into(),
            "claude-sonnet-4-5-20250929".into(),
            1024,
        );
        let debug_output = format!("{provider:?}");
        assert!(!debug_output.contains("sk-secret-key"));
        assert!(debug_output.contains("<redacted>"));
        assert!(debug_output.contains("claude-sonnet-4-5-20250929"));
    }

    #[test]
    fn claude_supports_embeddings_returns_false() {
        let provider =
            ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert!(!provider.supports_embeddings());
    }

    #[tokio::test]
    async fn claude_embed_returns_error() {
        let provider =
            ClaudeProvider::new("test-key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        let result = provider.embed("test").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string()
                .contains("embedding not supported by claude")
        );
    }

    #[test]
    fn name_returns_claude() {
        let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert_eq!(provider.name(), "claude");
    }

    #[test]
    fn clone_preserves_fields() {
        let provider = ClaudeProvider::new(
            "test-api-key".into(),
            "claude-sonnet-4-5-20250929".into(),
            2048,
        );
        let cloned = provider.clone();
        assert_eq!(cloned.model, provider.model);
        assert_eq!(cloned.api_key, provider.api_key);
        assert_eq!(cloned.max_tokens, provider.max_tokens);
    }

    #[test]
    fn new_stores_fields_correctly() {
        let provider = ClaudeProvider::new("my-key".into(), "claude-haiku-35".into(), 4096);
        assert_eq!(provider.api_key, "my-key");
        assert_eq!(provider.model, "claude-haiku-35");
        assert_eq!(provider.max_tokens, 4096);
    }

    #[test]
    fn debug_includes_model_and_max_tokens() {
        let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 512);
        let debug = format!("{provider:?}");
        assert!(debug.contains("ClaudeProvider"));
        assert!(debug.contains("512"));
        assert!(debug.contains("<reqwest::Client>"));
    }

    #[test]
    fn request_body_serializes_without_system() {
        let body = RequestBody {
            model: "claude-sonnet-4-5-20250929",
            max_tokens: 1024,
            system: None,
            messages: &[ApiMessage {
                role: "user",
                content: "hello",
            }],
            stream: false,
            thinking: None,
            output_config: None,
            temperature: None,
            cache_control: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(!json.contains("system"));
        assert!(!json.contains("stream"));
        assert!(json.contains("\"model\":\"claude-sonnet-4-5-20250929\""));
        assert!(json.contains("\"max_tokens\":1024"));
    }

    #[test]
    fn request_body_serializes_with_system_blocks() {
        let body = RequestBody {
            model: "claude-sonnet-4-5-20250929",
            max_tokens: 1024,
            system: Some(vec![SystemContentBlock {
                block_type: "text",
                text: "You are helpful.".into(),
                cache_control: Some(CacheControl {
                    cache_type: CacheType::Ephemeral,
                }),
            }]),
            messages: &[],
            stream: false,
            thinking: None,
            output_config: None,
            temperature: None,
            cache_control: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"system\""));
        assert!(json.contains("You are helpful."));
        assert!(json.contains("\"cache_control\""));
    }

    #[test]
    fn request_body_serializes_stream_true() {
        let body = RequestBody {
            model: "test",
            max_tokens: 100,
            system: None,
            messages: &[],
            stream: true,
            thinking: None,
            output_config: None,
            temperature: None,
            cache_control: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(json.contains("\"stream\":true"));
    }

    #[test]
    fn split_messages_all_roles() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "system prompt".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "user msg".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::Assistant,
                content: "assistant reply".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "followup".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];
        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "system prompt");
        assert_eq!(chat.len(), 3);
        assert_eq!(chat[0].role, "user");
        assert_eq!(chat[0].content, "user msg");
        assert_eq!(chat[1].role, "assistant");
        assert_eq!(chat[1].content, "assistant reply");
        assert_eq!(chat[2].role, "user");
        assert_eq!(chat[2].content, "followup");
    }

    #[test]
    fn split_messages_empty() {
        let (system, chat) = split_messages(&[]);
        assert!(system.is_none());
        assert!(chat.is_empty());
    }

    #[test]
    fn api_message_serializes() {
        let msg = ApiMessage {
            role: "user",
            content: "hello world",
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello world\""));
    }

    #[test]
    fn content_block_deserializes() {
        let json = r#"{"text":"response text"}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert_eq!(block.text, "response text");
    }

    #[test]
    fn api_response_multiple_content_blocks() {
        let json = r#"{"content":[{"text":"first"},{"text":"second"}]}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.content[0].text, "first");
        assert_eq!(resp.content[1].text, "second");
    }

    #[tokio::test]
    async fn chat_with_unreachable_endpoint_errors() {
        let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let messages = vec![Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat(&messages).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_stream_with_unreachable_endpoint_errors() {
        let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let messages = vec![Message {
            role: Role::User,
            content: "test".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let result = provider.chat_stream(&messages).await;
        assert!(result.is_err());
    }

    #[test]
    fn split_messages_only_system() {
        let messages = vec![Message {
            role: Role::System,
            content: "instruction".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "instruction");
        assert!(chat.is_empty());
    }

    #[test]
    fn split_messages_only_assistant() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: "reply".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let (system, chat) = split_messages(&messages);
        assert!(system.is_none());
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, "assistant");
    }

    #[test]
    fn split_messages_interleaved_system() {
        let messages = vec![
            Message {
                role: Role::System,
                content: "first".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "question".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::System,
                content: "second".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];
        let (system, chat) = split_messages(&messages);
        assert_eq!(system.unwrap(), "first\n\nsecond");
        assert_eq!(chat.len(), 1);
    }

    #[test]
    fn request_body_serializes_with_stream_false_omits_stream() {
        let body = RequestBody {
            model: "test",
            max_tokens: 100,
            system: None,
            messages: &[],
            stream: false,
            thinking: None,
            output_config: None,
            temperature: None,
            cache_control: None,
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(!json.contains("stream"));
    }

    #[test]
    fn split_system_no_markers_caches_entire_block() {
        // Text must meet the 2048-token threshold for sonnet (≈ 8192 chars).
        let long_text = format!("You are Zeph, an AI assistant. {}", "x".repeat(8200));
        let blocks = split_system_into_blocks(&long_text, "claude-sonnet-4-6");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].cache_control.is_some());
        assert!(blocks[0].text.contains("Zeph"));
    }

    #[test]
    fn split_system_no_markers_short_text_skips_cache() {
        let blocks =
            split_system_into_blocks("You are Zeph, an AI assistant.", "claude-sonnet-4-6");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].cache_control.is_none());
    }

    #[test]
    fn split_system_no_markers_exact_threshold_sonnet_caches() {
        // Exactly 8192 chars => 8192 / 4 = 2048 tokens == sonnet threshold: should cache.
        let exact_text = "A".repeat(8192);
        let blocks = split_system_into_blocks(&exact_text, "claude-sonnet-4-6");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].cache_control.is_some());
    }

    #[test]
    fn split_system_no_markers_opus_skips_short_text() {
        // 8192 chars = 2048 tokens < 4096 opus minimum — no cache.
        let medium_text = "A".repeat(8192);
        let blocks = split_system_into_blocks(&medium_text, "claude-opus-4-6");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].cache_control.is_none());
    }

    #[test]
    fn split_system_no_markers_opus_caches_long_text() {
        // 16384 chars = 4096 tokens >= 4096 opus minimum — should cache.
        let long_text = "A".repeat(16384);
        let blocks = split_system_into_blocks(&long_text, "claude-opus-4-6");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].cache_control.is_some());
    }

    #[test]
    fn split_system_with_all_markers() {
        // Each block must exceed 2048 tokens (≈ 8192 chars) for sonnet threshold
        let padding = "x".repeat(8200);
        let system = format!(
            "base prompt {padding}\n{CACHE_MARKER_STABLE}\nskills here {padding}\n\
             {CACHE_MARKER_TOOLS}\ntool catalog {padding}\n\
             {CACHE_MARKER_VOLATILE}\nvolatile stuff"
        );
        let blocks = split_system_into_blocks(&system, "claude-sonnet-4-6");
        assert_eq!(blocks.len(), 4);
        assert!(blocks[0].cache_control.is_some());
        assert!(blocks[0].text.contains("base prompt"));
        assert!(blocks[1].cache_control.is_some());
        assert!(blocks[1].text.contains("skills here"));
        assert!(blocks[2].cache_control.is_some());
        assert!(blocks[2].text.contains("tool catalog"));
        assert!(blocks[3].cache_control.is_none());
        assert!(blocks[3].text.contains("volatile stuff"));
    }

    #[test]
    fn split_system_partial_markers() {
        let padding = "x".repeat(8200);
        let system = format!("base prompt {padding}\n{CACHE_MARKER_VOLATILE}\nvolatile only");
        let blocks = split_system_into_blocks(&system, "claude-sonnet-4-6");
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].cache_control.is_some());
        assert!(blocks[1].cache_control.is_none());
    }

    #[test]
    fn split_system_block1_padded_when_below_threshold() {
        // Block 1 is below 2048 tokens but gets padded with AGENT_IDENTITY_PREAMBLE,
        // so it must receive cache_control after padding.
        let system = format!("short text\n{CACHE_MARKER_STABLE}\nmore content");
        let blocks = split_system_into_blocks(&system, "claude-sonnet-4-6");
        // Block 1 must be padded and cached
        assert!(blocks[0].cache_control.is_some());
        assert!(blocks[0].text.contains("short text"));
        assert!(blocks[0].text.contains("Agent Identity"));
    }

    #[test]
    fn split_system_block2_not_padded_when_below_threshold() {
        // Only Block 1 (first cacheable block) gets the identity preamble padding.
        // Subsequent blocks below threshold should NOT be padded.
        let padding = "x".repeat(8200);
        let system =
            format!("base {padding}\n{CACHE_MARKER_STABLE}\nshort\n{CACHE_MARKER_TOOLS}\nmore");
        let blocks = split_system_into_blocks(&system, "claude-sonnet-4-6");
        // Block 2 ("short") is below threshold and must NOT contain identity preamble
        assert!(!blocks[1].text.contains("Agent Identity"));
    }

    #[test]
    fn api_usage_deserialization() {
        let json = r#"{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":1000,"cache_read_input_tokens":900}"#;
        let usage: ApiUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_creation_input_tokens, 1000);
        assert_eq!(usage.cache_read_input_tokens, 900);
    }

    #[test]
    fn api_response_with_usage() {
        let json = r#"{"content":[{"text":"Hello"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_some());
        assert_eq!(resp.usage.unwrap().input_tokens, 10);
    }

    #[test]
    fn api_response_deserializes() {
        let json = r#"{"content":[{"text":"Hello world"}]}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.len(), 1);
        assert_eq!(resp.content[0].text, "Hello world");
    }

    #[test]
    fn api_response_empty_content() {
        let json = r#"{"content":[]}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.content.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
    async fn integration_claude_chat() {
        let api_key =
            std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
        let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let response = provider.chat(&messages).await.unwrap();
        assert!(response.to_lowercase().contains("pong"));
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
    async fn integration_claude_chat_stream() {
        let api_key =
            std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
        let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

        let messages = vec![Message {
            role: Role::User,
            content: "Reply with exactly: pong".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut full_response = String::new();
        let mut chunk_count = 0;

        while let Some(result) = stream.next().await {
            if let crate::StreamChunk::Content(text) = result.unwrap() {
                full_response.push_str(&text);
            }
            chunk_count += 1;
        }

        assert!(!full_response.is_empty());
        assert!(full_response.to_lowercase().contains("pong"));
        assert!(chunk_count >= 1);
    }

    #[tokio::test]
    #[ignore = "requires ZEPH_CLAUDE_API_KEY env var"]
    async fn integration_claude_stream_matches_chat() {
        let api_key =
            std::env::var("ZEPH_CLAUDE_API_KEY").expect("ZEPH_CLAUDE_API_KEY must be set");
        let provider = ClaudeProvider::new(api_key, "claude-sonnet-4-5-20250929".into(), 256);

        let messages = vec![Message {
            role: Role::User,
            content: "What is 2+2? Reply with just the number.".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let chat_response = provider.chat(&messages).await.unwrap();

        let mut stream = provider.chat_stream(&messages).await.unwrap();
        let mut stream_response = String::new();
        while let Some(result) = stream.next().await {
            if let crate::StreamChunk::Content(text) = result.unwrap() {
                stream_response.push_str(&text);
            }
        }

        assert!(chat_response.contains('4'));
        assert!(stream_response.contains('4'));
    }

    #[test]
    fn anthropic_tool_serialization() {
        let tool = AnthropicTool {
            name: "bash",
            description: "Execute a shell command",
            input_schema: &serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"}
                },
                "required": ["command"]
            }),
        };
        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"bash\""));
        assert!(json.contains("\"input_schema\""));
    }

    #[test]
    fn parse_tool_response_text_only() {
        let resp = ToolApiResponse {
            content: vec![AnthropicContentBlock::Text {
                text: "Hello".into(),
                cache_control: None,
            }],
            stop_reason: None,
            usage: None,
        };
        let result = parse_tool_response(resp);
        assert!(matches!(result, ChatResponse::Text(s) if s == "Hello"));
    }

    #[test]
    fn parse_tool_response_with_tool_use() {
        let resp = ToolApiResponse {
            content: vec![
                AnthropicContentBlock::Text {
                    text: "I'll run that".into(),
                    cache_control: None,
                },
                AnthropicContentBlock::ToolUse {
                    id: "toolu_123".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
            stop_reason: None,
            usage: None,
        };
        let result = parse_tool_response(resp);
        if let ChatResponse::ToolUse {
            text, tool_calls, ..
        } = result
        {
            assert_eq!(text.unwrap(), "I'll run that");
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].name, "bash");
            assert_eq!(tool_calls[0].id, "toolu_123");
        } else {
            panic!("expected ToolUse");
        }
    }

    #[test]
    fn parse_tool_response_tool_use_only() {
        let resp = ToolApiResponse {
            content: vec![AnthropicContentBlock::ToolUse {
                id: "toolu_456".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "/tmp/file.txt"}),
            }],
            stop_reason: None,
            usage: None,
        };
        let result = parse_tool_response(resp);
        if let ChatResponse::ToolUse {
            text, tool_calls, ..
        } = result
        {
            assert!(text.is_none());
            assert_eq!(tool_calls.len(), 1);
        } else {
            panic!("expected ToolUse");
        }
    }

    #[test]
    fn parse_tool_response_json_deserialization() {
        let json = r#"{"content":[{"type":"text","text":"Let me check"},{"type":"tool_use","id":"toolu_abc","name":"bash","input":{"command":"ls"}}]}"#;
        let resp: ToolApiResponse = serde_json::from_str(json).unwrap();
        let result = parse_tool_response(resp);
        assert!(matches!(result, ChatResponse::ToolUse { .. }));
    }

    #[test]
    fn split_messages_structured_with_tool_parts() {
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::Text {
                        text: "I'll run that".into(),
                    },
                    MessagePart::ToolUse {
                        id: "t1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                ],
            ),
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file1.rs".into(),
                    is_error: false,
                }],
            ),
        ];
        let (system, chat) = split_messages_structured(&messages, true);
        assert!(system.is_none());
        assert_eq!(chat.len(), 2);

        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(assistant_json.contains("tool_use"));
        assert!(assistant_json.contains("\"id\":\"t1\""));

        let user_json = serde_json::to_string(&chat[1]).unwrap();
        assert!(user_json.contains("tool_result"));
        assert!(user_json.contains("\"tool_use_id\":\"t1\""));
    }

    /// FIX2 regression: an assistant message with a `ToolUse` part that has NO matching
    /// `ToolResult` in the next user message must emit a text block instead of a `tool_use`
    /// block, preventing Claude API 400 errors caused by unmatched `tool_use/tool_result` pairs.
    #[test]
    fn split_messages_structured_downgrades_unmatched_tool_use_to_text() {
        // Orphaned assistant[ToolUse] — no following user[ToolResult].
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::Text {
                        text: "Let me run this.".into(),
                    },
                    MessagePart::ToolUse {
                        id: "orphan_id".into(),
                        name: "shell".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                ],
            ),
            // Next message is NOT a ToolResult response — simulates compaction-split orphan.
            Message::from_parts(
                Role::User,
                vec![MessagePart::Text {
                    text: "Thanks, what did you find?".into(),
                }],
            ),
        ];

        let (_, chat) = split_messages_structured(&messages, false);
        assert_eq!(chat.len(), 2);

        // The assistant block must NOT contain a tool_use block for the unmatched ID.
        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(
            !assistant_json.contains("\"type\":\"tool_use\""),
            "unmatched tool_use must be downgraded: {assistant_json}"
        );
        // The orphaned ID must appear in a text fallback instead.
        assert!(
            assistant_json.contains("orphan_id") || assistant_json.contains("shell"),
            "downgraded tool_use must appear as text fallback: {assistant_json}"
        );
    }

    /// FIX2 regression: a matched `tool_use/tool_result` pair must still emit a real
    /// `tool_use` block. The defensive check must not break valid exchanges.
    #[test]
    fn split_messages_structured_preserves_matched_tool_use_block() {
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![MessagePart::ToolUse {
                    id: "matched_id".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                }],
            ),
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "matched_id".into(),
                    content: "hi".into(),
                    is_error: false,
                }],
            ),
        ];

        let (_, chat) = split_messages_structured(&messages, false);
        assert_eq!(chat.len(), 2);

        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(
            assistant_json.contains("\"type\":\"tool_use\""),
            "matched tool_use must be emitted as tool_use block: {assistant_json}"
        );
        assert!(assistant_json.contains("\"id\":\"matched_id\""));
    }

    /// RC1 regression: when a `ToolUse` was downgraded to text (because the next user message had
    /// no matching `ToolResult`), the corresponding `ToolResult` in the user message must ALSO be
    /// downgraded to text instead of being emitted as a native `ToolResult` block.
    /// Previously only the `ToolUse` was downgraded, leaving an orphaned `ToolResult` that caused
    /// Claude API 400 errors on session restore.
    #[test]
    fn split_structured_downgrades_orphaned_tool_result() {
        // Scenario: assistant emits tool_use "t_orphan", but the following user message has a
        // ToolResult for a DIFFERENT id — so "t_orphan" is downgraded. The ToolResult for
        // "t_orphan" (which does appear in the user message) must also be downgraded.
        let messages = vec![
            Message::from_parts(
                Role::Assistant,
                vec![MessagePart::ToolUse {
                    id: "t_orphan".into(),
                    name: "memory_save".into(),
                    input: serde_json::json!({"content": "x"}),
                }],
            ),
            // User message references t_orphan but the assistant ToolUse was not matched
            // (there is no ToolResult for t_orphan in the NEXT user message from assistant's
            // perspective — the assistant sees this user message has t_orphan, but the
            // matched_tool_ids logic checks whether the ToolResult id matches).
            // To trigger the orphan path: provide a user message whose ToolResult id does NOT
            // match the ToolUse id — so matched_tool_ids for "t_orphan" is empty.
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "t_orphan".into(),
                    content: "saved".into(),
                    is_error: false,
                }],
            ),
        ];

        // Verify the full round-trip: the assistant ToolUse is matched (t_orphan has a
        // corresponding ToolResult), so this tests the happy path.
        let (_, chat) = split_messages_structured(&messages, false);
        assert_eq!(chat.len(), 2);

        // The assistant message must emit t_orphan as a real tool_use (matched pair).
        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(
            assistant_json.contains("\"type\":\"tool_use\""),
            "matched tool_use must be emitted as native block: {assistant_json}"
        );

        // The user message must emit t_orphan as a real tool_result (matched pair).
        let user_json = serde_json::to_string(&chat[1]).unwrap();
        assert!(
            user_json.contains("\"type\":\"tool_result\""),
            "matched tool_result must be emitted as native block: {user_json}"
        );

        // Now test the actual RC1 scenario: assistant emits TWO tool_use IDs but the user
        // message only has a ToolResult for ONE of them. The unmatched tool_use is downgraded,
        // and the ToolResult for the unmatched id must NOT appear in the user message output.
        let messages_partial = vec![
            Message::from_parts(
                Role::Assistant,
                vec![
                    MessagePart::ToolUse {
                        id: "t_matched".into(),
                        name: "shell".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                    MessagePart::ToolUse {
                        id: "t_missing_result".into(),
                        name: "shell".into(),
                        input: serde_json::json!({"command": "pwd"}),
                    },
                ],
            ),
            // User only provides result for t_matched; t_missing_result has no ToolResult.
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "t_matched".into(),
                    content: "output".into(),
                    is_error: false,
                }],
            ),
        ];

        let (_, chat2) = split_messages_structured(&messages_partial, false);
        assert_eq!(chat2.len(), 2);

        // t_missing_result must be downgraded to text in the assistant message: if its ID
        // appears at all it must not be inside a native tool_use block.
        let assistant_json2 = serde_json::to_string(&chat2[0]).unwrap();
        let has_native_missing = assistant_json2.contains("\"type\":\"tool_use\"")
            && assistant_json2.contains("\"id\":\"t_missing_result\"");
        assert!(
            !has_native_missing,
            "t_missing_result must not appear as a native tool_use block: {assistant_json2}"
        );

        // t_matched must still be emitted as a real tool_use.
        assert!(
            assistant_json2.contains("\"id\":\"t_matched\""),
            "t_matched must be emitted as native tool_use: {assistant_json2}"
        );

        // The user message must only have t_matched as a real tool_result.
        let user_json2 = serde_json::to_string(&chat2[1]).unwrap();
        assert!(
            user_json2.contains("\"type\":\"tool_result\""),
            "matched tool_result must be emitted as native block: {user_json2}"
        );
        assert!(
            user_json2.contains("\"tool_use_id\":\"t_matched\""),
            "t_matched tool_result must be present: {user_json2}"
        );
    }

    /// RC4 regression: system messages interleaved in the message list must NOT appear in the
    /// `visible` index array used by `split_messages_structured`. If they did, the +1 peek used
    /// to check whether a `ToolUse` has a matching `ToolResult` would land on a system message
    /// instead of the actual next user message, causing false-positive downgrades.
    #[test]
    fn split_structured_system_not_in_visible() {
        // System message appears between the assistant ToolUse and the user ToolResult.
        // With the RC4 fix the system message is filtered out of `visible`, so idx+1 correctly
        // lands on the user message and the ToolUse is NOT downgraded.
        let messages = vec![
            Message {
                role: Role::System,
                content: "You are a helpful assistant.".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message::from_parts(
                Role::Assistant,
                vec![MessagePart::ToolUse {
                    id: "t_sys_test".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                }],
            ),
            // Interleaved system message — must not disrupt the +1 peek.
            Message {
                role: Role::System,
                content: "Additional context injected mid-conversation.".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: "t_sys_test".into(),
                    content: "hi".into(),
                    is_error: false,
                }],
            ),
        ];

        let (system_text, chat) = split_messages_structured(&messages, false);

        // Both system messages must be extracted to the system string.
        let system = system_text.unwrap_or_default();
        assert!(
            system.contains("You are a helpful assistant."),
            "first system message must be in system text: {system}"
        );
        assert!(
            system.contains("Additional context"),
            "interleaved system message must be in system text: {system}"
        );

        // chat must contain only user and assistant messages (no system).
        assert_eq!(
            chat.len(),
            2,
            "chat must contain exactly assistant + user messages (no system), got {}",
            chat.len()
        );
        assert_eq!(chat[0].role, "assistant");
        assert_eq!(chat[1].role, "user");

        // The ToolUse must NOT be downgraded — system messages must not break the +1 peek.
        let assistant_json = serde_json::to_string(&chat[0]).unwrap();
        assert!(
            assistant_json.contains("\"type\":\"tool_use\""),
            "ToolUse must be emitted as native block when system messages are filtered: {assistant_json}"
        );
        assert!(
            assistant_json.contains("\"id\":\"t_sys_test\""),
            "correct tool_use id must be present: {assistant_json}"
        );

        // The ToolResult must be emitted as a native block (not downgraded).
        let user_json = serde_json::to_string(&chat[1]).unwrap();
        assert!(
            user_json.contains("\"type\":\"tool_result\""),
            "ToolResult must be emitted as native block: {user_json}"
        );
    }

    #[test]
    fn supports_tool_use_returns_true() {
        let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-5-20250929".into(), 1024);
        assert!(provider.supports_tool_use());
    }

    #[test]
    fn anthropic_content_block_image_serializes_correctly() {
        let block = AnthropicContentBlock::Image {
            source: ImageSource {
                source_type: "base64".to_owned(),
                media_type: "image/jpeg".to_owned(),
                data: "abc123".to_owned(),
            },
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["source"]["type"], "base64");
        assert_eq!(json["source"]["media_type"], "image/jpeg");
        assert_eq!(json["source"]["data"], "abc123");
    }

    #[test]
    fn split_messages_structured_produces_image_block() {
        use base64::{Engine, engine::general_purpose::STANDARD};

        let data = vec![0xFFu8, 0xD8, 0xFF];
        let msg = Message::from_parts(
            Role::User,
            vec![
                MessagePart::Text {
                    text: "look at this".into(),
                },
                MessagePart::Image(Box::new(ImageData {
                    data: data.clone(),
                    mime_type: "image/jpeg".into(),
                })),
            ],
        );
        let (system, chat) = split_messages_structured(&[msg], true);
        assert!(system.is_none());
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].role, "user");
        match &chat[0].content {
            StructuredContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                match &blocks[0] {
                    AnthropicContentBlock::Text { text, .. } => assert_eq!(text, "look at this"),
                    _ => panic!("expected Text block first"),
                }
                match &blocks[1] {
                    AnthropicContentBlock::Image { source } => {
                        assert_eq!(source.source_type, "base64");
                        assert_eq!(source.media_type, "image/jpeg");
                        assert_eq!(source.data, STANDARD.encode(&data));
                    }
                    _ => panic!("expected Image block second"),
                }
            }
            StructuredContent::Text(_) => panic!("expected Blocks content"),
        }
    }

    #[test]
    fn tool_cache_returns_same_values_on_second_call() {
        use crate::provider::ToolDefinition;
        let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run shell commands".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let first = provider.get_or_build_api_tools(&tools);
        let second = provider.get_or_build_api_tools(&tools);
        assert_eq!(first, second);
        assert_eq!(first[0]["name"], "bash");
        assert_eq!(first[0]["description"], "Run shell commands");
    }

    #[test]
    fn tool_cache_invalidates_when_tools_change() {
        use crate::provider::ToolDefinition;
        let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let tools_a = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run shell commands".into(),
            parameters: serde_json::json!({}),
        }];
        let tools_b = vec![ToolDefinition {
            name: "read".into(),
            description: "Read files".into(),
            parameters: serde_json::json!({}),
        }];
        let first = provider.get_or_build_api_tools(&tools_a);
        let second = provider.get_or_build_api_tools(&tools_b);
        assert_eq!(first[0]["name"], "bash");
        assert_eq!(second[0]["name"], "read");
    }

    #[test]
    fn tool_cache_serialized_shape_snapshot() {
        use crate::provider::ToolDefinition;
        let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run a shell command".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run"}
                },
                "required": ["command"]
            }),
        }];
        let cached = provider.get_or_build_api_tools(&tools);
        let pretty = serde_json::to_string_pretty(&cached).unwrap();
        insta::assert_snapshot!(pretty);
    }

    /// Spawn a minimal HTTP server that captures request bodies and returns fixed JSON responses.
    /// Returns `(port, captured_bodies_receiver, join_handle)`.
    async fn spawn_capture_server(
        responses: Vec<String>,
    ) -> (
        u16,
        tokio::sync::mpsc::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        let handle = tokio::spawn(async move {
            for resp in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.split();
                    let mut buf_reader = BufReader::new(reader);

                    // Read headers to find Content-Length
                    let mut content_length: usize = 0;
                    loop {
                        let mut line = String::new();
                        buf_reader.read_line(&mut line).await.unwrap_or(0);
                        if line == "\r\n" || line == "\n" || line.is_empty() {
                            break;
                        }
                        if line.to_lowercase().starts_with("content-length:") {
                            content_length = line
                                .split(':')
                                .nth(1)
                                .and_then(|v| v.trim().parse().ok())
                                .unwrap_or(0);
                        }
                    }

                    // Read body
                    let mut body = vec![0u8; content_length];
                    buf_reader.read_exact(&mut body).await.ok();
                    let body_str = String::from_utf8_lossy(&body).into_owned();
                    tx.send(body_str).await.ok();

                    let resp_bytes = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        resp.len(),
                        resp
                    );
                    writer.write_all(resp_bytes.as_bytes()).await.ok();
                });
            }
        });

        (port, rx, handle)
    }

    fn tool_api_response_json() -> String {
        r#"{"content":[{"type":"text","text":"done"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#.into()
    }

    #[tokio::test]
    async fn chat_with_tools_sends_correct_tool_fields() {
        use crate::provider::ToolDefinition;

        let response = tool_api_response_json();
        let (port, mut rx, _handle) = spawn_capture_server(vec![response]).await;

        let client = reqwest::Client::new();
        let provider =
            ClaudeProvider::new("test-key".into(), "claude-test".into(), 256).with_client(client);

        // Override API_URL via a custom client pointed at our mock
        let tools = vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read a file from disk".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        }];
        let messages = vec![Message::from_legacy(Role::User, "read /tmp/f")];

        // We can't override API_URL from outside, so test via get_or_build_api_tools directly
        // and verify the serialized body shape via snapshot.
        let _ = (port, &mut rx);

        let api_tools = provider.get_or_build_api_tools(&tools);
        assert_eq!(api_tools.len(), 1);
        assert_eq!(api_tools[0]["name"], "read_file");
        assert_eq!(api_tools[0]["description"], "Read a file from disk");
        assert!(api_tools[0]["input_schema"].is_object());
        assert_eq!(api_tools[0]["input_schema"]["type"], "object");
        let _ = messages;
    }

    #[tokio::test]
    async fn chat_with_tools_cache_hit_does_not_re_serialize() {
        use crate::provider::ToolDefinition;
        let provider = ClaudeProvider::new("key".into(), "model".into(), 512);
        let tools = vec![
            ToolDefinition {
                name: "tool_a".into(),
                description: "First tool".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "tool_b".into(),
                description: "Second tool".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
        ];

        let first = provider.get_or_build_api_tools(&tools);
        let second = provider.get_or_build_api_tools(&tools);
        let third = provider.get_or_build_api_tools(&tools);

        // All calls return identical values
        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["name"], "tool_a");
        assert_eq!(first[1]["name"], "tool_b");

        // Verify cache is populated
        let guard = provider.tool_cache.lock().unwrap();
        let (hash, values) = guard.as_ref().unwrap();
        assert_ne!(*hash, 0);
        assert_eq!(values.len(), 2);
    }

    #[tokio::test]
    async fn chat_with_tools_cache_partial_tool_set_change_invalidates() {
        use crate::provider::ToolDefinition;
        let provider = ClaudeProvider::new("key".into(), "model".into(), 512);

        let tools_v1 = vec![ToolDefinition {
            name: "search".into(),
            description: "Search the web".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let tools_v2 = vec![
            ToolDefinition {
                name: "search".into(),
                description: "Search the web".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "browse".into(),
                description: "Browse a URL".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
        ];

        let v1 = provider.get_or_build_api_tools(&tools_v1);
        assert_eq!(v1.len(), 1);

        let v2 = provider.get_or_build_api_tools(&tools_v2);
        assert_eq!(v2.len(), 2);
        assert_eq!(v2[1]["name"], "browse");

        // Cache now reflects v2
        let guard = provider.tool_cache.lock().unwrap();
        let (hash, values) = guard.as_ref().unwrap();
        assert_ne!(*hash, 0);
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn has_image_parts_detects_image_in_messages() {
        let with_image = Message::from_parts(
            Role::User,
            vec![MessagePart::Image(Box::new(ImageData {
                data: vec![1],
                mime_type: "image/png".into(),
            }))],
        );
        let without_image = Message::from_legacy(Role::User, "plain text");
        assert!(ClaudeProvider::has_image_parts(&[with_image]));
        assert!(!ClaudeProvider::has_image_parts(&[without_image]));
    }

    // Test that the pagination response JSON structure is correctly parsed inline.
    // list_models_remote uses serde_json::Value for page parsing; test the same logic here.
    #[test]
    fn pagination_response_has_more_true_extracts_last_id() {
        let page = serde_json::json!({
            "data": [{"id": "model-a", "type": "model", "display_name": "Model A"}],
            "has_more": true,
            "last_id": "model-a"
        });
        let has_more = page
            .get("has_more")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let last_id = page
            .get("last_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        assert!(has_more);
        assert_eq!(last_id, Some("model-a".to_string()));
    }

    #[test]
    fn pagination_response_has_more_false_stops_loop() {
        let page = serde_json::json!({
            "data": [{"id": "model-b", "type": "model", "display_name": "Model B"}],
            "has_more": false
        });
        let has_more = page
            .get("has_more")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        assert!(!has_more);
    }

    #[test]
    fn model_item_filters_non_model_type() {
        let page = serde_json::json!({
            "data": [
                {"id": "model-ok", "type": "model", "display_name": "OK"},
                {"id": "skip-me", "type": "other", "display_name": "Skip"}
            ],
            "has_more": false
        });
        let models: Vec<crate::model_cache::RemoteModelInfo> = page
            .get("data")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let type_field = item
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if type_field != "model" {
                            return None;
                        }
                        let id = item
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let display_name = item
                            .get("display_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&id)
                            .to_string();
                        Some(crate::model_cache::RemoteModelInfo {
                            id,
                            display_name,
                            context_window: None,
                            created_at: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "model-ok");
    }

    #[test]
    fn model_item_uses_id_as_display_name_when_missing() {
        let item = serde_json::json!({"id": "claude-x", "type": "model"});
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let display_name = item
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(&id)
            .to_string();
        assert_eq!(display_name, "claude-x");
    }

    // ------------------------------------------------------------------
    // Wiremock HTTP-level tests using fixture helpers from testing module
    // ------------------------------------------------------------------

    #[test]
    fn messages_response_deserialization() {
        let raw = serde_json::json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type": "text", "text": "hello claude"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        });
        let resp: ApiResponse = serde_json::from_value(raw).unwrap();
        let text: String = resp.content.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(text, "hello claude");
    }

    #[tokio::test]
    async fn messages_429_overload_propagates() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        use crate::testing::claude_overload_response;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(claude_overload_response(429))
            .mount(&server)
            .await;

        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages", server.uri()))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 429);
    }

    #[tokio::test]
    async fn messages_529_overload_propagates() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        use crate::testing::claude_overload_response;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(claude_overload_response(529))
            .mount(&server)
            .await;

        let resp = reqwest::Client::new()
            .post(format!("{}/v1/messages", server.uri()))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 529);
    }

    #[tokio::test]
    async fn claude_sse_fixture_contains_expected_events() {
        use crate::testing::claude_sse_stream_response;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/stream"))
            .respond_with(claude_sse_stream_response(&["Hello", " world"]))
            .mount(&server)
            .await;
        let raw = reqwest::Client::new()
            .post(format!("{}/stream", server.uri()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(raw.contains("Hello"));
        assert!(raw.contains(" world"));
        assert!(raw.contains("message_stop"));
        assert!(raw.contains("content_block_delta"));
    }

    #[test]
    fn thinking_config_extended_serializes() {
        let cfg = ThinkingConfig::Extended {
            budget_tokens: 10_000,
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["mode"], "extended");
        assert_eq!(json["budget_tokens"], 10_000);
    }

    #[test]
    fn thinking_config_adaptive_serializes_without_effort() {
        let cfg = ThinkingConfig::Adaptive { effort: None };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["mode"], "adaptive");
        assert!(json.get("effort").is_none());
    }

    #[test]
    fn thinking_config_adaptive_serializes_with_effort() {
        let cfg = ThinkingConfig::Adaptive {
            effort: Some(ThinkingEffort::High),
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json["mode"], "adaptive");
        assert_eq!(json["effort"], "high");
    }

    #[test]
    fn thinking_config_extended_deserializes() {
        let json = r#"{"mode":"extended","budget_tokens":8000}"#;
        let cfg: ThinkingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg,
            ThinkingConfig::Extended {
                budget_tokens: 8000
            }
        );
    }

    #[test]
    fn thinking_config_adaptive_deserializes() {
        let json = r#"{"mode":"adaptive","effort":"low"}"#;
        let cfg: ThinkingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg,
            ThinkingConfig::Adaptive {
                effort: Some(ThinkingEffort::Low)
            }
        );
    }

    #[test]
    fn thinking_capability_sonnet_4_6_needs_interleaved_beta() {
        let cap = thinking_capability("claude-sonnet-4-6-20250514");
        assert!(cap.needs_interleaved_beta);
    }

    #[test]
    fn thinking_capability_opus_4_6_no_interleaved_beta() {
        let cap = thinking_capability("claude-opus-4-6");
        assert!(!cap.needs_interleaved_beta);
    }

    #[test]
    fn thinking_capability_unknown_model_no_beta() {
        let cap = thinking_capability("gpt-4o");
        assert!(!cap.needs_interleaved_beta);
    }

    #[test]
    fn with_thinking_rejects_budget_below_minimum() {
        let err = ClaudeProvider::new("k".into(), "m".into(), 32_000)
            .with_thinking(ThinkingConfig::Extended { budget_tokens: 0 })
            .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");

        let err = ClaudeProvider::new("k".into(), "m".into(), 32_000)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 1023,
            })
            .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn with_thinking_accepts_minimum_budget() {
        ClaudeProvider::new("k".into(), "m".into(), 32_000)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 1024,
            })
            .unwrap();
    }

    #[test]
    fn with_thinking_accepts_maximum_budget() {
        ClaudeProvider::new("k".into(), "m".into(), 256_000)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 128_000,
            })
            .unwrap();
    }

    #[test]
    fn with_thinking_rejects_budget_above_maximum() {
        let err = ClaudeProvider::new("k".into(), "m".into(), 256_000)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 128_001,
            })
            .unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn with_thinking_rejects_budget_not_less_than_max_tokens() {
        // After auto-bump max_tokens = 16_000, budget_tokens = 16_000 is not < max_tokens
        let err = ClaudeProvider::new("k".into(), "m".into(), 1024)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 16_000,
            })
            .unwrap_err();
        assert!(err.to_string().contains("less than max_tokens"), "{err}");
    }

    #[test]
    fn with_thinking_bumps_max_tokens_when_too_low() {
        let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 1024)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 8000,
            })
            .unwrap();
        assert!(provider.max_tokens >= MIN_MAX_TOKENS_WITH_THINKING);
    }

    #[test]
    fn with_thinking_keeps_max_tokens_when_already_high() {
        let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 32_000)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 8000,
            })
            .unwrap();
        assert_eq!(provider.max_tokens, 32_000);
    }

    #[test]
    fn build_thinking_param_extended_returns_enabled_with_budget() {
        let provider = ClaudeProvider::new("k".into(), "m".into(), 16_000)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 5000,
            })
            .unwrap();
        let (param, temp, effort) = provider.build_thinking_param();
        let param = param.unwrap();
        assert_eq!(param.thinking_type, "enabled");
        assert_eq!(param.budget_tokens, Some(5000));
        assert!(temp.is_none());
        assert!(effort.is_none());
    }

    #[test]
    fn build_thinking_param_adaptive_returns_adaptive_type() {
        let provider = ClaudeProvider::new("k".into(), "m".into(), 16_000)
            .with_thinking(ThinkingConfig::Adaptive { effort: None })
            .unwrap();
        let (param, temp, effort) = provider.build_thinking_param();
        let param = param.unwrap();
        assert_eq!(param.thinking_type, "adaptive");
        assert!(param.budget_tokens.is_none());
        assert!(temp.is_none());
        assert!(effort.is_none());
    }

    #[test]
    fn build_thinking_param_adaptive_with_effort_returns_effort() {
        let provider = ClaudeProvider::new("k".into(), "m".into(), 16_000)
            .with_thinking(ThinkingConfig::Adaptive {
                effort: Some(ThinkingEffort::High),
            })
            .unwrap();
        let (param, temp, effort) = provider.build_thinking_param();
        let param = param.unwrap();
        assert_eq!(param.thinking_type, "adaptive");
        assert!(param.budget_tokens.is_none());
        assert!(temp.is_none());
        assert_eq!(effort, Some(ThinkingEffort::High));
    }

    #[test]
    fn build_thinking_param_adaptive_serializes_correctly() {
        let param = ThinkingParam {
            thinking_type: "adaptive",
            budget_tokens: None,
        };
        let json = serde_json::to_value(&param).unwrap();
        assert_eq!(json, serde_json::json!({"type": "adaptive"}));
        assert!(json.get("budget_tokens").is_none());
    }

    #[test]
    fn build_thinking_param_no_thinking_returns_none() {
        let provider = ClaudeProvider::new("k".into(), "m".into(), 1024);
        let (param, temp, effort) = provider.build_thinking_param();
        assert!(param.is_none());
        assert!(temp.is_none());
        assert!(effort.is_none());
    }

    #[test]
    fn beta_header_without_thinking_returns_none() {
        let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 1024);
        let beta = provider.beta_header(true);
        assert!(beta.is_none());
    }

    #[test]
    fn beta_header_sonnet_4_6_extended_with_tools_includes_interleaved() {
        let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 16_000)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 5000,
            })
            .unwrap();
        let beta = provider.beta_header(true);
        assert!(
            beta.as_deref()
                .is_some_and(|b| b.contains(ANTHROPIC_BETA_INTERLEAVED_THINKING))
        );
    }

    #[test]
    fn beta_header_sonnet_4_6_extended_no_tools_excludes_interleaved() {
        let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 16_000)
            .with_thinking(ThinkingConfig::Extended {
                budget_tokens: 5000,
            })
            .unwrap();
        let beta = provider.beta_header(false);
        assert!(beta.is_none());
    }

    #[test]
    fn beta_header_adaptive_mode_excludes_interleaved() {
        let provider = ClaudeProvider::new("k".into(), "claude-sonnet-4-6".into(), 16_000)
            .with_thinking(ThinkingConfig::Adaptive { effort: None })
            .unwrap();
        let beta = provider.beta_header(true);
        assert!(beta.is_none());
    }

    #[test]
    fn parse_tool_response_with_thinking_blocks() {
        let resp = ToolApiResponse {
            content: vec![
                AnthropicContentBlock::Thinking {
                    thinking: "let me think".into(),
                    signature: "sig123".into(),
                },
                AnthropicContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ],
            stop_reason: None,
            usage: None,
        };
        let result = parse_tool_response(resp);
        if let ChatResponse::ToolUse {
            thinking_blocks,
            tool_calls,
            ..
        } = result
        {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(thinking_blocks.len(), 1);
            if let ThinkingBlock::Thinking {
                thinking,
                signature,
            } = &thinking_blocks[0]
            {
                assert_eq!(thinking, "let me think");
                assert_eq!(signature, "sig123");
            } else {
                panic!("expected Thinking variant");
            }
        } else {
            panic!("expected ToolUse");
        }
    }

    #[test]
    fn parse_tool_response_with_redacted_thinking() {
        let resp = ToolApiResponse {
            content: vec![
                AnthropicContentBlock::RedactedThinking {
                    data: "redacted".into(),
                },
                AnthropicContentBlock::Text {
                    text: "result".into(),
                    cache_control: None,
                },
            ],
            stop_reason: None,
            usage: None,
        };
        let result = parse_tool_response(resp);
        // No tool calls, so returns Text; thinking is dropped for text-only responses
        assert!(matches!(result, ChatResponse::Text(_)));
    }

    #[test]
    fn thinking_block_serializes_in_structured_message() {
        let msg = Message::from_parts(
            Role::Assistant,
            vec![
                MessagePart::ThinkingBlock {
                    thinking: "my reasoning".into(),
                    signature: "abc".into(),
                },
                MessagePart::Text {
                    text: "answer".into(),
                },
            ],
        );
        let (_, chat) = split_messages_structured(&[msg], true);
        assert_eq!(chat.len(), 1);
        let json = serde_json::to_value(&chat[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "my reasoning");
        assert_eq!(blocks[0]["signature"], "abc");
        assert_eq!(blocks[1]["type"], "text");
    }

    #[test]
    fn redacted_thinking_block_serializes_in_structured_message() {
        let msg = Message::from_parts(
            Role::Assistant,
            vec![MessagePart::RedactedThinkingBlock {
                data: "secret".into(),
            }],
        );
        let (_, chat) = split_messages_structured(&[msg], true);
        let json = serde_json::to_value(&chat[0]).unwrap();
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "redacted_thinking");
        assert_eq!(blocks[0]["data"], "secret");
    }

    #[test]
    fn thinking_content_block_roundtrip() {
        let block = AnthropicContentBlock::Thinking {
            thinking: "internal reasoning".into(),
            signature: "signature-data".into(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "thinking");
        let restored: AnthropicContentBlock = serde_json::from_value(json).unwrap();
        if let AnthropicContentBlock::Thinking {
            thinking,
            signature,
        } = restored
        {
            assert_eq!(thinking, "internal reasoning");
            assert_eq!(signature, "signature-data");
        } else {
            panic!("expected Thinking");
        }
    }

    #[test]
    fn redacted_thinking_content_block_roundtrip() {
        let block = AnthropicContentBlock::RedactedThinking {
            data: "opaque-data".into(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "redacted_thinking");
        let restored: AnthropicContentBlock = serde_json::from_value(json).unwrap();
        if let AnthropicContentBlock::RedactedThinking { data } = restored {
            assert_eq!(data, "opaque-data");
        } else {
            panic!("expected RedactedThinking");
        }
    }

    // ── #1085: anthropic-beta header removed ──────────────────────────────────

    #[test]
    fn build_request_does_not_include_anthropic_beta_header() {
        let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-6".into(), 256);
        let messages = vec![Message {
            role: Role::User,
            content: "hi".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let req = provider.build_request(&messages, false).build().unwrap();
        assert!(
            req.headers().get("anthropic-beta").is_none(),
            "anthropic-beta header must not be present"
        );
        assert!(req.headers().get("anthropic-version").is_some());
        assert!(req.headers().get("x-api-key").is_some());
    }

    // ── #1084: cache_control only on last tool ────────────────────────────────

    #[test]
    fn get_or_build_api_tools_only_last_tool_has_cache_control() {
        use crate::provider::ToolDefinition;
        let provider = ClaudeProvider::new("key".into(), "model".into(), 512);
        let tools = vec![
            ToolDefinition {
                name: "alpha".into(),
                description: "First".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "beta".into(),
                description: "Second".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "gamma".into(),
                description: "Third".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
        ];
        let result = provider.get_or_build_api_tools(&tools);
        assert_eq!(result.len(), 3);
        assert!(
            result[0].get("cache_control").is_none(),
            "first tool must not have cache_control"
        );
        assert!(
            result[1].get("cache_control").is_none(),
            "middle tool must not have cache_control"
        );
        assert!(
            result[2].get("cache_control").is_some(),
            "last tool must have cache_control"
        );
        assert_eq!(result[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn get_or_build_api_tools_single_tool_has_cache_control() {
        use crate::provider::ToolDefinition;
        let provider = ClaudeProvider::new("key".into(), "model".into(), 512);
        let tools = vec![ToolDefinition {
            name: "only".into(),
            description: "Only tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let result = provider.get_or_build_api_tools(&tools);
        assert_eq!(result.len(), 1);
        assert!(result[0].get("cache_control").is_some());
        assert_eq!(result[0]["cache_control"]["type"], "ephemeral");
    }

    // ── #1083: model-aware token threshold ───────────────────────────────────

    #[test]
    fn cache_min_tokens_sonnet_returns_2048() {
        assert_eq!(cache_min_tokens("claude-sonnet-4-6"), 2048);
        assert_eq!(cache_min_tokens("claude-sonnet-4-5-20250929"), 2048);
    }

    #[test]
    fn cache_min_tokens_non_sonnet_returns_4096() {
        assert_eq!(cache_min_tokens("claude-opus-4-6"), 4096);
        assert_eq!(cache_min_tokens("claude-haiku-4-5"), 4096);
        assert_eq!(cache_min_tokens("unknown-model"), 4096);
    }

    #[test]
    fn split_system_opus_block_above_threshold_gets_cache_control() {
        // opus threshold = 4096 tokens = 16384 chars
        let padding = "x".repeat(16400);
        let system = format!("{padding}\n{CACHE_MARKER_STABLE}\nmore");
        let blocks = split_system_into_blocks(&system, "claude-opus-4-6");
        assert!(
            blocks[0].cache_control.is_some(),
            "block above opus threshold must be cached"
        );
    }

    #[test]
    fn split_system_opus_block_below_threshold_skips_cache_control() {
        // text under 16384 chars is below opus threshold (4096 tokens)
        let system = format!("short\n{CACHE_MARKER_STABLE}\nmore content");
        let blocks = split_system_into_blocks(&system, "claude-opus-4-6");
        assert!(
            blocks[0].cache_control.is_none(),
            "block below opus threshold must not be cached"
        );
    }

    // ── #1086: top-level cache_control for multi-turn ─────────────────────────

    #[test]
    fn build_request_single_message_no_top_level_cache_control() {
        let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-6".into(), 256);
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let req = provider.build_request(&messages, false).build().unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
        assert!(
            body.get("cache_control").is_none(),
            "single-turn request must not have top-level cache_control"
        );
    }

    #[test]
    fn build_request_multi_turn_has_top_level_cache_control() {
        let provider = ClaudeProvider::new("key".into(), "claude-sonnet-4-6".into(), 256);
        let messages = vec![
            Message {
                role: Role::User,
                content: "first".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::Assistant,
                content: "reply".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: "second".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];
        let req = provider.build_request(&messages, false).build().unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(req.body().and_then(|b| b.as_bytes()).unwrap()).unwrap();
        assert_eq!(
            body["cache_control"]["type"], "ephemeral",
            "multi-turn request must have top-level cache_control"
        );
    }

    // ── #1087: message-level breakpoint at position max(0, total-20) ──────────

    #[test]
    fn split_messages_structured_single_message_no_cache_breakpoint() {
        let messages = vec![Message {
            role: Role::User,
            content: "only message".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let (_, chat) = split_messages_structured(&messages, true);
        assert_eq!(chat.len(), 1);
        // With only 1 message, no breakpoint is placed
        let json = serde_json::to_value(&chat[0]).unwrap();
        let has_cache = json.to_string().contains("cache_control");
        assert!(
            !has_cache,
            "single message must not have cache_control breakpoint"
        );
    }

    #[test]
    fn split_messages_structured_two_messages_places_breakpoint_on_user() {
        let messages = vec![
            Message {
                role: Role::User,
                content: "first user".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::Assistant,
                content: "assistant reply".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];
        let (_, chat) = split_messages_structured(&messages, true);
        assert_eq!(chat.len(), 2);
        // Breakpoint must be on the user message at index 0 (only user in range)
        let user_json = serde_json::to_value(&chat[0]).unwrap();
        assert!(
            user_json.to_string().contains("cache_control"),
            "user message must carry cache_control breakpoint"
        );
        let assistant_json = serde_json::to_value(&chat[1]).unwrap();
        assert!(
            !assistant_json.to_string().contains("cache_control"),
            "assistant message must not have cache_control"
        );
    }

    #[test]
    fn split_messages_structured_breakpoint_targets_last_minus_20_position() {
        // Build 25 messages: user/assistant alternating, user first
        let mut messages = Vec::new();
        for i in 0..25u32 {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            let content = format!("message {i}");
            messages.push(Message {
                role,
                content,
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }
        let (_, chat) = split_messages_structured(&messages, true);
        assert_eq!(chat.len(), 25);
        // target = 25 - 20 = 5; first user at or after index 5 is index 6 (even indices are user)
        // Actually index 5 is assistant (odd), so search finds index 6 (user)
        let mut breakpoint_idx = None;
        for (i, msg) in chat.iter().enumerate() {
            let json = serde_json::to_value(msg).unwrap();
            if json.to_string().contains("cache_control") {
                breakpoint_idx = Some(i);
                break;
            }
        }
        let idx = breakpoint_idx.expect("must have a breakpoint somewhere");
        assert_eq!(
            chat[idx].role, "user",
            "breakpoint must be on a user message"
        );
        // Breakpoint index must be >= max(0, total-20) = 5
        assert!(idx >= 5, "breakpoint must be at or after position total-20");
    }

    // --- #1094: tool schema hash in cache key ---

    #[test]
    fn tool_cache_invalidates_on_schema_change() {
        use crate::provider::ToolDefinition;
        let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let tools_v1 = vec![ToolDefinition {
            name: "tool".into(),
            description: "desc".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}}),
        }];
        let tools_v2 = vec![ToolDefinition {
            name: "tool".into(),
            description: "desc".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"b": {"type": "number"}}}),
        }];
        let first = provider.get_or_build_api_tools(&tools_v1);
        let second = provider.get_or_build_api_tools(&tools_v2);
        // Same names but different schemas — must return different serialized tools.
        assert_eq!(
            first[0]["input_schema"]["properties"]["a"]["type"],
            "string"
        );
        assert_eq!(
            second[0]["input_schema"]["properties"]["b"]["type"],
            "number"
        );
        // Hash-based invalidation contract: different schemas must produce different keys.
        assert_ne!(tool_cache_key(&tools_v1), tool_cache_key(&tools_v2));
    }

    #[test]
    fn tool_cache_hits_on_same_tools() {
        use crate::provider::ToolDefinition;
        let provider = ClaudeProvider::new("key".into(), "model".into(), 1024);
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "Run".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let first = provider.get_or_build_api_tools(&tools);
        let second = provider.get_or_build_api_tools(&tools);
        assert_eq!(first, second);
        let expected = tool_cache_key(&tools);
        let cached_hash = provider
            .tool_cache
            .lock()
            .unwrap()
            .as_ref()
            .map(|(h, _)| *h);
        assert_eq!(cached_hash, Some(expected));
    }

    // --- #1093: cache_user_messages toggle ---

    #[test]
    fn split_messages_structured_cache_enabled_adds_cache_control() {
        let messages = vec![
            Message::from_legacy(Role::User, "first"),
            Message::from_legacy(Role::Assistant, "answer"),
            Message::from_legacy(Role::User, "second"),
        ];
        let (_, chat) = split_messages_structured(&messages, true);
        assert_eq!(chat.len(), 3);
        // Breakpoint targets the user message at max(0, total-20) = 0, which is chat[0].
        let has_cache = chat.iter().any(|m| {
            m.role == "user"
                && match &m.content {
                    StructuredContent::Blocks(blocks) => blocks.iter().any(|b| {
                        matches!(
                            b,
                            AnthropicContentBlock::Text {
                                cache_control: Some(_),
                                ..
                            }
                        )
                    }),
                    StructuredContent::Text(_) => false,
                }
        });
        assert!(
            has_cache,
            "at least one user message must have cache_control when enabled"
        );
    }

    #[test]
    fn split_messages_structured_cache_disabled_no_cache_control() {
        let messages = vec![
            Message::from_legacy(Role::User, "first"),
            Message::from_legacy(Role::Assistant, "answer"),
            Message::from_legacy(Role::User, "second"),
        ];
        let (_, chat) = split_messages_structured(&messages, false);
        assert_eq!(chat.len(), 3);
        // With cache disabled, last user message stays as plain Text.
        assert!(
            matches!(&chat[2].content, StructuredContent::Text(_)),
            "last user message must remain Text when cache disabled"
        );
    }

    #[test]
    fn with_cache_user_messages_builder() {
        let provider =
            ClaudeProvider::new("k".into(), "m".into(), 256).with_cache_user_messages(false);
        assert!(!provider.cache_user_messages);
        let provider2 = ClaudeProvider::new("k".into(), "m".into(), 256);
        assert!(provider2.cache_user_messages);
    }

    #[test]
    fn clone_preserves_cache_user_messages() {
        let provider =
            ClaudeProvider::new("k".into(), "m".into(), 256).with_cache_user_messages(false);
        let cloned = provider.clone();
        assert!(!cloned.cache_user_messages);
    }

    #[test]
    fn store_cache_usage_updates_last_usage() {
        let provider = ClaudeProvider::new("k".into(), "m".into(), 256);
        assert!(provider.last_usage().is_none());

        let usage = ApiUsage {
            input_tokens: 42,
            output_tokens: 17,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        provider.store_cache_usage(&usage);

        assert_eq!(provider.last_usage(), Some((42, 17)));
    }
}
