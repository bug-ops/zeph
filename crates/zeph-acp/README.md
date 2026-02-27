# zeph-acp

[![Crates.io](https://img.shields.io/crates/v/zeph-acp)](https://crates.io/crates/zeph-acp)
[![docs.rs](https://img.shields.io/docsrs/zeph-acp)](https://docs.rs/zeph-acp)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

ACP (Agent Client Protocol) server adapter for embedding Zeph in IDE environments.

## Installation

```toml
[dependencies]
zeph-acp = "*"
```

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
| `terminal` | `AcpShellExecutor` — shell executor backed by IDE-proxied ACP terminal; configurable command timeout with `kill_terminal_command` on expiry; deferred `terminal/release` ensures terminal remains alive until IDE receives `ToolCallContent::Terminal` |
| `permission` | `AcpPermissionGate` — forwards tool permission requests to the IDE for user approval; persists "always allow/deny" decisions to TOML file |
| `mcp_bridge` | `acp_mcp_servers_to_entries` — converts ACP-advertised MCP servers (Stdio, Http, Sse) into `McpServerEntry` configs |
| `error` | `AcpError` typed error enum |

**Re-exports:** `AcpContext`, `AgentSpawner`, `ProviderFactory`, `AcpError`, `AcpFileExecutor`, `AcpPermissionGate`, `AcpShellExecutor`, `AcpServerConfig`, `serve_connection`, `serve_stdio`, `acp_mcp_servers_to_extras`

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

## Tool call lifecycle

`ZephAcpAgent` emits ACP session notifications following the protocol-specified two-step lifecycle:

1. **Before execution** — `SessionUpdate::ToolCall` with `status: InProgress` is sent as soon as tool invocation begins, enabling the IDE to display a running indicator.
2. **After execution** — `SessionUpdate::ToolCallUpdate` with `status: Completed` (or `Failed` on error) carries the output content and optional file locations.

Each tool call is identified by a UUID generated per invocation. The UUID is threaded through `LoopbackEvent::ToolStart` / `LoopbackEvent::ToolOutput` so the update correctly references the original announcement. Both the fenced-block execution path (`handle_tool_result`) and the structured parallel tool-call path emit this full two-step sequence unconditionally — output content always appears inside a tool call block in the IDE regardless of which path handled the tool.

**Terminal release ordering** — when a shell tool call embeds a terminal via `ToolCallContent::Terminal`, the ACP spec requires the terminal to remain alive until the IDE has processed the `tool_call_update` notification. `ZephAcpAgent` defers `terminal/release` until after all notifications for that event are dispatched. The deferred release is triggered from the `prompt()` event loop via `AcpShellExecutor::release_terminal()`, which is retained in `SessionEntry` for exactly this purpose.

> [!NOTE]
> Prior to #1003 the fenced-block path did not generate a UUID or emit `ToolStart`. Prior to #1013 the terminal was released inside `execute_in_terminal` before `tool_call_update` was sent, preventing IDEs from displaying terminal output. Both issues are now resolved.

### Terminal command timeout

`AcpShellExecutor` enforces a configurable wall-clock timeout on every IDE-proxied shell command (default: 120 seconds, controlled via `acp.terminal_timeout_secs`). When the timeout expires:

1. `kill_terminal_command` is called to terminate the running process.
2. Partial output collected so far is returned as an error result.
3. The terminal is released and `AcpError::TerminalTimeout` is propagated to the agent loop.

```toml
[acp]
terminal_timeout_secs = 120   # set to 0 to disable (wait indefinitely)
```

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
| `acp.terminal_timeout_secs` | u64 | `120` | `ZEPH_ACP_TERMINAL_TIMEOUT_SECS` |
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

## Feature flags

| Feature | Status | Description |
|---------|--------|-------------|
| `acp-http` | stable | Enables the HTTP+SSE and WebSocket transports (axum-based). Required for `post_handler`, `get_handler`, `ws_upgrade_handler`, and `router`. |
| `unstable-session-list` | unstable | Enables the `list_sessions` ACP method. See below. |
| `unstable-session-fork` | unstable | Enables the `fork_session` ACP method. See below. |
| `unstable-session-resume` | unstable | Enables the `resume_session` ACP method. See below. |
| `unstable-session-usage` | unstable | Enables `UsageUpdate` events — token counts (input, output, cache) sent to the IDE after each turn. See below. |
| `unstable-session-model` | unstable | Enables `SetSessionModel` — IDE-driven model switching via a native picker without `session/configure`. See below. |
| `unstable-session-info-update` | unstable | Enables `SessionInfoUpdate` — agent-generated session title emitted to the IDE after the first turn. See below. |

> [!WARNING]
> All `unstable-*` features have wire protocol that is not yet finalized. Expect breaking changes before these features graduate to stable.

To opt in, add the desired features in your `Cargo.toml`:

```toml
[dependencies]
zeph-acp = { version = "*", features = [
    "unstable-session-list",
    "unstable-session-fork",
    "unstable-session-resume",
    "unstable-session-usage",
    "unstable-session-model",
    "unstable-session-info-update",
] }
```

All flags are independent and can be combined freely.

### `unstable-session-list`

Enables the `list_sessions` method on `ZephAcpAgent`. Returns a snapshot of all active in-memory sessions as `SessionInfo` records (session ID, working directory, last-updated timestamp). Supports an optional `cwd` filter — when provided, only sessions whose working directory matches the given path are returned.

When this feature is active, `initialize` advertises `SessionListCapabilities` in the `session` capabilities block, signalling to the IDE that the server supports session enumeration.

### `unstable-session-fork`

Enables the `fork_session` method. Branches an existing conversation into a new session by:

1. Verifying the source session exists in memory or SQLite.
2. Assigning a fresh UUID as the new session ID.
3. Asynchronously copying all persisted events from the source into the new session via `import_acp_events`.
4. Spawning a new agent loop for the forked session with the supplied `cwd`.

The forked session is immediately available for new turns. The event copy is fire-and-forget — if the store write fails, a warning is logged but the session is still created. Model config options are forwarded to the fork response when `available_models` is non-empty.

### `unstable-session-resume`

Enables the `resume_session` method. Restores a persisted session to an active in-memory state without replaying history as `session/update` events:

- If the session is already active in memory, returns success immediately (no-op).
- Otherwise, verifies existence in SQLite and hydrates a new `SessionEntry`, making the session available for new turns with lower latency than the default `load_session` replay path.
- Requires a configured `SqliteStore`; returns an error if no store is present.

### `unstable-session-usage`

Enables `UsageUpdate` session events. After each agent turn `ZephAcpAgent` emits a `SessionUpdate::UsageUpdate` carrying token counts for the turn:

- `input_tokens` — tokens consumed from the prompt.
- `output_tokens` — tokens produced by the model.
- `cache_read_tokens` / `cache_write_tokens` — cache activity when the provider supports prompt caching.

The IDE can use this data to display running cost estimates or token budgets without polling a separate endpoint.

### `unstable-session-model`

Enables `SetSessionModel` handling. When the IDE sends a `set_session_model` request (e.g., from a native model-picker dropdown), `ZephAcpAgent`:

1. Resolves the requested `"provider:model"` key via `ProviderFactory`.
2. Stores the resolved provider in the session-scoped `provider_override`.
3. Returns a confirmation to the IDE so the picker reflects the active selection.

This avoids the need to wrap model selection in a `session/configure` call and maps directly to the Zed AI model picker interaction.

### `unstable-session-info-update`

Enables `SessionInfoUpdate` events. After the first completed turn in a new session, `ZephAcpAgent` emits a `SessionUpdate::SessionInfoUpdate` containing a short, LLM-generated title derived from the opening message. The IDE can use this title to label the session in its sidebar or tab bar.

## Plan updates

During orchestrator runs `ZephAcpAgent` emits `SessionUpdate::Plan` events as the agent formulates its execution plan. The IDE receives these events in real time and can render a collapsible plan view alongside the conversation, giving users visibility into multi-step reasoning before tool calls begin.

## Slash command dispatch

`ZephAcpAgent` sends an `AvailableCommandsUpdate` to the IDE during session initialization listing the built-in slash commands:

| Command | Description |
|---------|-------------|
| `/help` | Show available slash commands |
| `/model` | Switch the active model |
| `/mode` | Change the session mode (`ask` / `code` / `architect`) |
| `/clear` | Clear the current conversation history |
| `/compact` | Trigger a manual context compaction |

User input that begins with `/` is matched against this list and dispatched to the corresponding handler before the message reaches the agent loop.

## LSP diagnostics injection

When a prompt contains an `@diagnostics` mention (inserted by Zed's mention system), `ZephAcpAgent` resolves the current LSP diagnostics for the active file or workspace and injects them as structured context into the prompt before inference. This gives the model accurate, up-to-date error and warning information without requiring the user to copy-paste diagnostic output manually.

## License

MIT
