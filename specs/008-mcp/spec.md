# Spec: MCP Client
## Sources
### External- MCP specification (2025-11-25): https://modelcontextprotocol.io/specification/2025-11-25.md
- rmcp crate (Rust SDK): https://crates.io/crates/rmcp

### Internal| File | Contents |
|---|---|
| `crates/zeph-mcp/src/` | `McpManager`, server lifecycle, tool registry |
| `crates/zeph-tools/src/composite.rs` | `McpExecutor` in composite chain |
| `crates/zeph-tui/src/channel.rs` | `suppress_stderr` integration |

---

`crates/zeph-mcp/` — Model Context Protocol client, multi-server lifecycle.

## Architecture
```
McpManager
├── servers: HashMap<String, McpServer>  — name → server instance
├── tool_registry: Qdrant-backed         — semantic tool search across all servers
└── suppress_stderr: bool                — must be true in TUI mode
```

## Server Lifecycle
1. **Startup** (`connect_all`) — two phases:
   - **Phase 1**: non-OAuth servers connect concurrently
   - **Phase 2**: OAuth servers connect sequentially (each may require browser interaction)
2. **Transport**: stdio child process (primary) or HTTP with optional auth (Mode A/B — see below)
3. **Capability negotiation**: `initialize` → `tools/list` → register in tool registry
4. **Health**: each server has a heartbeat; dead servers are restarted automatically
5. **Shutdown**: graceful SIGTERM → wait → SIGKILL fallback

## HTTP Authentication Modes
### Mode A — Static headers
Arbitrary request headers injected on every HTTP request. Header values support embedded vault
references (`${VAULT_KEY}`) resolved at startup.

```toml
[[mcp.servers]]
id = "todoist"
url = "https://api.todoist.com/mcp/v1"
[mcp.servers.headers]
Authorization = "Bearer ${TODOIST_API_TOKEN}"
```

- `headers` and `oauth.enabled` are mutually exclusive per server (validated at config load)
- Invalid header values (non-ASCII, control chars) are dropped with a `warn!` log

### Mode B — OAuth 2.1 with PKCE
Full authorization code flow with PKCE (S256). Implemented via rmcp `auth` feature.

```toml
[[mcp.servers]]
id = "todoist-oauth"
url = "https://ai.todoist.net/mcp"
[mcp.servers.oauth]
enabled = true
token_storage = "vault"   # "vault" | "memory"
callback_port = 18766     # localhost callback port
```

**Flow**:
1. Pre-bind `TcpListener` on `127.0.0.1:{callback_port}` before client registration (resolves redirect_uri port)
2. Discover OAuth metadata via RFC 8414 / RFC 9728 — all resolved endpoints validated via SSRF guard
3. Dynamic client registration (RFC 7591)
4. Display authorization URL: CLI via `eprintln!`, TUI via `status_tx` spinner "Waiting for OAuth...", Telegram via stderr
5. Callback server reads buffered TCP until `\r\n\r\n` (handles packet fragmentation)
6. Exchange code → tokens, persist via `VaultCredentialStore`
7. Subsequent runs: load tokens from vault, proactive TTL-based refresh

**Token vault keys**: `mcp.oauth.{server_id}.{access_token,refresh_token,expires_at}`

**Security**: PKCE S256 via rmcp, CSRF state validated and deleted after single use, callback binds `127.0.0.1` only, all OAuth metadata endpoints SSRF-checked

## Tool Discovery
- Tools from all MCP servers are merged into the main tool catalog each turn
- Qdrant-backed semantic registry: allows finding tools by description, not just exact name
- `McpExecutor` in `CompositeExecutor` handles routing of `ToolCall` to correct server
- Tool names are namespaced: `{server_name}__{tool_name}` to avoid collisions

