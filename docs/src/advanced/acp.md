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
terminal_timeout_secs = 120
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
| `terminal_timeout_secs` | `120` | Terminal command execution timeout; `kill_terminal_command` is sent on expiry |
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

## Session modes

Each ACP session operates in a mode that signals intent to the agent. Modes are set by the IDE using `set_session_mode` and can be changed at any time during a session.

| Mode | Description |
|------|-------------|
| `ask` | Question-answering; agent does not modify files |
| `code` | Active coding assistance; file edits and shell commands are permitted (default) |
| `architect` | High-level design and planning; agent focuses on reasoning over implementation |

When the mode changes, Zeph emits a `current_mode_update` notification so the IDE can update its UI immediately.

## Capabilities

Zeph advertises the following capabilities in the `initialize` response:

```json
{
  "agent_capabilities": {
    "load_session": true,
    "session_capabilities": {
      "list": {},
      "fork": {},
      "resume": {}
    }
  }
}
```

`session_capabilities` is always present regardless of whether the `unstable_session_*` features are compiled in. The actual `list_sessions`, `fork_session`, and `resume_session` handlers are available when the corresponding features are enabled (all three are on by default — see [Feature Flags](../reference/feature-flags.md#acp-session-management-unstable)).

## Session management

### list_sessions

`list_sessions` returns all in-memory sessions with their working directory and last-active timestamp.

```json
// Request
{ "method": "list_sessions", "params": {} }

// Response
{
  "sessions": [
    {
      "session_id": "550e8400-e29b-41d4-a716-446655440000",
      "working_dir": "/home/user/project",
      "updated_at": "42"
    }
  ]
}
```

### fork_session

`fork_session` creates a new session that starts with a copy of the source session's persisted event history. The forked session is independent — changes to either session do not affect the other.

```json
// Request
{
  "method": "fork_session",
  "params": { "session_id": "550e8400-e29b-41d4-a716-446655440000" }
}

// Response
{
  "session_id": "661f9511-f3ac-52e5-b827-557766551111",
  "modes": { "current": "code", "available": ["ask", "code", "architect"] }
}
```

Event history is copied asynchronously from SQLite. If no store is configured, the fork starts with an empty history.

### resume_session

`resume_session` restores a previously terminated session from SQLite persistence without replaying its event history into the agent loop. Use this to reconnect to a session after a process restart.

```json
// Request
{
  "method": "resume_session",
  "params": { "session_id": "550e8400-e29b-41d4-a716-446655440000" }
}

// Response: {}
```

If the session is already in memory, `resume_session` returns immediately without creating a duplicate.

## Tool call lifecycle

Each tool invocation follows a two-step lifecycle:

1. **`InProgress`** — emitted immediately when the agent starts executing a tool
2. **`Completed`** — emitted after the tool returns its output

The IDE can use the `InProgress` update to show a spinner or disable UI input while the tool runs. Zeph emits both updates in order for every tool output within a turn before streaming the next assistant token.

### Terminal tool calls

When a bash tool call is routed through the IDE terminal (rather than Zeph's internal shell executor), Zeph attaches a `ToolCallContent::Terminal` entry to the tool call update. This carries the terminal ID so the IDE can display the output in the correct terminal pane.

The terminal command timeout applies to these calls: if execution exceeds `terminal_timeout_secs` (default: 120 s), Zeph sends `kill_terminal_command` to the IDE and the tool call resolves with a timeout error.

## Extension notifications

`ext_notification` is the fire-and-forget counterpart to `ext_method`. The IDE sends a notification and does not wait for a response. Zeph logs the method name at `DEBUG` level and discards the payload.

```json
{
  "method": "ext_notification",
  "params": {
    "method": "editor/fileSaved",
    "params": { "uri": "file:///home/user/project/src/main.rs" }
  }
}
```

Use `ext_notification` for event telemetry from the IDE (file saves, cursor moves, selection changes) that the agent should be aware of but need not respond to.

## User message echo

After the IDE sends a user prompt, Zeph immediately echoes the text back as a `UserMessageChunk` session notification. This allows the IDE to attribute streaming output correctly and render the full conversation in order even when the agent response begins before the IDE has rendered the original prompt.

## MCP HTTP transport

ACP sessions can connect to MCP servers over HTTP in addition to the default stdio transport. Configure `McpServer::Http` in the MCP section of `config.toml`:

```toml
[[mcp.servers]]
name = "my-tools"
transport = "http"
url = "http://localhost:3000/mcp"
```

Zeph routes the connection through `mcp_bridge`, which maps `McpServer::Http` to `McpTransport::Http` at session startup. No additional flags are required.

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

## Content block support

Zeph handles the following ACP content block types in user messages:

| Block type | Handling |
|------------|----------|
| `Text` | Processed normally |
| `Image` | Supported for JPEG, PNG, GIF, WebP up to 20 MiB (base64-encoded) |
| `Audio` | Not supported — logged as a structured `WARN` and skipped |
| `ResourceLink` | Not supported — logged as a structured `WARN` with the URI and skipped |

Unsupported blocks do not terminate the session. The remaining content in the message is processed normally.

User message text is limited to 1 MiB per prompt. Prompts exceeding this limit are rejected with an `invalid_request` error.

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
| `_agent/mcp/list` | List connected MCP servers for a session |

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

Session management and IDE integration capabilities are available behind dedicated feature flags. They are part of the ACP protocol's unstable surface — their wire format and behavior may change before stabilization.

Each feature adds a standard ACP protocol method or notification to the agent's advertised `session_capabilities`. The IDE discovers these capabilities in the `initialize` response and can invoke the corresponding methods.

| Feature flag | ACP method / notification | Description |
|--------------|---------------------------|-------------|
| `unstable-session-list` | `list_sessions` | Enumerate in-memory sessions. Accepts an optional `cwd` filter; returns session ID, working directory, and last-updated timestamp for each matching session. |
| `unstable-session-fork` | `fork_session` | Clone an existing session's persisted event history into a new session and immediately spawn a fresh agent loop from that checkpoint. The source session continues unaffected. |
| `unstable-session-resume` | `resume_session` | Reattach to a session that exists in SQLite but is not currently active in memory. Spawns an agent loop without replaying historical events. Useful for continuing a session after a Zeph restart. |
| `unstable-session-usage` | `UsageUpdate` in `PromptResponse` | Include token consumption data (input tokens, output tokens, cache read/write tokens) in each prompt response. IDEs use this to display per-turn and cumulative cost estimates. |
| `unstable-session-model` | `set_session_model` | Allow the IDE to switch the active LLM model mid-session via a model picker UI. Zeph emits a `SetSessionModel` notification so the IDE can reflect the change immediately. |
| `unstable-session-info-update` | `SessionInfoUpdate` | Zeph automatically generates a short title for the session after the first exchange and emits a `SessionInfoUpdate` notification. IDEs display this as the conversation title in their session list. |

The composite flag `acp-unstable` (root crate) enables all six at once.

> **Note:** These features are gated on the `zeph-acp` crate. Each flag also enables the corresponding feature in the `agent-client-protocol` dependency. Stability and wire format are not guaranteed across minor versions until promoted to stable.

### Enabling the features

Enable individual flags:

```bash
cargo build --features unstable-session-list
cargo build --features unstable-session-fork
cargo build --features unstable-session-resume
cargo build --features unstable-session-usage
cargo build --features unstable-session-model
cargo build --features unstable-session-info-update
```

Enable all six at once with the composite flag:

```bash
cargo build --features acp-unstable
```

When embedding `zeph-acp` as a library dependency:

```toml
[dependencies]
zeph-acp = { version = "...", features = [
  "unstable-session-list",
  "unstable-session-fork",
  "unstable-session-resume",
  "unstable-session-usage",
  "unstable-session-model",
  "unstable-session-info-update",
] }
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

### usage tracking (unstable-session-usage)

When `unstable-session-usage` is compiled in, each `PromptResponse` carries a `UsageUpdate` block with token counts for the turn:

| Field | Description |
|-------|-------------|
| `input_tokens` | Tokens consumed from the prompt (including injected context) |
| `output_tokens` | Tokens generated by the model |
| `cache_read_tokens` | Prompt cache hits (provider-dependent) |
| `cache_write_tokens` | Tokens written to the prompt cache (provider-dependent) |

IDEs use `UsageUpdate` to render per-turn and cumulative cost estimates alongside each response. Fields that are not supported by the active provider are omitted (not set to zero).

### model picker (unstable-session-model)

When `unstable-session-model` is compiled in, the IDE can request a model change at any point during a session:

```jsonc
// IDE → Zeph
{ "method": "set_session_model", "params": { "session_id": "...", "model": "claude:claude-opus-4-5" } }

// Zeph emits a SetSessionModel notification
{
  "method": "notifications/session",
  "params": {
    "session_id": "...",
    "update": { "type": "set_session_model", "model": "claude:claude-opus-4-5" }
  }
}
```

The model change takes effect on the next prompt. The new model must appear in `available_models` in `config.toml`; requests to switch to an unlisted model are rejected with an `invalid_params` error.

### session title (unstable-session-info-update)

When `unstable-session-info-update` is compiled in, Zeph generates a short session title after the first completed exchange and emits a `SessionInfoUpdate` notification:

```jsonc
{
  "method": "notifications/session",
  "params": {
    "session_id": "...",
    "update": {
      "type": "session_info_update",
      "title": "Refactor auth middleware"
    }
  }
}
```

The title is generated by a lightweight LLM call using the first user message and assistant response as input. It is emitted once per session; subsequent turns do not trigger an update. IDEs display the title in their conversation history or session list.

## Plan updates during orchestration

When Zeph runs an orchestrator turn (multi-step reasoning with sub-agents), it emits `SessionUpdate::Plan` notifications to give the IDE real-time visibility into what the orchestrator intends to do:

```jsonc
{
  "method": "notifications/session",
  "params": {
    "session_id": "...",
    "update": {
      "type": "plan",
      "steps": [
        { "id": "1", "description": "Read src/auth.rs", "status": "pending" },
        { "id": "2", "description": "Identify token validation logic", "status": "pending" },
        { "id": "3", "description": "Propose refactor", "status": "pending" }
      ]
    }
  }
}
```

As steps execute, subsequent `plan` updates carry revised `status` values (`in_progress`, `completed`, `failed`). The IDE can render these as a collapsible plan panel or inline progress indicators.

Plan updates are emitted by the orchestrator automatically — no configuration is required. They are only produced during multi-step turns; single-turn prompts produce no plan notifications.

## Slash commands

Zeph advertises built-in slash commands to the IDE via `AvailableCommandsUpdate`. When the user types `/` in the IDE input, it can display the command list as autocomplete suggestions.

Advertised commands:

| Command | Description |
|---------|-------------|
| `/help` | List all available slash commands |
| `/model` | Show the current model or switch to a different one (`/model claude:claude-opus-4-5`) |
| `/mode` | Show or change the session mode (`/mode architect`) |
| `/clear` | Clear the conversation history for the current session |
| `/compact` | Summarize and compress the conversation history to reduce token usage |

`AvailableCommandsUpdate` is emitted at session start and whenever the command set changes (for example, after a mode switch that enables or disables commands). The IDE receives it as a session notification:

```jsonc
{
  "method": "notifications/session",
  "params": {
    "session_id": "...",
    "update": {
      "type": "available_commands_update",
      "commands": [
        { "name": "/help",    "description": "List all available slash commands" },
        { "name": "/model",   "description": "Show or switch the active LLM model" },
        { "name": "/mode",    "description": "Show or change the session mode" },
        { "name": "/clear",   "description": "Clear conversation history" },
        { "name": "/compact", "description": "Summarize conversation history" }
      ]
    }
  }
}
```

Slash commands are dispatched server-side. The IDE sends the raw text (e.g., `/model ollama:llama3`) as a normal user message; Zeph intercepts it before the LLM call and executes the corresponding handler.

## LSP diagnostics context injection

In Zed and other IDEs that expose LSP diagnostics over ACP, Zeph can automatically inject the current file's diagnostics into the prompt context. To request diagnostics, include `@diagnostics` anywhere in the user message:

```
Why does @diagnostics show an unused variable warning in auth.rs?
```

When Zeph sees `@diagnostics`, it requests the active diagnostics from the IDE via the `get_diagnostics` extension method, formats them as a structured block, and prepends the block to the prompt before sending it to the LLM:

```
[LSP Diagnostics]
src/auth.rs:42:5  warning  unused variable: `token`  [unused_variables]
src/auth.rs:67:1  error    mismatched types: expected `bool`, found `()`  [E0308]
```

If the IDE returns no diagnostics, the `@diagnostics` mention is silently removed and the prompt proceeds without a diagnostics block.

> **Note:** `@diagnostics` requires the IDE to support the `get_diagnostics` extension method. Zed supports it natively. Other editors may need a plugin or updated ACP client. If the IDE does not implement `get_diagnostics`, Zeph logs a `WARN` and continues without injecting the block.

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

**Terminal commands hang**

If a terminal command does not complete, Zeph sends `kill_terminal_command` after `terminal_timeout_secs` (default: 120 s). Reduce this value in `config.toml` if you need faster timeout behavior:

```toml
[acp]
terminal_timeout_secs = 30
```
