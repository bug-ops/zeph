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

### Tool Management
- Tools merged from all MCP servers into single catalog each turn
- Semantic discovery: Qdrant-backed search by description
- Namespacing: `{server_id}__{tool_name}` to prevent collisions
- Per-message pruning: filter tools by relevance before LLM exposure

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
| `crates/zeph-tools/src/composite.rs` | `McpExecutor` in composite chain |
| `crates/zeph-tui/src/channel.rs` | `suppress_stderr` integration |

---

## See Also

- [[MOC-specs]] — Master index of all specifications
- [[006-tools/spec]] — Tool execution framework
- [[010-security/spec]] — Security and authorization
