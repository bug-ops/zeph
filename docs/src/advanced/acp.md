# ACP (Agent Client Protocol)

Zeph implements the [Agent Client Protocol](https://agentclientprotocol.com) — an open standard that lets AI agents communicate with editors and IDEs. With ACP, Zeph becomes a coding assistant inside your editor: it reads files, runs shell commands, and streams responses — all through a standardized protocol.

## Prerequisites

- Zeph installed and configured (`zeph init` completed, at least one LLM provider set up)
- The `acp` feature enabled (included in the default release binary)

Verify that ACP is available:

```bash
zeph --acp-manifest
```

Expected output:

```json
{
  "name": "zeph",
  "version": "0.12.1",
  "transport": "stdio",
  "command": ["zeph", "--acp"],
  "capabilities": ["prompt", "cancel", "load_session", "set_session_mode", "config_options", "ext_methods"],
  "description": "Zeph AI Agent"
}
```

## Transport modes

Zeph supports three ACP transports:

| Transport | Flag | Use case |
|-----------|------|----------|
| **stdio** | `--acp` | Editor spawns Zeph as a child process (recommended for local use) |
| **HTTP+SSE** | `--acp-http` | Shared or remote server, multiple clients |
| **WebSocket** | `--acp-http` | Same server, alternative protocol for WS-native clients |

The stdio transport is the simplest — the editor manages the process lifecycle, no ports or network configuration needed.

## IDE setup

### Zed

1. Open **Settings** (`Cmd+,` on macOS, `Ctrl+,` on Linux).

2. Add the agent configuration:

```json
{
  "agent": {
    "profiles": {
      "zeph": {
        "provider": "acp",
        "binary": {
          "path": "zeph",
          "args": ["--acp"]
        }
      }
    },
    "default_profile": "zeph"
  }
}
```

3. Open the assistant panel (`Cmd+Shift+A`) — Zed will spawn `zeph --acp` and connect over stdio.

> **Tip:** If Zeph is not in your `PATH`, use the full binary path (e.g., `"path": "/usr/local/bin/zeph"`).

### Helix

Helix does not have native ACP support yet. Use the HTTP transport with an ACP-compatible proxy or plugin:

1. Start Zeph as an HTTP server:

```bash
zeph --acp-http --acp-http-bind 127.0.0.1:8080
```

2. Configure a language server or external tool in `~/.config/helix/languages.toml` that communicates with the ACP HTTP endpoint at `http://127.0.0.1:8080`.

### VS Code

1. Install an ACP client extension (e.g., [ACP Client](https://marketplace.visualstudio.com/items?itemName=anthropic.acp-client) or any extension implementing the ACP spec).

2. Configure the extension to use Zeph:

```json
{
  "acp.command": ["zeph", "--acp"],
  "acp.transport": "stdio"
}
```

Alternatively, for a shared server setup:

```bash
zeph --acp-http --acp-http-bind 127.0.0.1:8080
```

Then point the extension to `http://127.0.0.1:8080`.

### Any ACP client

For editors or tools implementing the ACP spec:

- **stdio** — spawn `zeph --acp` as a subprocess, communicate over stdin/stdout
- **HTTP+SSE** — start `zeph --acp-http` and connect to the bind address
- **WebSocket** — connect to the `/ws` endpoint on the same HTTP server

## Configuration

ACP settings live in `config.toml` under the `[acp]` section:

```toml
[acp]
enabled = true
agent_name = "zeph"
agent_version = "0.12.1"
max_sessions = 4
session_idle_timeout_secs = 1800
# permission_file = "~/.config/zeph/acp-permissions.toml"
# available_models = ["claude:claude-sonnet-4-5", "ollama:llama3"]
# transport = "stdio"             # "stdio", "http", or "both"
# http_bind = "127.0.0.1:8080"
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `false` | Enable ACP server |
| `agent_name` | `"zeph"` | Agent name advertised to the IDE |
| `agent_version` | package version | Agent version advertised to the IDE |
| `max_sessions` | `4` | Maximum concurrent sessions |
| `session_idle_timeout_secs` | `1800` | Idle sessions are reaped after this timeout (seconds) |
| `permission_file` | none | Path to persisted tool permission decisions |
| `terminal_timeout_secs` | `120` | Wall-clock timeout for IDE-proxied shell commands; `0` disables the timeout |
| `available_models` | `[]` | Models advertised to the IDE for runtime switching (format: `provider:model`) |
| `transport` | `"stdio"` | Transport mode: `"stdio"`, `"http"`, or `"both"` |
| `http_bind` | `"127.0.0.1:8080"` | Bind address for the HTTP transport |

You can also configure ACP via the interactive wizard:

```bash
zeph init
```

The wizard will ask whether to enable ACP and which agent name/version to use.

## Tool call lifecycle

Zeph follows the ACP protocol specification for tool call notifications. Each tool invocation produces two session updates visible to the IDE:

1. **`SessionUpdate::ToolCall` with `status: InProgress`** — emitted immediately before the tool executes. The IDE can display a running spinner or pending indicator.
2. **`SessionUpdate::ToolCallUpdate` with `status: Completed` or `Failed`** — emitted after execution completes, carrying the output content and optional file locations for source navigation.

Both updates share the same UUID so the IDE can correlate them. Tools that finish successfully use `Completed`; tools that return an error (non-zero exit code, exception, or explicit failure) use `Failed`.

### Terminal command timeout

Shell commands run via the IDE terminal (`bash` tool) are subject to a configurable wall-clock timeout:

```toml
[acp]
terminal_timeout_secs = 120   # default; set to 0 to wait indefinitely
```

When the timeout expires:

1. `kill_terminal_command` is called to terminate the running process.
2. Any partial output collected up to that point is returned as an error result.
3. The terminal session is released and the agent receives `AcpError::TerminalTimeout`.

> **Tip:** Increase `terminal_timeout_secs` for long-running build or test commands that legitimately take more than two minutes.

> **Caution:** Setting `terminal_timeout_secs = 0` disables the timeout entirely. Commands that hang indefinitely will stall the agent turn until cancelled.

## MCP server transports

When an IDE passes MCP server definitions to Zeph via the ACP `McpServer` field, Zeph's `mcp_bridge` maps each server to a `zeph-mcp` `ServerEntry`. Three transport types are supported:

| ACP transport | `zeph-mcp` mapping | Notes |
|---------------|--------------------|-------|
| `Stdio` | `McpTransport::Stdio` | IDE spawns the MCP server binary; environment variables are forwarded as-is |
| `Http` | `McpTransport::Http` | Connects to a Streamable HTTP MCP endpoint |
| `Sse` | `McpTransport::Http` | Legacy SSE transport; mapped to Streamable HTTP (rmcp's `StreamableHttpClientTransport` is backward-compatible) |

Unknown transport variants are skipped with a `WARN` log line and do not cause the session to fail.

No configuration is needed beyond what the IDE sends. Zeph reads the server list from each `new_session` request and registers the servers with the shared `McpManager` for the duration of the session.

## Model switching

If you configure `available_models`, the IDE can switch between LLM providers at runtime:

```toml
[acp]
available_models = [
  "claude:claude-sonnet-4-5",
  "openai:gpt-4o",
  "ollama:qwen3:14b",
]
```

The IDE presents these as selectable options. Zeph routes each prompt to the chosen provider without restarting the server.

## Advertised capabilities

During `initialize`, Zeph reports two capability flags in `AgentCapabilities.meta`:

| Key | Value | Meaning |
|-----|-------|---------|
| `config_options` | `true` | Zeph supports runtime model switching via `set_session_config_option` |
| `ext_methods` | `true` | Zeph accepts custom extension methods via `ext_method` |

IDEs use these flags to decide which optional protocol features to activate. A client that sees `config_options: true` may render a model picker in the UI; one that sees `ext_methods: true` may call custom `_`-prefixed methods without first probing for support.

## Session modes

Zeph supports ACP session modes, allowing the IDE to switch the agent's behavior within a session:

| Mode | Description |
|------|-------------|
| `code` | Default mode — full tool access, code generation, file operations |
| `architect` | Design-focused — emphasizes planning and architecture over direct edits |
| `ask` | Read-only — answers questions without making changes |

The active mode is advertised in the `new_session` and `load_session` responses via the `modes` field. The IDE can switch modes at any time using `set_session_mode`:

```jsonc
// Request
{ "method": "set_session_mode", "params": { "session_id": "...", "mode_id": "architect" } }

// Zeph emits a CurrentModeUpdate notification after a successful switch
{ "method": "notifications/session", "params": { "session_id": "...", "update": { "type": "current_mode_update", "mode_id": "architect" } } }
```

> **Note:** Mode switching takes effect on the next prompt. An in-flight prompt continues in the mode it started with.

## Extension notifications

Zeph implements the `ext_notification` handler. The IDE sends one-way notifications using this method without waiting for a response. Zeph accepts any method name and returns `Ok(())`. This is useful for IDE-side telemetry or state hints that do not require agent action.

## Custom extension methods

Zeph extends the base ACP protocol with custom methods via `ext_method`. All use a leading underscore to avoid collisions with the standard spec.

| Method | Description |
|--------|-------------|
| `_session/list` | List all sessions (in-memory + persisted) |
| `_session/get` | Get session details and event history |
| `_session/delete` | Delete a session |
| `_session/export` | Export session events for backup |
| `_session/import` | Import events into a new session |
| `_agent/tools` | List available tools for a session |
| `_agent/working_dir/update` | Change the working directory for a session |

These methods are useful for building custom IDE integrations or debugging session state.

## WebSocket transport

When running in HTTP mode (`--acp-http`), Zeph exposes a WebSocket endpoint at `/acp/ws` alongside the SSE endpoint at `/acp`. The server enforces the following constraints:

**Session concurrency** — slot reservation is atomic (compare-and-swap on an `AtomicUsize` counter), so `max_sessions` is a hard cap regardless of how many connections race to upgrade simultaneously. No TOCTOU window exists between the check and the increment.

**Keepalive** — the server sends a WebSocket ping every 30 seconds. If a pong is not received within 90 seconds of the ping, the connection is closed.

**Binary frames** — only text frames carry ACP JSON messages. If a client sends a binary frame the server responds with WebSocket close code `1003` (Unsupported Data) as required by RFC 6455.

**Close frame delivery** — on graceful shutdown the write task is given a 1-second drain window to deliver the close frame before the TCP connection is dropped. This satisfies the RFC 6455 §7.1.1 requirement that both sides exchange close frames.

**Max message size** — incoming WebSocket messages are limited to 1 MiB (1,048,576 bytes). Messages exceeding this limit cause an immediate close with code `1009` (Message Too Big).

## Bearer authentication

The ACP HTTP server (both `/acp` SSE and `/acp/ws` WebSocket endpoints) supports optional bearer token authentication.

```toml
[acp]
auth_bearer_token = "your-secret-token"
```

The token can also be supplied via environment variable or CLI argument:

| Method | Value |
|--------|-------|
| `config.toml` | `acp.auth_bearer_token = "token"` |
| Environment | `ZEPH_ACP_AUTH_TOKEN=token` |
| CLI | `--acp-auth-token TOKEN` |

When a token is configured, every request to `/acp` and `/acp/ws` must include an `Authorization: Bearer <token>` header. Requests without a valid token receive `401 Unauthorized`.

The agent discovery endpoint (`GET /.well-known/acp.json`) is always exempt from authentication — clients need to discover the agent manifest before they can authenticate.

When no token is configured the server runs in open mode. This is acceptable for local loopback use where network access is restricted.

> **Warning:** Always set `auth_bearer_token` (or `ZEPH_ACP_AUTH_TOKEN`) when binding to a non-loopback address or exposing the ACP port over a network. Running without a token on a publicly reachable interface allows any client to connect and issue commands.

## Agent discovery

Zeph publishes an ACP agent manifest at a well-known URL:

```
GET /.well-known/acp.json
```

Example response (with bearer auth configured):

```json
{
  "name": "zeph",
  "version": "0.12.1",
  "protocol": "acp",
  "protocol_version": "0.9",
  "transports": {
    "http_sse": { "url": "/acp" },
    "websocket": { "url": "/acp/ws" }
  },
  "authentication": { "type": "bearer" }
}
```

When `auth_bearer_token` is not set, the `authentication` field is `null`:

```json
{
  "name": "zeph",
  "version": "0.12.1",
  "protocol": "acp",
  "protocol_version": "0.9",
  "transports": {
    "http_sse": { "url": "/acp" },
    "websocket": { "url": "/acp/ws" }
  },
  "authentication": null
}
```

Discovery is enabled by default and can be disabled if needed:

```toml
[acp]
discovery_enabled = true   # set to false to suppress the manifest endpoint
```

| Method | Value |
|--------|-------|
| `config.toml` | `acp.discovery_enabled = false` |
| Environment | `ZEPH_ACP_DISCOVERY_ENABLED=false` |

The discovery endpoint is always unauthenticated by design. ACP clients must be able to read the manifest before they know which authentication scheme to use.

## Unstable session features

Three session management capabilities are available behind dedicated feature flags. They are part of the ACP protocol's unstable surface — their wire format and behavior may change before stabilization.

Each feature adds a standard ACP protocol method to the agent's advertised `session_capabilities`. The IDE discovers these capabilities in the `initialize` response and can invoke the corresponding methods.

| Feature flag | ACP method | Description |
|--------------|------------|-------------|
| `unstable-session-list` | `list_sessions` | Enumerate in-memory sessions. Accepts an optional `cwd` filter; returns session ID, working directory, and last-updated timestamp for each matching session. |
| `unstable-session-fork` | `fork_session` | Clone an existing session's persisted event history into a new session and immediately spawn a fresh agent loop from that checkpoint. The source session continues unaffected. |
| `unstable-session-resume` | `resume_session` | Reattach to a session that exists in SQLite but is not currently active in memory. Spawns an agent loop without replaying historical events. Useful for continuing a session after a Zeph restart. |

The composite flag `acp-unstable` (root crate) enables all three at once.

> **Note:** These features are gated on the `zeph-acp` crate. The `unstable-session-list` flag also enables the corresponding feature in the `agent-client-protocol` dependency (`unstable_session_list`), and likewise for the other two. Stability and wire format are not guaranteed across minor versions until promoted to stable.

### Enabling the features

Enable individual flags:

```bash
cargo build --features unstable-session-list
cargo build --features unstable-session-fork
cargo build --features unstable-session-resume
```

Enable all three at once with the composite flag:

```bash
cargo build --features acp-unstable
```

When embedding `zeph-acp` as a library dependency:

```toml
[dependencies]
zeph-acp = { version = "...", features = ["unstable-session-list", "unstable-session-fork", "unstable-session-resume"] }
```

### list_sessions

When `unstable-session-list` is active, the agent advertises `list` in `session_capabilities`. The IDE can call `list_sessions` to enumerate all sessions currently live in memory.

Request parameters:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cwd` | path | no | Filter — only return sessions whose working directory matches this path |

Response fields per session entry:

| Field | Description |
|-------|-------------|
| `session_id` | Unique session identifier |
| `cwd` | Session working directory |
| `updated_at` | RFC 3339 timestamp of session creation or last update |

Sessions that are in memory but have no working directory set are included with an empty path. Persisted-only sessions (in SQLite but not active) are not enumerated by `list_sessions`.

### fork_session

When `unstable-session-fork` is active, the agent advertises `fork` in `session_capabilities`. The IDE can call `fork_session` to branch an existing session.

The fork operation:

1. Looks up the source session — in memory or in the SQLite store.
2. Copies all persisted events from the source into a new session record (async, does not block the response).
3. Spawns a fresh agent loop for the new session starting from the forked state.
4. Returns the new session ID and any available model config options.

The source session remains active and unchanged. Both sessions are independent after the fork.

```jsonc
// Request
{ "method": "fork_session", "params": { "session_id": "<source-id>", "cwd": "/workspace" } }

// Response
{ "session_id": "<new-forked-id>", "config_options": [...] }
```

> **Note:** The event copy is performed asynchronously. There is a brief window where the new session's agent loop starts before all events are written to SQLite.

### resume_session

When `unstable-session-resume` is active, the agent advertises `resume` in `session_capabilities`. The IDE can call `resume_session` to reattach to a previously persisted session.

The resume operation:

1. Checks whether the session is already active in memory — if so, returns immediately (no-op).
2. Verifies the session exists in SQLite.
3. Spawns a fresh agent loop for the session **without** replaying historical events through the loop. The session's stored event log is preserved in SQLite and accessible via `_session/get`.

```jsonc
// Request
{ "method": "resume_session", "params": { "session_id": "<persisted-id>", "cwd": "/workspace" } }

// Response (empty on success)
{}
```

Use `resume_session` to continue a session after a Zeph process restart, or to open a background session for inspection without disturbing its history.

## Security

- **Session IDs** — validated against `[a-zA-Z0-9_-]`, max 128 characters
- **Path traversal** — `_agent/working_dir/update` rejects paths containing `..`
- **Import cap** — session import limited to 10,000 events per request
- **Tool permissions** — optionally persisted to `permission_file` so users don't re-approve tools on every session
- **Bearer auth** — see [Bearer authentication](#bearer-authentication) above
- **Atomic slot reservation** — `max_sessions` enforced without TOCTOU race; see [WebSocket transport](#websocket-transport) above

## Troubleshooting

**Zeph binary not found by the editor**

Ensure `zeph` is in your shell `PATH`. Test with:

```bash
which zeph
zeph --acp-manifest
```

If using a custom install path, specify the full path in the editor config.

**Connection drops or no response**

Check that your `config.toml` has a valid LLM provider configured. Zeph needs at least one working provider to process prompts. Run `zeph` in CLI mode first to verify your setup works.

**HTTP transport: "address already in use"**

Another process is using the bind port. Change the port:

```bash
zeph --acp-http --acp-http-bind 127.0.0.1:9090
```

**Sessions accumulate in memory**

Idle sessions are automatically reaped after `session_idle_timeout_secs` (default: 30 minutes). Lower this value if memory is a concern.
