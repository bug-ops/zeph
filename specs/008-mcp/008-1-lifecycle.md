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
    ) -> Result<ToolResult>;
}
```

## Connection Maintenance

`McpClient` (via rmcp crate) maintains bidirectional messaging:

- **Requests**: Zeph sends tool calls, resource reads, prompt queries
- **Notifications**: Server sends `tools/list_changed`, `resources/list_changed`, etc.
- **Responses**: Async correlation by request ID; timeout triggers error
- **Errors**: JSON-RPC errors forwarded as `McpError`

Config:
```toml
[[mcp.servers]]
id = "filesystem"
transport = { type = "stdio", command = "npx", args = ["-y", "@modelcontextprotocol/server-filesystem"] }
timeout_secs = 30
trust_level = "untrusted"
tool_allowlist = ["read_file", "list_directory"]
```

## Failure Detection & Reconnection

Initial connection failures are logged with stderr; transient failures during tool calls trigger retry logic (via rmcp's error handling and `McpToolExecutor` retry). Long-lived servers are not actively health-checked (only re-validated if a notification arrives).

Failed servers are marked `Error` in `ServerConnectOutcome` and their tools are unavailable for dispatch.

## Graceful Shutdown

Cleanup on agent termination (via `TaskSupervisor::shutdown_all`):

```rust
impl McpManager {
    /// Shutdown all servers gracefully.
    pub async fn shutdown_all(&self, timeout: Duration) -> Result<()> {
        for server in &self.servers {
            // Cancel pending requests
            // Send shutdown signal if server supports it
            // Close stdio handles or HTTP connections
            // Wait for graceful close or force-terminate
        }
    }
}
```

Timeout: configurable per agent (default 5s). If shutdown exceeds timeout, remaining servers are force-closed without waiting for pending requests.

## Tool Registry Coordination

`McpToolRegistry` (`crates/zeph-mcp/src/registry.rs`) indexes tools by server. When a server notifies `tools/list_changed`, the registry re-fetches and updates its index:

```rust
impl ToolListChangedHandler {
    pub async fn on_tools_list_changed(&self, server_id: &str) -> Result<()> {
        // 1. Fetch updated tools/list from server
        // 2. Sanitize tool definitions
        // 3. Update registry index
        // 4. Fire MCP client callback if attached
    }
}
```

## Transport Security

- **Stdio**: inherited from parent environment (scrubbed of secrets); runs with same UID as agent
- **HTTP**: URL resolved via `validate_url_ssrf()` — private IPs blocked unless allowlisted
- **OAuth**: authorization endpoints validated via `validate_oauth_metadata_urls()` against SSRF blocklist

## See Also

- [[008-mcp/spec]] — Parent
- [[008-2-discovery]] — Tool discovery and semantic indexing
- [[008-3-security]] — Tool sanitization and policy enforcement
- [[010-3-authorization]] — SSRF protection
