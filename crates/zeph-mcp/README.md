# zeph-mcp

MCP client lifecycle, tool discovery, and execution.

## Overview

Implements the Model Context Protocol client for Zeph, managing connections to multiple MCP servers, discovering their tools at startup, and routing tool calls through a unified executor. Built on rmcp 0.15.

## Key Modules

- **client** — low-level MCP transport and session handling
- **manager** — `McpManager`, `McpTransport`, `ServerEntry` for multi-server lifecycle; command allowlist validation (npx, uvx, node, python3, docker, etc.), env var blocklist (LD_PRELOAD, DYLD_*, NODE_OPTIONS, etc.), and path separator rejection
- **executor** — `McpToolExecutor` bridging MCP tools into the `ToolExecutor` trait
- **registry** — `McpToolRegistry` for tool lookup and optional Qdrant-backed search
- **tool** — `McpTool` wrapper with schema and metadata
- **prompt** — MCP prompt template support
- **error** — `McpError` error types

## Usage

```toml
# Cargo.toml (workspace root)
zeph-mcp = { path = "crates/zeph-mcp" }
```

## License

MIT
