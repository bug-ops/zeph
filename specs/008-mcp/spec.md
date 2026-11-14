---
aliases:
  - MCP Client
  - Model Context Protocol
tags:
  - sdd
  - spec
  - mcp
  - tools
  - protocol
  - contract
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[006-tools/spec]]"
  - "[[010-security/spec]]"
---

# Spec: MCP Client (Parent Index)

> [!info]
> MCP client via rmcp, multi-server lifecycle, semantic tool discovery, per-message pruning cache,
> injection detection, elicitation, tool collision detection, caller identity propagation, tool quota.

## Overview

This is the **parent specification** for the MCP (Model Context Protocol) client subsystem.
For detailed information on specific areas, refer to the child specs below.

---

## Child Specifications

| Spec | Topic | Purpose |
|------|-------|---------|
| [[008-1-lifecycle]] | Server Connection | Startup, health monitoring, automatic restart, graceful shutdown |
| [[008-2-discovery]] | Tool Discovery | Semantic registry, namespacing, collision detection, per-message pruning |
| [[008-3-security]] | Security & Auth | OAuth 2.1, SSRF protection, tool allowlisting, trust levels |
| [[008-4-elicitation]] | Server-Driven Elicitation | MCP 2025-06-18 elicitation/create routing, sensitive field warnings, Sandboxed rejection |

---

## System Architecture

```
McpManager
├── servers: HashMap<String, McpServer>  — name → server instance
├── tool_registry: Qdrant-backed         — semantic tool search across all servers
├── pruning_cache: PruningCache          — per-message tool relevance cache
└── suppress_stderr: bool                — must be true in TUI mode
```

---

## Key Contracts

### Server Lifecycle
- Startup: non-OAuth servers connect concurrently, OAuth servers connect sequentially
- Transport: stdio child process (primary) or HTTP with optional auth
- Capability negotiation: `initialize` → `tools/list` → register in tool registry
- Health: each server has heartbeat; dead servers are restarted automatically
- Shutdown: graceful SIGTERM → wait → SIGKILL fallback
- **Retry with jitter**: exponential backoff on connection failure, base configured by `startup_retry_backoff_ms`, capped at 8 s; jitter applies `[nominal * 3/4, nominal]` range (AWS-style); commit #4568

### Per-Server Config Fields (Added)

| Field | Type | Default | Description |
|---|---|---|---|
| `startup_retry_backoff_ms` | `u64` | `1000` | Base interval for exponential backoff on startup failure. Actual delay = `min(base * 2^(attempt-1), 8000) ms` with jitter. Minimum clamped to 1. |
| `tool_timeout_secs` | `Option<u64>` | `None` (no limit) | Per-call timeout for `tools/call` on this server. Max 3600. When set, overrides global timeout for this server's tools. |

Config migration step 50 adds these fields. Both live under `[[mcp.servers]]`.

### Tool Management
- Tools merged from all MCP servers into single catalog each turn
- Semantic discovery: Qdrant-backed search by description
- Namespacing: `{server_id}__{tool_name}` to prevent collisions
- Per-message pruning: filter tools by relevance before LLM exposure
- **Single writer (#7075, #7072, #7074)**: `McpToolList::new(tools)` returns a non-`Clone` `McpToolListWriter` and a `Clone` `McpToolListReader`. The writer owns the exposed tool list and the `McpToolIds` registry (`zeph_tools::McpToolIds`: sanitized tool id to `McpServerId`); `publish_full` replaces both atomically, `publish_pruned` replaces only the exposed list with a subset, so the executor list is always a subset of the registry. The executor holds the reader. The trust gate, shadow sentinel, policy gate, inline and sub-agent loops read the registry through `McpToolIdsHandle::snapshot()` (`McpToolIdsHandle` has no `Default`; it is built only with `with_writer`). When no writer is attached (embedders, tests) classification falls back to `McpToolIds::from_definitions` over the live executor definitions. No `:`-split server id derivation remains; the server id is read from the registry.
- `McpServerId` (`zeph_common::types`, `Arc<str>` identity newtype, no `Default`, no validation) is used by `McpTool.server_id`, `ToolDef.server_id`, `McpToolIds`, `ToolResultSource::Mcp`, `ToolCollision` and the audit entry. The boundary stays `String`/`&str`: `McpManager` maps and its `&str` API, `ServerEntry`/`McpServerConfig.id`.
- `sanitized_id()` is pure (no logging). The over-length truncation WARN is emitted once per tool-list change by the manager diagnostic pass, which also records MCP-vs-MCP id collisions; `McpManager::tool_collisions()` returns them and `/mcp list` shows a "Shadowed tools" section. Dispatch stays first-server-wins.

### Protocol Invariants (MCP 2025-11-25)
- JSON-RPC 2.0 over stdio or HTTP+SSE
- `initialize` / `initialized` handshake required before tool calls
- Tool input schema forwarded verbatim to LLM (JSON Schema)
- Server stderr captured and suppressed in TUI mode
- `tools/call` response includes `isError: bool` field

---

## Sources

### External
- **MCP specification** (2025-11-25): https://modelcontextprotocol.io/specification/2025-11-25.md
- **rmcp crate** (Rust SDK): https://crates.io/crates/rmcp

### Internal
| File | Contents |
|---|---|
| `crates/zeph-mcp/src/` | `McpManager`, server lifecycle, tool registry |
| `crates/zeph-mcp/src/executor.rs` | `McpToolExecutor` in composite chain |
| `crates/zeph-mcp/src/manager/{mod.rs,builder.rs,connect.rs,server.rs,retry.rs}` | `suppress_stderr` integration |

---

## Implementation Notes

- `ToolRefreshEvent` channel is bounded to 16 slots (commit #4488) — prevents unbounded memory growth in high-churn registries
- Cancellation token plumbed through retry loop (commit #4609) — shutdown is clean even during retry wait
- **Breaking change (#6614)**: `McpClient::connect` no longer takes two adjacent positional
  `bool`s (`suppress_stderr`, `env_isolation`) — it takes `StderrPolicy` (`Forward`/`Suppress`)
  and `EnvPolicy` (`InheritAll`/`Isolated`) enums instead, so a future call-site edit can no
  longer silently swap the two and flip security-relevant behavior (full parent-env
  inheritance) without a compiler error. `connect_url`/`connect_url_with_headers`/
  `connect_url_oauth` now take `McpTrustLevel` directly instead of a pre-collapsed
  `trusted: bool`. No runtime behavior change — call sites in `manager/retry.rs` and
  `manager/connect.rs` convert at the boundary.

## See Also

- [[MOC-specs]] — Master index of all specifications
- [[006-tools/spec]] — Tool execution framework
- [[010-security/spec]] — Security and authorization
