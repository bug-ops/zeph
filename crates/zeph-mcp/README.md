# zeph-mcp

[![Crates.io](https://img.shields.io/crates/v/zeph-mcp)](https://crates.io/crates/zeph-mcp)
[![docs.rs](https://img.shields.io/docsrs/zeph-mcp)](https://docs.rs/zeph-mcp)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

MCP client with multi-server lifecycle and Qdrant tool registry for Zeph.

## Overview

Implements the Model Context Protocol client for Zeph, managing connections to multiple MCP servers, discovering their tools at startup, and routing tool calls through a unified executor. Built on rmcp 0.17.

## Key Modules

- **client** — low-level MCP transport and session handling; `ToolListChangedHandler` receives `tools/list_changed` notifications, applies `sanitize_tools()` (rate-limited to once per 5 s per server, capped at 100 tools), and forwards the sanitized list to `McpManager` via a refresh channel
- **manager** — `McpManager`, `McpTransport`, `ServerEntry` for multi-server lifecycle; command allowlist validation (npx, uvx, node, python3, docker, mcpls, etc.), env var blocklist (LD_PRELOAD, DYLD_*, NODE_OPTIONS, etc.), and path separator rejection; statically configured servers (from `[[mcp.servers]]`) bypass SSRF validation to allow connections to `localhost` and private IPs — dynamically added servers retain full SSRF protection
- **sanitize** — `sanitize_tools()` applied to all tool definitions at registration time and again on every `tools/list_changed` refresh; strips 17 injection-detection patterns, Unicode Cf-category characters, and caps descriptions at 1024 bytes; fields triggering a pattern are replaced with `"[sanitized]"` — tool registration is never blocked
- **executor** — `McpToolExecutor` bridging MCP tools into the `ToolExecutor` trait; propagates `caller_id` from sub-agent dispatches to the audit log
- **registry** — `McpToolRegistry` for tool lookup and optional Qdrant-backed search
- **tool** — `McpTool` wrapper with schema and metadata
- **prompt** — MCP prompt template support
- **error** — `McpError` error types with typed `McpErrorCode` for retry classification (`Transient`, `RateLimited`, `InvalidInput`, `AuthFailure`, `ServerError`, `NotFound`, `PolicyBlocked`)

## MCP Roots protocol

The MCP client implements the `roots/list` handler, exposing configured project roots to MCP servers. Roots are declared via `[mcp.roots]` in config and passed to each server connection at initialization time. Servers that support `roots/list` can use this information to scope their file system access to the declared directories.

## Semantic tool discovery

`SemanticToolIndex` indexes all registered MCP tool definitions as embedding vectors in Qdrant (or the SQLite vector backend). On each LLM turn, only the top-K most relevant tools — ranked by cosine similarity to the current query — are included in the tools array sent to the model. This keeps the tools payload small for models with narrow context windows and reduces prompt injection surface area.

```toml
[mcp.tool_discovery]
enabled      = true
top_k        = 20         # max tools sent per request (0 = all tools, disables discovery)
min_score    = 0.55       # minimum similarity threshold
collection   = "zeph_mcp_tools"
```

## outputSchema forwarding

When `mcp.forward_output_schema = true`, Zeph appends a bounded "Expected output schema" hint derived from the MCP tool's `outputSchema` to the tool description sent to the LLM. This enables more accurate tool-result parsing and typed tool chaining. Schema content is sanitized through the injection pipeline; the hint is capped at `mcp.output_schema_hint_bytes` (default: 1024 bytes). The tool cache key covers both `description` and `output_schema` to prevent stale hits on server reconnects.

```toml
[mcp]
forward_output_schema    = true
output_schema_hint_bytes = 1024
```

> [!NOTE]
> `forward_output_schema` is supported by Claude and OpenAI backends. Compatible, Gemini, and Ollama providers emit a `WARN` log when the setting is enabled, since those backends do not support structured output schemas.

**Note:**
> Tool discovery requires an embedding model. Configure `[llm.orchestrator] embedding_model` or set a dedicated `embedding_provider` for the mcp subsystem. When Qdrant is unavailable the index falls back to BM25 keyword matching.

## Per-message pruning cache

`PruningCache` tracks which tool set was sent in the previous LLM request. If the ranked tool list for the current turn is identical, the cache returns the pre-serialized JSON blob directly, skipping re-serialization and re-ranking.

Cache invalidation triggers on: new tool registered, tool removed, `tools/list_changed` notification, or config reload. No manual configuration is required; the cache is always active when `[mcp.tool_discovery] enabled = true`.

## Tool attestation

`expected_tools` in a server config entry declares the tool names that server is authorised to expose. If a tool name appears in `tools/list` that is not in `expected_tools`, it is logged as a security warning and excluded from the registry.

```toml
[[mcp.servers]]
id             = "filesystem"
command        = "npx"
args           = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
expected_tools = ["read_file", "write_file", "list_directory"]
```

**Important:**
> Leave `expected_tools` empty (or omit it) to allow all tools from a server. Setting it to an empty list `[]` blocks all tools from that server.

## Elicitation

MCP servers can request structured user input via the `elicitation/create` method. When enabled, Zeph presents a phishing-prevention header before displaying the server's form and routes the response back over a bounded channel.

| Config field | Type | Default | Description |
|---|---|---|---|
| `elicitation_enabled` | bool | `false` | Enable elicitation globally (opt-in) |
| `elicitation_timeout` | u64 (secs) | `120` | Seconds to wait for user input before timing out |
| `elicitation_queue_capacity` | usize | `16` | Bounded channel capacity for pending elicitation requests |
| `elicitation_warn_sensitive_fields` | bool | `true` | Warn when field names suggest sensitive input (password, token, key, etc.) |

A per-server `elicitation_enabled` override takes precedence over the global setting. Sandboxed servers (trust level `Sandboxed`) can never use elicitation regardless of config.

```toml
[mcp]
elicitation_enabled = true
elicitation_timeout = 120
```

## Security hardening

- **Tool collision detection** — when two servers expose tools with the same `sanitized_id`, a warning is emitted at registration time. The first-registered tool wins.
- **Tool-list snapshot locking** — set `lock_tool_list = true` on a server entry to reject any `tools/list_changed` refresh after the initial snapshot. Prevents malicious servers from injecting new tools mid-session.
- **Per-server stdio env isolation** — `env_isolation = true` (or `default_env_isolation = true` globally) strips the inherited process environment before spawning stdio MCP servers, preventing accidental secret leakage via `PATH`, `HOME`, and similar variables. Explicitly declared `env` keys are still passed through.
- **Intent-anchor nonce boundaries** — tool output from MCP servers is wrapped with per-call nonce delimiters before entering the LLM context, reducing prompt injection surface.

```toml
[mcp]
default_env_isolation = true   # strip env for all stdio servers by default

[[mcp.servers]]
id              = "untrusted"
command         = "npx"
args            = ["-y", "some-mcp-server"]
lock_tool_list  = true         # reject tool list changes after startup
env_isolation   = true         # explicit per-server override
```

## MCPShield trust calibration

`MCPShield` assigns a per-server trust score that starts at 1.0 and degrades on anomalous events: tool definition mutations between `tools/list_changed` cycles, sanitization hits, unexpected tool names, and tool execution errors. When the trust score drops below `shield.quarantine_threshold`, the server is quarantined and its tools are excluded from the registry until the score recovers (exponential half-life decay).

```toml
[mcp.shield]
enabled               = true
quarantine_threshold  = 0.4    # score below which a server is quarantined
decay_half_life_secs  = 3600   # half-life for trust score recovery
```

**Tip:**
> View per-server trust scores in the TUI with `mcp:list` from the command palette — the trust column shows the current score and a coloured indicator (green ≥ 0.7, yellow ≥ 0.4, red < 0.4).

## Structured error codes

Every `McpError::ToolCall` carries a typed `McpErrorCode` that the agent uses to decide whether to retry:

| Code | Retryable | When |
|------|-----------|------|
| `Transient` | Yes | Temporary failure; connection drops, timeouts |
| `RateLimited` | Yes | Server asked to back off |
| `ServerError` | Yes | Internal server error |
| `InvalidInput` | No | Bad parameters — retrying unchanged will fail again |
| `AuthFailure` | No | Token invalid or expired |
| `NotFound` | No | Tool or resource does not exist |
| `PolicyBlocked` | No | Blocked by policy rule or OAP authorization |

Errors that do not carry an explicit code (timeouts, connection failures, SSRF blocks) are mapped automatically. `McpErrorCode::is_retryable()` is the authoritative retry gate used by the agent loop.

## OAP authorization

Tool calls can be authorized declaratively via `[tools.authorization]` in config. Rules are appended after `[tools.policy]` rules using first-match-wins semantics. OAP is disabled by default.

```toml
[tools.authorization]
enabled = true

[[tools.authorization.rules]]
action = "allow"
tools  = ["read_file", "list_directory"]

[[tools.authorization.rules]]
action = "deny"
tools  = ["shell"]
```

Denied calls return `McpErrorCode::PolicyBlocked` and are not retried.

## Tool call quota

Limit the total number of tool calls per agent session:

```toml
[tools]
max_tool_calls_per_session = 100   # None = unlimited (default)
```

Only the first attempt counts against the quota — retries of a failed call are free.

## Configuration

```toml
[[mcp.servers]]
id = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
env = {}

[[mcp.servers]]
id = "fetch"
command = "uvx"
args = ["mcp-server-fetch"]
```

**Note:**
> Statically configured servers (from `[[mcp.servers]]`) bypass SSRF validation to allow connections to `localhost` and private IPs. Dynamically added servers retain full SSRF protection.

## Features

| Feature | Description |
|---------|-------------|
| `mock` | Enables `MockMcpClient` for downstream tests |

## Installation

```bash
cargo add zeph-mcp
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

MIT
