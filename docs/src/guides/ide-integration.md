# IDE Integration

Zeph can act as a first-class coding assistant inside Zed and VS Code through the [Agent Client Protocol](https://agentclientprotocol.com). The editor spawns Zeph as a stdio subprocess and communicates over JSON-RPC; no daemon or network port is required.

For a full reference on ACP capabilities, transports, and configuration options, see [ACP (Agent Client Protocol)](../advanced/acp.md).

## Prerequisites

- **Zeph** installed and configured (`zeph init` completed, at least one LLM provider active).
- **ACP feature** enabled in the binary (included in the default release build).
- **Zed 1.0+** with the official ACP extension, **or** VS Code with the ACP extension.

Verify that ACP is available in your binary:

```bash
zeph --acp-manifest
```

Expected output:

```json
{
  "name": "zeph",
  "version": "0.14.3",
  "transport": "stdio",
  "command": ["zeph", "--acp"],
  "capabilities": ["prompt", "cancel", "load_session", "set_session_mode", "config_options", "ext_methods"],
  "description": "Zeph AI Agent",
  "readiness": {
    "notification": { "method": "zeph/ready" },
    "http": { "health_endpoint": "/health", "statuses": [200, 503] }
  }
}
```

If the command is not found, ensure the Zeph binary directory is on your `PATH` (see [Troubleshooting](#troubleshooting)).

## Enabling ACP in config.toml

Add the following section to your `config.toml` if it is not already present:

```toml
[acp]
enabled = true
# Optional: restrict which skills are exposed over ACP
# allowed_skills = ["code-review", "refactor"]
```

The `enabled` flag activates the ACP command-line flags (`--acp`, `--acp-http`, `--acp-manifest`). No network configuration is needed for the stdio transport used by IDE extensions.

## Launching Zeph as an ACP stdio server

The editor extension manages the process lifecycle. When the user opens the assistant panel, the extension runs:

```bash
zeph --acp
```

Zeph reads JSON-RPC messages from stdin and writes responses to stdout. You can test the connection manually:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"acp/manifest"}' | zeph --acp
```

## Readiness checks for extensions

IDE integrations can stop guessing when Zeph has finished warming up:

- **stdio transport:** wait for the first `zeph/ready` notification before sending the first interactive request. Example payload:

```json
{"jsonrpc":"2.0","method":"zeph/ready","params":{"version":"0.14.3","pid":12345,"log_file":"/path/to/zeph.log"}}
```

- **HTTP transport:** poll `GET /health` until it returns `200 OK`.

```bash
curl -fsS http://127.0.0.1:8080/health
```

If startup is still in progress, Zeph returns `503 Service Unavailable` with `{"status":"starting", ...}`. Once ready, the response becomes `{"status":"ok","version":"...","uptime_secs":...}`.

## IDE setup

### Zed

1. Open **Settings** (`Cmd+,` on macOS, `Ctrl+,` on Linux).
2. Add the agent configuration under `"agent"`:

```json
{
  "agent": {
    "profiles": {
      "zeph": {
        "provider": "acp",
        "binary": "zeph",
        "args": ["--acp"]
      }
    },
    "default_profile": "zeph"
  }
}
```

3. Reload the window. The **Zeph** entry appears in the assistant model selector.

### VS Code

Install the ACP extension from the marketplace, then add to `settings.json`:

```json
{
  "acp.agents": [
    {
      "name": "Zeph",
      "command": "zeph",
      "args": ["--acp"]
    }
  ]
}
```

## Subagent visibility features

When Zeph orchestrates subagents internally, the IDE extension surfaces the execution hierarchy directly in the chat view.

### Subagent nesting

Every `session_update` message carries a `_meta.claudeCode.parentToolUseId` field that identifies which parent tool call spawned the update. ACP-aware extensions (Zed, VS Code) use this field to nest subagent output under the originating tool call card in the chat panel, giving a clear visual tree of agent activity.

### Live terminal streaming

`AcpShellExecutor` streams bash output in real time. Each chunk is delivered as a `session_update` with a `_meta.terminal_output` payload. The extension appends these chunks to the tool call card as they arrive, so you see command output line by line without waiting for the process to finish.

### Agent following

When Zeph reads or writes a file, the `ToolCall.location` field carries the `filePath` of the target. The IDE extension receives this location and moves the editor cursor to the active file, keeping the viewport synchronized with what the agent is working on.

## Troubleshooting

**`zeph: command not found`**

The binary is not on your `PATH`. Add the installation directory:

```bash
# Cargo install default
export PATH="$HOME/.cargo/bin:$PATH"
```

Add the export to your shell profile (`~/.zshrc`, `~/.bashrc`) to make it permanent.

**`--acp` flag not recognized**

Your binary was built without the ACP feature. Rebuild with:

```bash
cargo install zeph --features acp
```

Or use the official release binary, which includes ACP by default.

**Extension connects but returns no responses**

Run `zeph --acp-manifest` in the terminal to confirm the process starts and outputs valid JSON. If it hangs or errors, check your `config.toml` for syntax errors and verify that `[acp] enabled = true` is present.

**Verifying the manifest**

```bash
zeph --acp-manifest
```

The `capabilities` array must include `"prompt"` for basic chat to work. If any capability is missing, ensure you are running the latest release.
