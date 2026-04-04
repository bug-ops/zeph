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

### Per-Server Trust and Tool Allowlist

Each `[[mcp.servers]]` entry accepts a `trust_level` and an optional `tool_allowlist` to control which tools from that server are exposed to the agent.

```toml
# Operator-controlled server: all tools allowed, SSRF checks skipped
[[mcp.servers]]
id = "internal-tools"
command = "npx"
args = ["-y", "@acme/internal-mcp"]
trust_level = "trusted"

# Community server: only the listed tools are exposed
[[mcp.servers]]
id = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]
trust_level = "untrusted"
tool_allowlist = ["read_file", "list_directory", "search_files"]

# Sandboxed server: fail-closed — no tools exposed unless explicitly listed
[[mcp.servers]]
id = "experimental"
url = "http://localhost:9000/mcp"
trust_level = "sandboxed"
tool_allowlist = ["safe_tool_a", "safe_tool_b"]
```

| Trust Level | Tool Exposure | SSRF Checks | Notes |
|-------------|--------------|-------------|-------|
| `trusted` | All tools | Skipped | For operator-controlled, static-config servers |
| `untrusted` (default) | All tools | Applied | Emits a startup warning when `tool_allowlist` is empty |
| `sandboxed` | Only `tool_allowlist` entries | Applied | Empty allowlist exposes zero tools (fail-closed) |

The default trust level is `untrusted`. When `tool_allowlist` is not set on an `untrusted` server, a startup warning is logged to encourage explicit allowlisting of the tools you intend to use.

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

## Native Tool Integration (Claude / OpenAI)

When the active provider supports structured tool calling (Claude, OpenAI), MCP tools are exposed as native `ToolDefinition`s — no text injection into the system prompt.

`McpToolExecutor` implements `tool_definitions()`, which returns all connected MCP tools as typed definitions with qualified names in `server_id:tool_name` format. The agent calls `execute_tool_call()` when the LLM returns a structured tool_use block for an MCP tool. The executor parses the qualified name, looks up the tool in the shared list, and dispatches the call to `manager.call_tool()`.

The shared tool list (`Arc<RwLock<Vec<McpTool>>>`) is updated automatically when servers are added or removed via `/mcp add` / `/mcp remove`. This means the provider sees the current tool set on every turn without requiring a restart.

For providers without native tool support (Ollama with `tool_use = false`, Candle), `append_mcp_prompt()` falls back to injecting tool descriptions as text into the system prompt, filtered by relevance score via Qdrant.

## Semantic Tool Discovery

By default, MCP tools are matched against the current request using the same cosine similarity pipeline as skills. The `SemanticToolIndex` adds a configurable discovery layer on top of this baseline:

```toml
[mcp.tool_discovery]
strategy = "Embedding"          # "Embedding" (default), "Llm", or "None"
top_k = 10                      # Maximum tools to inject per turn (default: 10)
min_similarity = 0.30           # Minimum cosine similarity for a tool to be included (default: 0.30)
always_include = ["read_file"]  # Tool names that bypass the similarity gate entirely
min_tools_to_filter = 5         # Only apply filtering when the server exposes at least this many tools (default: 5)
```

`strategy` controls how candidate tools are ranked:

| Value | Behavior |
|-------|----------|
| `Embedding` | Embed the user query and rank tools by cosine similarity. Requires an embedding provider. |
| `Llm` | Ask a lightweight LLM to select the most relevant tools from the full list. Higher latency; useful for tools with ambiguous descriptions. |
| `None` | Disable filtering; all tools from all servers are injected on every turn. |

`always_include` accepts bare tool names or qualified `server_id:tool_name` strings. Entries in this list are injected regardless of their similarity score. Use it for tools the agent should always have available (e.g., `read_file`, `list_directory`).

`min_tools_to_filter` prevents aggressive filtering on small servers. When a server exposes fewer tools than this value, all tools from that server are included unconditionally.

## MCP Elicitation

MCP servers can request structured user input mid-task via the `elicitation/create` protocol method. This allows a server to prompt for missing parameters, confirmations, or credentials without requiring a separate out-of-band channel.

### Enabling Elicitation

Elicitation is disabled by default. Enable it globally or per server:

```toml
[mcp]
elicitation_enabled = true       # global default (default: false)
elicitation_timeout = 120        # seconds to wait for user input (default: 120)
elicitation_queue_capacity = 16  # max queued requests (default: 16)
elicitation_warn_sensitive_fields = true  # warn before sensitive field prompts

[[mcp.servers]]
id = "my-server"
command = "npx"
args = ["-y", "@acme/mcp-server"]
elicitation_enabled = true       # per-server override (overrides global default)
```

`Sandboxed` trust-level servers are never permitted to elicit regardless of config.

### How It Works

When a server sends `elicitation/create`:

- **CLI:** the user sees a phishing-prevention header showing the server name, followed by field prompts. Fields are typed (string, integer, number, boolean, enum).
- **Non-interactive channels** (Telegram, ACP without a connected client): the request is automatically declined.
- If the request queue is full (exceeds `elicitation_queue_capacity`), the request is auto-declined with a warning log instead of blocking or accumulating indefinitely.

### Security Notes

- Always review which servers have `elicitation_enabled = true`. A compromised server with elicitation access can prompt for arbitrary user input.
- `elicitation_warn_sensitive_fields = true` (default) logs a warning when field names match secret patterns before prompting.
- See [Elicitation Security](../reference/security/mcp.md#elicitation-security) for the full security model.

## MCP Roots Protocol

Zeph implements the MCP Roots protocol, which allows MCP servers to discover the project root directory and workspace structure. When a server requests roots, Zeph responds with the current working directory and any configured project paths.

Tool descriptions from MCP servers are capped at a configurable limit to prevent oversized prompt injection from servers with verbose tool descriptions.

## How Matching Works

MCP tools are embedded in Qdrant (`zeph_mcp_tools` collection) with BLAKE3 content-hash delta sync. Unified matching injects both skills and MCP tools into the system prompt by relevance score — keeping prompt size O(K) instead of O(N) where N is total tools across all servers.
