# zeph-mcp

[![Crates.io](https://img.shields.io/crates/v/zeph-mcp)](https://crates.io/crates/zeph-mcp)
[![docs.rs](https://img.shields.io/docsrs/zeph-mcp)](https://docs.rs/zeph-mcp)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

MCP client with multi-server lifecycle and Qdrant tool registry for Zeph.

## Overview

Implements the Model Context Protocol client for Zeph, managing connections to multiple MCP servers, discovering their tools at startup, and routing tool calls through a unified executor. Built on rmcp 0.17.

## Key Modules

- **client** — low-level MCP transport and session handling; `ToolListChangedHandler` receives `tools/list_changed` notifications, applies `sanitize_tools()` (rate-limited to once per 5 s per server, capped at 100 tools), and forwards the sanitized list to `McpManager` via a refresh channel
- **manager** — `McpManager`, `McpTransport`, `ServerEntry` for multi-server lifecycle; command allowlist validation (npx, uvx, node, python3, docker, mcpls, etc.), env var blocklist (LD_PRELOAD, DYLD_*, NODE_OPTIONS, etc.), and path separator rejection; statically configured servers (from `[[mcp.servers]]`) bypass SSRF validation to allow connections to `localhost` and private IPs — dynamically added servers retain full SSRF protection
- **sanitize** — `sanitize_tools()` applied to all tool definitions at registration time and again on every `tools/list_changed` refresh; strips 17 injection-detection patterns, Unicode Cf-category characters, and caps descriptions at 1024 bytes; fields triggering a pattern are replaced with `"[sanitized]"` — tool registration is never blocked
- **executor** — `McpToolExecutor` bridging MCP tools into the `ToolExecutor` trait
- **registry** — `McpToolRegistry` for tool lookup and optional Qdrant-backed search
- **tool** — `McpTool` wrapper with schema and metadata
- **prompt** — MCP prompt template support
- **error** — `McpError` error types

## Installation

```bash
cargo add zeph-mcp
```

## License

MIT