## Protocol Invariants (MCP 2025-11-25)
- JSON-RPC 2.0 over stdio or HTTP+SSE
- `initialize` / `initialized` handshake required before any tool calls
- `tools/call` response must include `isError: bool` field
- Server stderr must be captured and suppressed in TUI mode — not propagated to user
- Tool input schema is JSON Schema — forwarded verbatim to LLM `chat_with_tools`

## Key Invariants
- `McpManager` is `Arc<>` — shared between agent loop and background restart tasks
- `suppress_stderr = true` is mandatory when TUI is active — failure to do so corrupts TUI output
- Tool names must be namespaced to prevent collisions across servers
- Server restart is automatic — agent must not error on transient server failures
- Qdrant tool registry is optional — falls back to exact-name lookup if Qdrant unavailable
- `headers` and `oauth.enabled` are mutually exclusive on the same server entry
- OAuth token storage uses `VaultCredentialStore` backed by age vault — tokens survive restarts
- Token refresh is proactive (TTL-based) only; no 401 retry (upstream rmcp limitation)
- `StoredCredentials` in rmcp does not implement `Zeroize` — accepted upstream limitation, vault at-rest encrypted
- All OAuth servers are skipped (with warning) on auth failure — non-fatal, matching existing server error behavior

## Sources Update
- [#1930](https://github.com/bug-ops/zeph/issues/1930) — OAuth 2.1 support for remote MCP servers (implemented in PR #1937)
- rmcp `auth` feature: `OAuthState`, `AuthorizationManager`, `VaultCredentialStore`, `InMemoryStateStore`
- MCP Authorization spec: https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization/

---

## Server Trust Levels
`[[mcp.servers]]` now accepts `trust_level` and `tool_allowlist`. Issue #2178.

### Trust Levels
| Level | SSRF guard | Allowlist enforcement |
|---|---|---|
| `trusted` | Skipped | All tools exposed |
| `untrusted` (default) | Enforced | Warns if allowlist empty |
| `sandboxed` | Enforced | Fail-closed (empty allowlist = zero tools) |

### Config
```toml
[[mcp.servers]]
id = "my-server"
trust_level = "untrusted"   # default
tool_allowlist = ["tool_a", "tool_b"]  # empty = all (untrusted) or none (sandboxed)
```

`--migrate-config` sets `trust_level = "trusted"` on all existing servers to preserve prior SSRF-bypass behavior.

### Key Invariants
- `sandboxed` with empty `tool_allowlist` exposes zero tools — fail-closed, not fail-open
- `untrusted` with empty allowlist emits a `WARN` but still exposes all tools (not fail-closed)
- `trusted` always skips SSRF validation regardless of URL
- SSRF guard must run before tool exposure for `untrusted` and `sandboxed` servers
- NEVER expose `sandboxed` tools not listed in `tool_allowlist` — even if SSRF passes
- `--init` wizard must prompt for trust level on remote (`http`/`sse`) servers

---

## MCP Semantic Tool Discovery
`crates/zeph-mcp/src/discovery/` (`SemanticToolIndex`). Implemented in v0.18.0.

### Overview
`SemanticToolIndex` replaces the Qdrant-backed registry for per-turn tool
relevance filtering. It computes embeddings for all MCP tool descriptions
concurrently at startup, caches them in memory, and selects the top-K most
relevant tools per user message via cosine similarity.

### ToolDiscoveryStrategy
```rust
pub enum ToolDiscoveryStrategy {
    Embedding,  // cosine similarity between query and tool description embeddings
    Llm,        // LLM-based relevance judgment (slower, more accurate)
    None,       // all tools exposed every turn (legacy behavior)
}
```

Default: `Embedding`.

### Semantic Tool Index
`SemanticToolIndex` holds:
- `tool_embeddings: HashMap<String, Vec<f32>>` — tool_name → embedding vector
- `strategy: ToolDiscoveryStrategy`
- `embedding_provider: AnyProvider` — provider used for tool embedding and query embedding

Embeddings are computed concurrently for all tools at `McpManager::connect_all()`.
When a new server connects or `tools/list_changed` notification is received, the
index is rebuilt for that server's tools.

### Config
```toml
[mcp.tool_discovery]
strategy = "embedding"      # "embedding" | "llm" | "none"
top_k = 10                  # max tools returned per turn
min_similarity = 0.3        # minimum cosine similarity to include a tool
always_include = []         # tool names always included regardless of similarity
min_tools_to_filter = 5     # skip filtering when tool count <= this threshold
```

When `min_tools_to_filter >= total_tool_count`, all tools are returned without
filtering (avoids overhead when tool count is small).

### Rebuild on `tools/list_changed`
The MCP spec's `tools/list_changed` notification triggers `SemanticToolIndex::rebuild_for_server(server_id)`. The rebuild runs in a background tokio task and swaps the index atomically on completion. The old index remains active until the new one is ready.

### Key Invariants
- `always_include` tools bypass cosine filtering and are always returned — order them first in the result
- When `strategy = "none"`, `SemanticToolIndex` is not initialized and all tools are returned unfiltered
- Embedding computation is concurrent (`tokio::join_all`) — individual tool embedding failures skip that tool with a `WARN`, never abort the whole index
- `min_tools_to_filter` prevents filtering on small tool catalogs where filtering overhead exceeds benefit
- Index rebuild is atomic — the old index is never partially replaced
- NEVER block the agent turn on index rebuild — rebuild is background-only

---

## MCP Per-Message Pruning Cache
`crates/zeph-mcp/src/cache/` (`PruningCache`). Implemented in v0.18.0.

### Overview
`PruningCache` caches the result of tool relevance pruning (the filtered tool list
shown to the LLM) to avoid redundant embedding + cosine computation when the same
user message and tool list recur within a session.

### Cache Key
Each cache entry is keyed on:

```
(message_content_hash: u64, tool_list_hash: u64)
```

Both hashes use **BLAKE3** (not SipHash). The content hash covers the raw user
message text. The tool list hash covers the sorted list of tool names and their
description digests.

### CachedResult
```rust
pub enum CachedResult {
    Ok(Vec<ToolDefinition>),     // pruned tool list from a previous successful call
    Failed,                       // previous call failed; skip caching on retry
}
```

`Failed` entries are stored to prevent redundant retry attempts within a turn. They
expire at the start of the next user turn.

### Cache Reset
`PruningCache::reset()` is called at the start of each user turn (before LLM
inference), clearing `Failed` entries and any entries whose `tool_list_hash` no
longer matches the current tool catalog.

The cache is **not** cleared between turns for `Ok` entries — a user asking the
same question twice within a session benefits from the cached result.

### Key Invariants
- Cache keys use BLAKE3 — never use std `HashMap` default hasher for cache keys
- `reset()` MUST be called at the start of every user turn — stale `Failed` entries cause incorrect behavior
- `CachedResult::Failed` entries expire on next turn reset — never persist across turns
- Cache is session-scoped, not persisted to SQLite — it is an in-memory optimization only
- NEVER cache the result of `strategy = "none"` (returns all tools) — no value in caching the full set

---

## MCP Roots Protocol
> **Status**: Implemented. Closes #2445.

`McpRootEntry` struct and `roots` field on `McpServerConfig`. `ToolListChangedHandler` advertises `roots` capability (`list_changed: false`) and responds to `roots/list` with configured roots. Roots are validated at connection time.

### Config
```toml
[[mcp.servers]]
name = "my-server"
roots = [{ uri = "file:///path/to/project", name = "project" }]
```

### Key Invariants
- Non-`file://` URI roots are rejected at validation time
- Missing paths produce a warning, not a hard error
- `std::fs::canonicalize()` is applied to existing paths — traversal payloads like `file:///etc/../secret` are expanded before passing to servers

---

## MCP Injection Detection Feedback Loop
> **Status**: Implemented. Closes #2459.

`sanitize_tools()` returns `SanitizeResult` (injection count, flagged tools, flagged patterns). Up to `MAX_INJECTION_PENALTIES_PER_REGISTRATION = 3` trust-score penalties applied per registration batch via `apply_injection_penalties()`. Total penalty is capped at 0.75 per batch.

### Key Invariants
- Trust-score penalties are bounded at 0.75 per registration batch — no runaway score collapse
- Server instructions are sanitized (injection patterns replaced with `[sanitized]`) before truncation and storage

---

## Per-Tool Security Metadata
> **Status**: Implemented. Closes #2420.

`ToolSecurityMeta` struct carrying `DataSensitivity` (`None/Low/Medium/High`) and `Vec<CapabilityClass>` (`FilesystemRead/Write`, `Shell`, `Network`, `DatabaseRead/Write`, `MemoryWrite`, `ExternalApi`). `infer_security_meta()` assigns metadata from tool name keywords at registration. Operator config `[mcp.servers.tool_metadata]` overrides heuristics per tool.

### Data-Flow Policy
`check_data_flow()` blocks `High`-sensitivity tools on `Untrusted`/`Sandboxed` servers at registration. `Medium`-sensitivity on `Sandboxed` emits a warning but is permitted.

### Config
```toml
[mcp]
max_description_bytes = 2048
max_instructions_bytes = 2048
```

### Key Invariants
- `McpTrustLevel::restriction_level()` ordering: `Trusted=0 < Untrusted=1 < Sandboxed=2`
- `tool_allowlist = None` means no override (all tools allowed with untrusted warning); `Some(vec![])` is explicit deny-all (fail-closed)
- `BREAKING` : `tool_allowlist` type changed from `Vec<String>` to `Option<Vec<String>>`
- Tool list hash must be recomputed when any server reconnects or `tools/list_changed` fires

---

## MCP Elicitation
> **Status**: Implemented. Protocol method: `elicitation/create`.

Elicitation allows an MCP server to request additional structured input from the user during a tool call. Zeph handles `elicitation/create` by forwarding the request to the active channel (CLI prompts interactively).

### Bounded Channel
Elicitation requests are queued in a bounded `tokio::sync::mpsc` channel (capacity set by `elicitation_queue_capacity`, default 16). When the queue is full, the request is rejected with an error response — the server is not blocked.

### Config
```toml
[mcp]
elicitation_enabled = true        # master switch; default true
elicitation_timeout = 30          # seconds to wait for user response
elicitation_queue_capacity = 16   # bounded channel capacity
elicitation_warn_sensitive_fields = true  # warn when elicited schema fields look sensitive

[[mcp.servers]]
elicitation_enabled = true   # per-server override; inherits global default when unset
```

### Sandboxed Servers
Servers with `trust_level = "sandboxed"` are never permitted to elicit. Elicitation requests from sandboxed servers are rejected with a protocol error — elicitation is a trust escalation path.

### Sensitive Field Warning
When `elicitation_warn_sensitive_fields = true` (default), any elicitation schema field whose name matches sensitive-field heuristics (e.g., `password`, `secret`, `token`, `key`) produces a `WARN` log before the user is prompted. The prompt is still shown — the warning is advisory only.

### Key Invariants
- Sandboxed servers MUST NEVER be permitted to elicit — reject at the protocol layer
- Bounded channel prevents unbounded queue growth — full queue → immediate rejection, not block
- `elicitation_enabled = false` at the global level disables elicitation for all servers regardless of per-server setting
- Per-server `elicitation_enabled = false` disables elicitation for that server only
- Sensitive-field warning fires before the user sees the prompt — never suppress or delay it
- NEVER pass elicitation schema to the LLM — elicitation is a user→server interaction only

---

## Tool Collision Detection
>  **Status**: Implemented.

When two or more MCP servers register tools with the same `sanitized_id` (the normalized tool identifier used in the tool catalog), a `WARN` log is emitted listing all colliding servers and the conflicting `sanitized_id`. Collision detection runs at tool registration time (during `tools/list` response processing and on `tools/list_changed`).

### Key Invariants
- Collision detection is informational — colliding tools are still registered (first-registered wins in the catalog)
- `sanitized_id` is the normalized form used for collision comparison — not the raw tool name
- NEVER silently discard a colliding tool without emitting the `WARN`

---

## Tool-List Snapshot Locking
>  **Status**: Implemented.

When `lock_tool_list = true`, the tool catalog is frozen after the initial `tools/list` response. Subsequent `tools/list_changed` notifications are ignored and logged at `INFO` level. This prevents tool-list mutation attacks from compromised or misbehaving servers.

### Config
```toml
[mcp]
lock_tool_list = false   # default false; set true to freeze catalog after startup
```

### Key Invariants
- `lock_tool_list = true` means `tools/list_changed` notifications are acknowledged but the catalog is not updated
- The lock applies per-manager, not per-server — all servers are frozen when enabled
- NEVER silently drop `tools/list_changed` — always log at `INFO` when locking prevents an update

---

## Per-Server Stdio Env Isolation
>  **Status**: Implemented.

Stdio MCP servers can be launched with a controlled environment. `env_isolation` per-server field and `default_env_isolation` global field control which environment variables are inherited by the child process.

### Config
```toml
[mcp]
default_env_isolation = false   # global default; false = inherit parent env (legacy behavior)

[[mcp.servers]]
env_isolation = true   # per-server override; true = spawn with empty env (only declared vars)
```

When `env_isolation = true` for a server, the child process is spawned with an empty environment. Only variables declared in `[mcp.servers.env]` are passed. When `false`, the full parent process environment is inherited.

### Key Invariants
- `env_isolation = true` overrides the global `default_env_isolation` for that server
- When `env_isolation = true`, ONLY explicitly declared `env` vars reach the child — no implicit inheritance
- Header vault references (`${VAULT_KEY}`) in `env` are still resolved regardless of isolation mode

---

## MCP Error Codes
> **Status**: Implemented. Closes #2479.

`McpErrorCode` enum in `crates/zeph-mcp/src/error.rs`:

| Code | Retryable | Description |
|------|-----------|-------------|
| `Transient` | Yes | Retry likely to succeed |
| `RateLimited` | Yes | Back off and retry |
| `InvalidInput` | No | Do not retry without changing parameters |
| `AuthFailure` | No | Re-authenticate or escalate |
| `ServerError` | Yes | May be transient, retry with backoff |
| `NotFound` | No | Resource or tool does not exist |
| `PolicyBlocked` | No | Blocked by policy rules |

`McpError::ToolCall` carries `code: McpErrorCode`. `McpError::code()` maps all variants to typed codes. Enables caller-side retry classification without string parsing.

### Key Invariants
- `is_retryable()` is the canonical check for retry decisions — never parse error message strings
- `PolicyBlocked` is never retryable — policy decisions are not transient

---

## Caller Identity Propagation
> **Status**: Implemented. Closes #2479.

`ToolCall` gains `caller_id: Option<String>` — propagated from the channel layer to `AuditEntry`. `AuditEntry` records `caller_id` and `policy_match` fields (`skip_serializing_if = None`). Policy gate populates `policy_match` from `PolicyDecision::trace` on every allow/deny decision.

### Key Invariant
- `caller_id` is set at the channel layer, not inside tool executors — never synthesize it from tool context

---

## Intent-Anchor Nonce Wrapper
>  **Status**: Implemented.

Tool output from MCP servers is wrapped with an intent-anchor nonce before injection into context. The nonce ties the tool result to the specific tool call that produced it, preventing result-swapping attacks where a compromised server returns a payload intended to satisfy a different pending tool call.

### Key Invariants
- Nonce is generated per tool call invocation — never reused across calls or turns
- Nonce wrapper is stripped before the content is passed to the LLM — the LLM sees clean tool output
- NEVER use a static or session-scoped nonce — it must be unique per call
