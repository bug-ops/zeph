---
name: mcp-generate
description: Generate MCP server configuration for Zeph. Use when the user asks about adding MCP servers, configuring MCP tools, or integrating external tool providers.
---
# MCP Server Configuration

## Adding an MCP Server

Add server entries to `config/default.toml`:

```toml
[[mcp.servers]]
id = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
timeout = 30

[mcp.servers.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_PERSONAL_ACCESS_TOKEN}"
```

## Configuration Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | yes | — | Unique server identifier |
| `command` | yes | — | Executable to spawn |
| `args` | no | `[]` | Command arguments |
| `env` | no | `{}` | Environment variables |
| `timeout` | no | `30` | Tool call timeout in seconds |

## Enabling MCP Feature

Build with MCP support:
```bash
cargo build --features mcp
```

Or add to default features in `Cargo.toml`:
```toml
[features]
default = ["a2a", "mcp"]
```

## Common MCP Servers

### Filesystem
```toml
[[mcp.servers]]
id = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed/dir"]
```

### GitHub
```toml
[[mcp.servers]]
id = "github"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[mcp.servers.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_PERSONAL_ACCESS_TOKEN}"
```

## How It Works

1. On startup, Zeph connects to each configured MCP server via stdio
2. Tools are discovered via the MCP `tools/list` protocol
3. Tool descriptions are embedded into Qdrant for semantic matching
4. When a user query matches an MCP tool, it appears in the system prompt
5. The agent invokes tools via ` ```mcp ` fenced blocks with JSON payloads
