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
  "capabilities": ["prompt", "cancel", "load_session"],
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
| `available_models` | `[]` | Models advertised to the IDE for runtime switching (format: `provider:model`) |
| `transport` | `"stdio"` | Transport mode: `"stdio"`, `"http"`, or `"both"` |
| `http_bind` | `"127.0.0.1:8080"` | Bind address for the HTTP transport |

You can also configure ACP via the interactive wizard:

```bash
zeph init
```

The wizard will ask whether to enable ACP and which agent name/version to use.

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

## Security

- **Session IDs** — validated against `[a-zA-Z0-9_-]`, max 128 characters
- **Path traversal** — `_agent/working_dir/update` rejects paths containing `..`
- **Import cap** — session import limited to 10,000 events per request
- **Tool permissions** — optionally persisted to `permission_file` so users don't re-approve tools on every session

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
