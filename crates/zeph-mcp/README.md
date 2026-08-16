# zeph-mcp

[![Crates.io](https://img.shields.io/crates/v/zeph-mcp)](https://crates.io/crates/zeph-mcp)
[![docs.rs](https://img.shields.io/docsrs/zeph-mcp)](https://docs.rs/zeph-mcp)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

MCP client with multi-server lifecycle and Qdrant tool registry for Zeph.

## Overview

Implements the Model Context Protocol client for Zeph, managing connections to multiple MCP servers, discovering their tools at startup, and routing tool calls through a unified executor. Built on [rmcp](https://crates.io/crates/rmcp) 3.1.

## Key Modules

- **client** — low-level MCP transport and session handling; `ToolListChangedHandler` receives `tools/list_changed` notifications, applies `sanitize_tools()` (rate-limited to once per 5 s per server, capped at 100 tools), and forwards the sanitized list to `McpManager` via a refresh channel
- **manager** — `McpManager`, `McpTransport`, `ServerEntry` for multi-server lifecycle; command allowlist validation (npx, uvx, node, python3, docker, mcpls, etc.), env var blocklist (LD_PRELOAD, DYLD_*, NODE_OPTIONS, etc.), and path separator rejection. Split into `connect`, `call`, `ingest`, `retry`, `server`, and `builder` submodules
- **sanitize** — `sanitize_tools()` applied to all tool definitions at registration time and again on every `tools/list_changed` refresh; strips the 27 shared `zeph_common::patterns::RAW_INJECTION_PATTERNS`, Unicode Cf-category characters, and caps descriptions at `mcp.max_description_bytes` (default 2048); fields triggering a pattern are replaced with `"[sanitized]"` — tool registration is never blocked
- **executor** — `McpToolExecutor` bridging MCP tools into the `ToolExecutor` trait; propagates `caller_id` from sub-agent dispatches to the audit log and (when configured) validated images into `ToolOutput.media`
- **registry** — `McpToolRegistry` for tool lookup and optional Qdrant-backed search
- **semantic_index** — `SemanticToolIndex` for embedding-ranked tool discovery
- **pruning** — `PruningCache`, the per-message tool-set cache
- **oauth** — OAuth 2.1 callback listener used by `McpTransport::OAuth` connections; binds the callback port before the browser flow starts, then awaits the `?code=…&state=…` redirect
- **elicitation** — `elicitation/create` handling with a phishing-prevention header
- **roots** — the `roots/list` handler
- **attestation** / **trust_score** — `expected_tools` attestation and persistent per-server trust scoring
- **tool** — `McpTool` wrapper with schema and metadata
- **prompt** — MCP prompt template support
- **error** — `McpError` error types with typed `McpErrorCode` for retry classification (`Transient`, `RateLimited`, `InvalidInput`, `AuthFailure`, `ServerError`, `NotFound`, `PolicyBlocked`)

## Startup auto-retry

When an MCP server fails to connect at startup, `McpManager` retries with exponential backoff:
`jitter(min(startup_retry_backoff_ms * 2^(attempt - 1), 8000 ms))`. Jitter is full-jitter,
AWS-style, over `[nominal * 3/4, nominal]`, so concurrent servers do not reconnect in lockstep.

| Field | Type | Default | Description |
|---|---|---|---|
| `max_connect_attempts` | u8 | `3` | Connect attempts per server at startup. Must be in `1..=10`; out-of-range values are rejected at parse time |
| `startup_retry_backoff_ms` | u64 | `1000` | Base delay before the first retry; doubles per attempt, capped at 8 s |

```toml
[mcp]
max_connect_attempts     = 5
startup_retry_backoff_ms = 1000
```

> [!NOTE]
> Both settings are global — there is no per-server override. Dynamic `add_server` calls retain
> single-attempt behaviour regardless of `max_connect_attempts`.

HTTP 4xx authentication errors (`401`, `403`) are mapped to `McpError::HttpAuth` and are not retried — a permanent auth failure will not exhaust the retry budget.

> [!TIP]
> Increase `max_connect_attempts` for servers that have slow cold-start times (e.g. Docker-based servers that pull images on first run).

## MCP Roots protocol

The MCP client implements the `roots/list` handler, exposing configured project roots to MCP servers. Roots are declared **per server** via `roots` on a `[[mcp.servers]]` entry and passed to that server's connection at initialization time. Servers that support `roots/list` can use this information to scope their file system access to the declared directories.

```toml
[[mcp.servers]]
id      = "filesystem"
command = "npx"
args    = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[[mcp.servers.roots]]
uri  = "file:///workspace/myproject"
name = "project"
```

## Semantic tool discovery

`SemanticToolIndex` indexes all registered MCP tool definitions as embedding vectors in Qdrant (or the SQLite vector backend). On each LLM turn, only the top-K most relevant tools — ranked by cosine similarity to the current query — are included in the tools array sent to the model. This keeps the tools payload small for models with narrow context windows and reduces prompt injection surface area.

| Field | Type | Default | Description |
|---|---|---|---|
| `strategy` | `"none"` \| `"embedding"` \| `"llm"` | `"none"` | Discovery strategy. `none` passes every tool through |
| `top_k` | usize | `10` | Top-scoring tools included per turn (embedding strategy) |
| `min_similarity` | f32 | `0.2` | Minimum cosine similarity for inclusion (embedding strategy) |
| `embedding_provider` | provider name | `""` | Name from `[[llm.providers]]`; empty = the agent's default embedding provider |
| `always_include` | `Vec<String>` | `[]` | Tool names included regardless of score |
| `min_tools_to_filter` | usize | `10` | Skip discovery entirely below this tool count |
| `strict` | bool | `false` | Treat an embedding failure as a hard error instead of falling back to all tools |

```toml
[mcp.tool_discovery]
strategy            = "embedding"
top_k               = 20
min_similarity      = 0.35
embedding_provider  = "fast"
min_tools_to_filter = 10
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
> The `embedding` strategy requires an embedding model — set `embedding_provider` to a name from `[[llm.providers]]`, or leave it empty to use the agent's default embedding provider. With `strict = false` (the default) an embedding failure falls back to passing all tools through rather than failing the turn.

## Per-message pruning cache

`PruningCache` tracks which tool set was sent in the previous LLM request. If the ranked tool list for the current turn is identical, the cache returns the pre-serialized JSON blob directly, skipping re-serialization and re-ranking.

Cache invalidation triggers on: new tool registered, tool removed, `tools/list_changed` notification, or config reload. No manual configuration is required; the cache is always active when `[mcp.tool_discovery] enabled = true`.

## Tool attestation

`expected_tools` in a server config entry declares the tool names that server is authorised to expose. A tool appearing in `tools/list` that is not in `expected_tools` is logged as a security warning; for `untrusted` and `sandboxed` servers it is also filtered out of the registry. For `trusted` servers the warning is logged but the tool is kept.

```toml
[[mcp.servers]]
id             = "filesystem"
command        = "npx"
args           = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
expected_tools = ["read_file", "write_file", "list_directory"]
```

> [!IMPORTANT]
> An empty (or omitted) `expected_tools` means attestation is **skipped**, not that all tools are
> blocked — every tool the server advertises is accepted. To restrict which tools a server may
> expose, use `tool_allowlist` together with a non-`trusted` `trust_level`.

`McpManager` also caches each server's tool fingerprints (Blake3 of name + description +
`input_schema`) across reconnects. On the next connect or `tools/list_changed` refresh, a tool
whose description or schema silently changed since the previous session logs a schema-drift
("rug-pull") warning — detection only, no automatic blocking.

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
- **Tool-list snapshot locking** — set `lock_tool_list = true` under `[mcp]` to reject any `tools/list_changed` refresh after the initial snapshot. Prevents malicious servers from injecting new tools mid-session. This is a global switch, not a per-server field.
- **Per-server stdio env isolation** — `env_isolation = true` (or `default_env_isolation = true` globally) strips the inherited process environment before spawning stdio MCP servers, preventing accidental secret leakage via `PATH`, `HOME`, and similar variables. Explicitly declared `env` keys are still passed through.
- **Intent-anchor nonce boundaries** — tool output from MCP servers is wrapped with per-call nonce delimiters before entering the LLM context, reducing prompt injection surface.
- **Schema depth-cap dropping** — both `input_schema` and `output_schema` are dropped to an empty object when a tool definition nests past `MAX_SCHEMA_DEPTH` (10 levels), closing an injection vector where a malicious server buries a payload too deep for pattern matching to reach. Each drop counts as an injection for trust-score purposes; the `input_schemas_dropped`/`output_schemas_dropped` counters are surfaced through `ServerConnectOutcome`/`McpServerStatus` into the TUI.
- **Bounded cross-reference regex cache** — `name_referenced_in`'s per-tool-name regex caches are capped at 256 entries via `lru::LruCache`, so a server that rotates its advertised tool names cannot grow memory unbounded over the lifetime of a long-running daemon/gateway process.

```toml
[mcp]
default_env_isolation = true   # strip env for all stdio servers by default
lock_tool_list        = true   # reject tool list changes after startup (global)

[[mcp.servers]]
id            = "untrusted"
command       = "npx"
args          = ["-y", "some-mcp-server"]
trust_level   = "sandboxed"    # strict: only allowlisted tools are exposed
env_isolation = true           # explicit per-server override
```

## Trust calibration

`ServerTrustScore` tracks a persistent per-server score in `[0.0, 1.0]`, starting at `0.5`
(neutral). Successful tool calls raise it; failures and injection detections lower it.
`recommended_trust_level()` maps the current score onto an `McpTrustLevel` for runtime gating,
and scores are persisted through `TrustScoreStore` so they survive agent restarts.

Decay is **asymmetric**: only scores above the `0.5` neutral point decay over time. A
low-scoring server must earn trust back through successful calls — it cannot recover by
waiting.

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable trust calibration (opt-in) |
| `probe_on_connect` | bool | `true` | Run the pre-invocation probe on connect |
| `monitor_invocations` | bool | `true` | Update trust scores from invocation outcomes |
| `persist_scores` | bool | `true` | Persist scores to SQLite |
| `decay_rate_per_day` | f64 | — | Per-day decay applied to scores above `0.5` |
| `injection_penalty` | f64 | — | Score penalty applied when injection is detected |
| `verifier_provider` | provider name | `""` | Optional LLM provider for trust verification; empty = disabled |

```toml
[mcp.trust_calibration]
enabled             = true
probe_on_connect    = true
monitor_invocations = true
persist_scores      = true
```

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
effect = "allow"
tool   = "read_file"

[[tools.authorization.rules]]
effect = "deny"
tool   = "shell"
```

Each rule carries a single `tool` glob and an `effect` of `"allow"` or `"deny"`. Denied calls
return `McpErrorCode::PolicyBlocked` and are not retried.

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

Per-server `trust_level` governs SSRF validation and tool exposure:

| `trust_level` | SSRF | Tool exposure |
|---|---|---|
| `"trusted"` | Skipped — `localhost` and private IPs reachable | All tools exposed |
| `"untrusted"` (default) | Enforced | Fails closed (zero tools) with no `tool_allowlist`, unless `allow_untrusted_without_allowlist = true` |
| `"sandboxed"` | Enforced | Only allowlisted tools; empty allowlist = no tools |

> [!CAUTION]
> `trust_level = "trusted"` is the only thing that bypasses SSRF validation — being declared
> statically in `[[mcp.servers]]` does not. Reserve it for operator-controlled servers you need
> to reach over `localhost` or a private IP.

## MCP image passthrough

Servers with `media_passthrough = true` may return `ContentBlock::Image` blocks that are
validated by `zeph_sanitizer::MediaSanitizer` and attached to `ToolOutput.media` as native
image parts. Validation covers magic-byte vs declared-MIME agreement, a format allowlist, an
encoded byte cap, and decoded dimension/pixel caps (decompression-bomb defense).

Global caps live under `[mcp.media]` and apply to every passthrough-enabled server:

| Field | Type | Default | Description |
|---|---|---|---|
| `max_image_bytes` | usize | `5242880` (5 MiB) | Encoded size cap, checked before any decode |
| `max_dimension_px` | u32 | `8192` | Maximum width or height of the decoded image |
| `max_pixels` | u64 | `64000000` | Maximum total pixel count (width × height) |
| `max_images_per_result` | usize | `4` | Images validated/attached per single tool result |
| `max_images_per_turn` | usize | `8` | Images attached per turn across all tool calls in the batch |
| `allowed_formats` | `Vec<String>` | `["jpeg", "png", "gif", "webp"]` | Permitted image formats |

```toml
[mcp.media]
max_image_bytes       = 5242880
max_images_per_result = 4
allowed_formats       = ["png", "jpeg"]

[[mcp.servers]]
id                = "screenshot"
command           = "uvx"
args              = ["mcp-server-screenshot"]
media_passthrough = true
```

> [!IMPORTANT]
> Passthrough is off unless the server sets `media_passthrough = true` **and** a `MediaSanitizer`
> is attached. Servers at `trust_level = "sandboxed"` never pass images through, regardless of
> the flag. A rejected image is not fatal — the rendered text placeholder always remains as the
> fallback.

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `sqlite` | yes | SQLite backend (via `zeph-db`, `zeph-memory`, `zeph-tools`) |
| `postgres` | no | PostgreSQL backend |
| `mock` | no | Exposes `MockMcpCaller` for downstream tests |
| `test-utils` | no | Test utilities and testcontainers for PostgreSQL integration tests (implies `postgres`) |
| `profiling` | no | Extra tracing spans for latency profiling |

## Installation

```bash
cargo add zeph-mcp
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
