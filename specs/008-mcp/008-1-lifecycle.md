---
aliases:
  - MCP Server Lifecycle
  - MCP Connection Management
  - MCP Startup & Shutdown
tags:
  - sdd
  - spec
  - mcp
  - protocol
created: 2026-04-10
status: complete
related:
  - "[[008-mcp/spec]]"
  - "[[008-2-discovery]]"
  - "[[008-3-security]]"
  - "[[002-agent-loop/spec]]"
---

# Spec: MCP Server Lifecycle

Server startup/shutdown, connection management, stdio environment isolation, graceful cleanup.

## Overview

MCP servers are subprocess-based or HTTP tool providers. Zeph manages their complete lifecycle: spawning with environment isolation, maintaining connections, detecting failures, and graceful shutdown. The `McpManager` (`crates/zeph-mcp/src/manager/`) orchestrates multi-server lifecycle.

## Key Invariants

**Always:**
- Each stdio MCP server runs in isolated subprocess with `ZEPH_*` secrets scrubbed from environment
- Server startup failures logged with stderr/exit code
- Server connections are bidirectional: Zeph sends requests, servers send notifications (`tools/list_changed`, etc.)
- Server shutdown waits for pending requests (timeout: configurable via `shutdown_timeout_secs`)
- Env var scrubbing happens unconditionally before subprocess spawn

**Never:**
- Pass `ZEPH_*` secrets to MCP server environment — they are already scrubbed
- Leave zombie processes on shutdown — always collect process status
- Assume server is healthy without probing — initial `tools/list` serves as health check
- Send requests to a server while it is shutting down

## Startup Sequence

```
1. Validate transport config (stdio command, HTTP URL, or OAuth credentials)
2. Scrub environment: remove ZEPH_*, AWS_*, GITHUB_TOKEN, etc.
3. Spawn subprocess (stdio) or establish HTTP connection (http/oauth)
4. Send initialize request via rmcp crate, await response (timeout: 10s)
5. Fetch tools/list via DefaultMcpProber for pre-connect injection scan
6. Sanitize tool definitions (names, descriptions, input schemas)
7. Register tools in `McpToolRegistry` with trust scores
8. Mark server as "connected" in manager state
```

API:

```rust
pub struct ServerEntry {
    pub id: String,                          // Unique server ID
    pub transport: McpTransport,             // Stdio/Http/OAuth variant
    pub timeout: Duration,                   // Request timeout
    pub trust_level: McpTrustLevel,         // Trusted/Untrusted/Sandboxed
    pub tool_allowlist: Option<Vec<String>>, // Optional tool allowlist
    pub allow_untrusted_without_allowlist: bool,  // Secure default: false
    pub expected_tools: Vec<String>,        // Attestation list
    pub roots: Vec<String>,                 // Root resources for file servers
    pub tool_metadata: HashMap<String, ToolSecurityMeta>,  // Pre-defined tool metadata
    pub elicitation_enabled: bool,          // Tool parameter probing
    pub elicitation_timeout_secs: u64,      // Probing timeout
    pub env_isolation: bool,                // Scrub env vars (default: true)
    pub media_passthrough: bool,            // Allow media types in responses
}

pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
    OAuth {
        auth_uri: String,
        token_uri: String,
        client_id: String,
        client_secret: Secret,
    },
}

impl McpManager {
    /// Connect all configured servers in parallel.
    pub async fn connect_all(&self) -> (Vec<McpTool>, Vec<ServerConnectOutcome>);
    
    /// Call a tool on a specific server.
    pub async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<CallToolResult, McpError>;
}
```

## Connection Maintenance

`McpClient` (via rmcp crate) maintains bidirectional messaging:

- **Requests**: Zeph sends tool calls, resource reads, prompt queries
- **Notifications**: Server sends `tools/list_changed`, `resources/list_changed`, etc.
- **Responses**: Async correlation by request ID; timeout triggers error
- **Errors**: JSON-RPC errors forwarded as `McpError`

**Configuration example** (`[[mcp.servers]]`):

For a Stdio transport (subprocess):
```toml
[[mcp.servers]]
id = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem"]
# env = { CUSTOM_VAR = "value" }  # Optional: env vars for the subprocess
timeout = 30                       # Timeout in seconds for requests
trust_level = "untrusted"          # "trusted", "untrusted", or "sandboxed"
tool_allowlist = ["read_file", "list_directory"]
```

For HTTP transport (remote service):
```toml
[[mcp.servers]]
id = "remote-api"
url = "http://localhost:8000/mcp"
headers = { Authorization = "Bearer ${VAULT_API_TOKEN}" }  # Supports vault refs
timeout = 60
trust_level = "untrusted"
```

**Field reference:**
- `id`: Unique server identifier
- `command` (Stdio): executable to spawn
- `args` (Stdio): command-line arguments
- `url` (HTTP): remote server URL
- `env` (Stdio): environment variables (supports `${VAULT_KEY}` references)
- `headers` (HTTP): static HTTP headers
- `timeout`: request timeout in seconds
- `trust_level`: `"trusted"` | `"untrusted"` | `"sandboxed"`
- `tool_allowlist`: optional array of tool names to expose
- `expected_tools`: optional array for attestation/schema-drift detection
- `media_passthrough`: enable image attachment (default: false)
- `env_isolation`: isolate subprocess environment (default: false)

## Failure Detection & Reconnection

Initial connection failures are logged with stderr; transient failures during tool calls trigger retry logic (via rmcp's error handling and `McpToolExecutor` retry). Long-lived servers are not actively health-checked (only re-validated if a notification arrives).

Failed servers are marked `Error` in `ServerConnectOutcome` and their tools are unavailable for dispatch.

## Graceful Shutdown

Cleanup on agent termination — `McpManager` is dropped after the agent loop exits:

```rust
impl McpManager {
    /// Shutdown all servers gracefully (by-value self, no timeout param).
    /// 
    /// Consumes the manager; servers are shut down as part of the Drop impl.
    pub async fn shutdown_all(self) {
        // Cancel pending requests for all servers
        // Send shutdown signal if server supports it
        // Close stdio handles or HTTP connections
        // Wait for graceful close or force-terminate
    }
}
```

**Timeout behavior**: shutdown is not time-bounded in the public API. The agent loop exit triggers a shutdown cascade; slow servers do not block agent termination since the manager is dropped.

## Tool Registry Coordination

`McpToolRegistry` (`crates/zeph-mcp/src/registry.rs`) indexes tools by server. When a server sends the `tools/list_changed` notification, the manager re-fetches and updates the registry:
- Fetch updated `tools/list` from the server
- Sanitize tool definitions (scrub injection patterns)
- Update the registry index
- Invalidate any per-message tool pruning caches (since tool set changed)

## Transport Security

- **Stdio**: inherited from parent environment (scrubbed of secrets via `env_blocklist`); runs with same UID as agent
- **HTTP**: URL resolved via `validate_url()` (crates/zeph-tools/src/net.rs) — private IPs and non-HTTPS schemes blocked
- **OAuth**: authorization endpoints validated against SSRF blocklist

## See Also

- [[008-mcp/spec]] — Parent
- [[008-2-discovery]] — Tool discovery and semantic indexing
- [[008-3-security]] — Tool sanitization and policy enforcement
- [[010-3-authorization]] — SSRF protection
