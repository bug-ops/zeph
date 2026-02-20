# MCP Integration

Connect external tool servers via [Model Context Protocol](https://modelcontextprotocol.io/) (MCP). Tools are discovered, embedded, and matched alongside skills using the same cosine similarity pipeline — only relevant MCP tools are injected into the prompt, so adding more servers does not inflate token usage.

## Configuration

### Stdio Transport (spawn child process)

```toml
[[mcp.servers]]
id = "filesystem"
command = "npx"
args = ["-y", "@anthropic/mcp-filesystem"]
```

### HTTP Transport (remote server)

```toml
[[mcp.servers]]
id = "remote-tools"
url = "http://localhost:8080/mcp"
```

### Security

```toml
[mcp]
allowed_commands = ["npx", "uvx", "node", "python", "python3"]
max_dynamic_servers = 10
```

`allowed_commands` restricts which binaries can be spawned as MCP stdio servers. Commands containing path separators (`/` or `\`) are rejected to prevent path traversal — only bare command names resolved via `$PATH` are accepted. `max_dynamic_servers` limits the number of servers added at runtime.

Environment variables containing secrets (API keys, tokens, credentials — 21 variables plus `BASH_FUNC_*` patterns) are automatically stripped from MCP child process environments. See [MCP Security](../reference/security/mcp.md) for the full blocklist.

## Dynamic Management

Add and remove MCP servers at runtime via chat commands:

```text
/mcp add filesystem npx -y @anthropic/mcp-filesystem
/mcp add remote-api http://localhost:8080/mcp
/mcp list
/mcp remove filesystem
```

After adding or removing a server, Qdrant registry syncs automatically for semantic tool matching.

## How Matching Works

MCP tools are embedded in Qdrant (`zeph_mcp_tools` collection) with BLAKE3 content-hash delta sync. Unified matching injects both skills and MCP tools into the system prompt by relevance score — keeping prompt size O(K) instead of O(N) where N is total tools across all servers.
