# zeph-acp

[![Crates.io](https://img.shields.io/crates/v/zeph-acp)](https://crates.io/crates/zeph-acp)
[![docs.rs](https://img.shields.io/docsrs/zeph-acp)](https://docs.rs/zeph-acp)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

ACP (Agent Client Protocol) server adapter for embedding Zeph in IDE environments.

## Overview

Implements the [Agent Client Protocol](https://agentclientprotocol.org) server side, allowing IDEs and editors to drive the Zeph agent loop over stdio, HTTP+SSE, or WebSocket transports. The crate wires IDE-proxied capabilities — file system access, terminal execution, and permission gates — into the agent loop via `AcpContext`, exposes `AgentSpawner` as the integration point for the host application, and supports runtime model switching via `ProviderFactory` and MCP server management via `ext_method`.

## Installation

```toml
[dependencies]
zeph-acp = "0.1"

# With HTTP+SSE transport
zeph-acp = { version = "0.1", features = ["acp-http"] }
```

> [!IMPORTANT]
> Requires Rust 1.88 or later.

## Features

| Feature | Description | Default |
|---------|-------------|---------|
| `acp-http` | HTTP+SSE transport via axum (`AcpHttpState`, `acp_router`, `post_handler`, `get_handler`) | No |

> [!TIP]
> Enable `acp-http` only when deploying Zeph as a network-accessible ACP endpoint. The default stdio transport is sufficient for local IDE integrations.

## Key modules

| Module | Description |
|--------|-------------|
| `agent` | `AcpContext` — IDE-proxied capabilities (file executor, shell executor, permission gate, cancel signal) wired into the agent loop per session; `AgentSpawner` factory type; `ZephAcpAgent` ACP protocol handler with multi-session support, LRU eviction, idle reaper, SQLite persistence, rich content support (images, embedded resources, tool locations), runtime model switching via `ProviderFactory`, and MCP server management via `ext_method` |
| `transport` | `serve_stdio` / `serve_connection` (stdio), HTTP+SSE handlers (`post_handler`, `get_handler`), WebSocket handler (`ws_upgrade_handler`), duplex bridge, axum router; `AcpServerConfig` |
| `fs` | `AcpFileExecutor` — file system executor backed by IDE-proxied ACP file operations |
| `terminal` | `AcpShellExecutor` — shell executor backed by IDE-proxied ACP terminal |
| `permission` | `AcpPermissionGate` — forwards tool permission requests to the IDE for user approval; persists "always allow/deny" decisions to TOML file |
| `mcp_bridge` | `acp_mcp_servers_to_entries` — converts ACP-advertised MCP servers (Stdio, Http, Sse) into `McpServerEntry` configs |
| `error` | `AcpError` typed error enum |

**Re-exports:** `AcpContext`, `AgentSpawner`, `ProviderFactory`, `AcpError`, `AcpFileExecutor`, `AcpPermissionGate`, `AcpShellExecutor`, `AcpServerConfig`, `serve_connection`, `serve_stdio`, `acp_mcp_servers_to_entries`

**Re-exports (feature `acp-http`):** `SendAgentSpawner`, `AcpHttpState`, `acp_router`

## AcpContext

`AcpContext` carries per-session IDE capabilities into the agent loop. Each field is `None` when the IDE did not advertise the corresponding capability:

```rust
pub struct AcpContext {
    pub file_executor: Option<AcpFileExecutor>,
    pub shell_executor: Option<AcpShellExecutor>,
    pub permission_gate: Option<AcpPermissionGate>,
    /// Notify to interrupt the running agent operation.
    pub cancel_signal: Arc<Notify>,
}
```

The `cancel_signal` is shared with the agent's `LoopbackHandle` so that an IDE cancel request immediately interrupts the running inference loop.

## Protocol methods

### AgentCapabilities (G3)

The `initialize` response advertises enriched capabilities:

```rust
acp::AgentCapabilities::new()
    .load_session(true)
    .meta({
        cap_meta.insert("config_options", json!(true));
        cap_meta.insert("ext_methods", json!(true));
        cap_meta
    })
```

This signals to the IDE that the agent supports session config options (`session/configure`) and custom `ext_method` extensions.

### set_session_mode (G2)

`ZephAcpAgent` implements `set_session_mode` to handle IDE-driven mode switches per session:

- Validates that the target session exists; returns `invalid_request` error if not found.
- Logs the `session_id` and `mode_id` at debug level.
- Currently a no-op acknowledgement — mode semantics are handled by the IDE.

### ext_notification (G4)

`ZephAcpAgent` implements `ext_notification` to accept IDE-originated fire-and-forget notifications:

- Logs the notification method name at debug level.
- Returns `Ok(())` for all known and unknown methods — unrecognized notifications are silently accepted.

## MCP transport support (G8)

`acp_mcp_servers_to_entries` converts ACP-advertised MCP servers into `zeph-mcp` `ServerEntry` configs. Three transport types are supported:

| ACP variant | Mapped transport | Notes |
|-------------|-----------------|-------|
| `McpServer::Stdio` | `McpTransport::Stdio` | Env vars forwarded as-is to child process |
| `McpServer::Http` | `McpTransport::Http` | Streamable HTTP via rmcp |
| `McpServer::Sse` | `McpTransport::Http` | Legacy SSE mapped to streamable HTTP (backward-compatible) |

> [!NOTE]
> SSE is a legacy MCP transport. rmcp's `StreamableHttpClientTransport` handles both SSE and streamable HTTP endpoints, so both variants map to `McpTransport::Http`.

```rust
use zeph_acp::acp_mcp_servers_to_entries;

let entries = acp_mcp_servers_to_entries(&initialize_request.mcp_servers);
// entries: Vec<ServerEntry> ready for McpManager::start_all
```

## HTTP+SSE transport (feature `acp-http`)

Enable the `acp-http` feature to expose Zeph over HTTP with Server-Sent Events:

```rust
use zeph_acp::{AcpHttpState, AcpServerConfig, acp_router};

let state = AcpHttpState::new(spawner, AcpServerConfig::default());
state.start_reaper(); // prune idle connections every 60 s

let app = acp_router(state);
// mount app into your axum Router
```

Endpoints:

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/acp` | Send a JSON-RPC request; stream responses as SSE. Creates a new connection when `Acp-Session-Id` header is absent. |
| `GET` | `/acp` | Reconnect to an existing connection's SSE stream. Requires `Acp-Session-Id` header. |
| `GET` | `/acp/ws` | WebSocket upgrade for bidirectional streaming. |

Session IDs are UUIDs returned in the `Acp-Session-Id` response header. Idle connections (beyond `session_idle_timeout_secs`) are reaped by a background task.

> [!TIP]
> Use `SendAgentSpawner` (the `Send`-safe variant of `AgentSpawner`) when constructing `AcpHttpState`. This satisfies axum's `State` requirement for `Send + Sync`.

## Rich content

ACP prompts can carry multi-modal content blocks beyond plain text:

- **Images** — base64-encoded image blocks (`image/jpeg`, `image/png`, `image/gif`, `image/webp`) are decoded and forwarded to the LLM provider as inline attachments. Oversized payloads and unsupported MIME types are skipped with a warning.
- **Embedded resources** — `TextResourceContents` blocks are injected into the prompt text wrapped in `<resource>` markers.
- **Tool locations** — tool call results can include file path locations (`ToolCallLocation`) that the IDE uses for source navigation.
- **Thinking chunks** — intermediate reasoning status events are streamed back to the IDE as `session/update` events.

## Model switching

The IDE can switch the active LLM model at runtime via `session/configure` with `config_id = "model"`. `ZephAcpAgent` uses a `ProviderFactory` closure that resolves a `"provider:model"` key to an `AnyProvider`, and an `available_models` allowlist that populates the IDE dropdown. The resolved provider is stored in a shared `Arc<RwLock<Option<AnyProvider>>>` (`provider_override`) that the agent loop checks on each turn.

## MCP server management

`ext_method` handles custom JSON-RPC extensions for managing MCP servers at runtime:

| Method | Description |
|--------|-------------|
| `_agent/mcp/list` | List active MCP servers |
| `_agent/mcp/add` | Register a new MCP server |
| `_agent/mcp/remove` | Remove a running MCP server |

Requires a shared `McpManager` reference set via `AcpServerConfig::mcp_manager`.

## Session lifecycle

`ZephAcpAgent` manages multiple concurrent sessions with the following capabilities:

- **LRU eviction** — when the number of active sessions reaches `max_sessions`, the least-recently-used session is evicted to free resources.
- **SQLite persistence** — session events are persisted to `acp_sessions` and `acp_session_events` tables (migration 013) via `zeph-memory`. This enables session resume across process restarts.
- **Session resume** — `load_session` replays persisted history as `session/update` events, restoring the conversation state.
- **Idle reaper** — a background task periodically removes sessions that have been idle longer than `session_idle_timeout_secs`.

### Configuration

| Config field | Type | Default | Env override |
|-------------|------|---------|--------------|
| `acp.max_sessions` | usize | `16` | `ZEPH_ACP_MAX_SESSIONS` |
| `acp.session_idle_timeout_secs` | u64 | `1800` | `ZEPH_ACP_SESSION_IDLE_TIMEOUT_SECS` |
| `acp.permission_file` | PathBuf | `~/.config/zeph/acp-permissions.toml` | `ZEPH_ACP_PERMISSION_FILE` |
| `acp.available_models` | `Vec<String>` | `[]` | — |

## Permission persistence

When the IDE user selects "always allow" or "always deny" for a tool, `AcpPermissionGate` persists the decision to a TOML file (`~/.config/zeph/acp-permissions.toml` by default). On next session startup the gate pre-populates its cache from this file, skipping redundant IDE prompts.

- Atomic write via temp file + rename to prevent corruption.
- File permissions set to `0o600` (owner-only).
- Graceful fallback: if the file is missing or malformed, the gate starts with an empty cache.

## AgentSpawner

`AgentSpawner` is the integration contract between `zeph-acp` and the host application:

```rust
pub type AgentSpawner = Arc<
    dyn Fn(LoopbackChannel, Option<AcpContext>) -> Pin<Box<dyn Future<Output = ()> + 'static>>
        + 'static,
>;
```

The host constructs an `AgentSpawner` closure that wires `AcpContext` capabilities into `Agent` via `with_cancel_signal()` on the builder, then passes the closure to `serve_stdio` or `serve_connection`.

For HTTP transport, use `SendAgentSpawner` which requires `Send + Sync`:

```rust
pub type SendAgentSpawner = Arc<
    dyn Fn(LoopbackChannel, Option<AcpContext>) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>
        + Send + Sync + 'static,
>;
```

## Custom methods

`ZephAcpAgent` exposes vendor-specific extensions via `ExtRequest` dispatch. The `custom` module matches on `req.method` and routes to the appropriate handler. Unrecognized methods return `None`, allowing the ACP runtime to respond with "method not found".

| Method | Description |
|--------|-------------|
| `_session/list` | List all sessions (in-memory + persisted via `SqliteStore::list_acp_sessions`) |
| `_session/get` | Get session details and event history |
| `_session/delete` | Remove session from memory and SQLite |
| `_session/export` | Export session events as a portable JSON payload |
| `_session/import` | Import events into a new session (UUID assigned server-side) |
| `_agent/tools` | Return the list of tools available to the agent |
| `_agent/working_dir/update` | Change the working directory for a session |

### Security guards

- **Session ID validation** — IDs must be at most 128 characters, restricted to `[a-zA-Z0-9_-]`. Rejects control characters, slashes, and whitespace.
- **Path traversal protection** — `_agent/working_dir/update` rejects any path containing `..` (`Component::ParentDir`).
- **Import size cap** — `_session/import` rejects payloads exceeding 10,000 events.

### Auth hints in `initialize`

The `initialize` response includes an `auth_hint` key in its metadata map. For stdio transport (trusted local client) this is a generic `"authentication required"` string. IDEs can use this hint to prompt the user for credentials before issuing further requests.

## License

MIT
