# Daemon Mode

Run Zeph as a headless background agent with an A2A endpoint, then connect a TUI client for real-time interaction.

## Prerequisites

Daemon mode requires both `daemon` and `a2a` feature flags:

```bash
cargo build --release --features daemon,a2a
```

To connect a TUI client, build with `tui` and `a2a`:

```bash
cargo build --release --features tui,a2a
```

## Configuration

Run the interactive wizard to configure daemon settings:

```bash
zeph init
```

The wizard generates the `[daemon]` and `[a2a]` sections in `config.toml`:

```toml
[daemon]
enabled = true
pid_file = "~/.zeph/zeph.pid"
health_interval_secs = 30
max_restart_backoff_secs = 60

[a2a]
enabled = true
host = "0.0.0.0"
port = 3000
auth_token = "your-secret-token"
```

## Starting the Daemon

```bash
zeph --daemon
```

The daemon:

1. Writes a PID file for instance detection
2. Bootstraps a full agent (provider, memory, skills, tools, MCP)
3. Starts the A2A JSON-RPC server on the configured host/port
4. Runs under `DaemonSupervisor` with health monitoring
5. Handles Ctrl-C for graceful shutdown (removes PID file)

The agent uses a `LoopbackChannel` internally, which auto-approves confirmation prompts and bridges I/O between the A2A task processor and the agent loop via tokio mpsc channels.

## Connecting the TUI

From any machine that can reach the daemon:

```bash
zeph --connect http://localhost:3000
```

The TUI connects to the remote daemon via A2A SSE streaming. Tokens are rendered in real-time as they arrive from the agent. All standard TUI features (markdown rendering, command palette, file picker) work in connected mode.

### Authentication

If the daemon has `auth_token` configured, set `ZEPH_A2A_AUTH_TOKEN` before connecting:

```bash
ZEPH_A2A_AUTH_TOKEN=your-secret-token zeph --connect http://localhost:3000
```

## Architecture

```text
+-------------------+       A2A SSE        +-------------------+
|   TUI Client      | <------------------> |   Daemon          |
|   (--connect)     |     JSON-RPC 2.0     |   (--daemon)      |
+-------------------+                      +-------------------+
                                           | LoopbackChannel   |
                                           |   input_tx/rx     |
                                           |   output_tx/rx    |
                                           +-------------------+
                                           | Agent Loop        |
                                           | LLM + Tools + MCP |
                                           +-------------------+
```

The `LoopbackChannel` implements the `Channel` trait with two linked mpsc pairs:

- **input**: the A2A task processor sends user messages to the agent
- **output**: the agent emits `LoopbackEvent` variants (`Chunk`, `Flush`, `FullMessage`, `Status`, `ToolOutput`) back to the processor

The `TaskProcessor` translates `LoopbackEvent` into `ProcessorEvent::ArtifactChunk` for SSE streaming to connected clients.

## Daemon Management via Command Palette

When using TUI in connected mode, additional commands are available in the command palette (`Ctrl+P`):

| Command | Description |
|---------|-------------|
| `daemon:connect` | Connect to remote daemon |
| `daemon:disconnect` | Disconnect from daemon |
| `daemon:status` | Show connection status |
