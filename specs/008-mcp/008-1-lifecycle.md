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
  - "[[002-agent-loop]]"
---

# Spec: MCP Server Lifecycle

Server startup/shutdown, connection management, stdio environment isolation, graceful cleanup.

## Overview

MCP servers are subprocess-based tool providers. Zeph manages their complete lifecycle: spawning with environment isolation, maintaining connections, detecting failures, and graceful shutdown.

## Key Invariants

**Always:**
- Each MCP server runs in isolated subprocess with env vars scrubbed
- Server startup failures logged with full context (stderr, exit code, timeout)
- Connections are bidirectional: Zeph sends requests, servers send notifications
- Server shutdown waits for pending requests (configurable timeout: default 5s)

**Never:**
- Pass secrets (API keys, tokens) to MCP server environment
- Leave zombie processes on exit (always await subprocess termination)
- Assume server is healthy without heartbeat validation

## Startup Sequence

```
1. Resolve server config (name, command, env, stdio mode)
2. Scrub environment: remove ZEPH_* secrets, keep only safe vars
3. Spawn subprocess with stdio isolation
4. Send initialize request, await response (timeout: 10s)
5. Register tool registry, store connection metadata
6. Mark server as "ready" in MCP client state
```

Code sketch:

```rust
async fn start_server(&self, config: &ServerConfig) -> Result<Connection> {
    // 1. Sanitize environment
    let env = self.scrub_secrets(&config.env);
    
    // 2. Spawn process
    let mut child = Command::new(&config.command)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn MCP server")?;
    
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    
    // 3. Initialize protocol (exchange capabilities)
    let conn = JsonRpcTransport::new(stdin, stdout);
    let caps = conn.initialize(
        &InitializeRequest {
            protocol_version: "2024-11-05",
            capabilities: /* client capabilities */,
        },
        Duration::from_secs(10),
    ).await?;
    
    // 4. Store connection
    let server_id = self.store_connection(config.name.clone(), conn).await?;
    
    Ok(server_id)
}
```

## Connection Management

Maintain bidirectional messaging:

```rust
pub struct McpConnection {
    id: String,
    name: String,
    transport: JsonRpcTransport,
    pending_requests: Arc<Mutex<HashMap<u64, Waiter>>>,
    server_notifications: Arc<Mutex<VecDeque<Notification>>>,
    health_check_interval: Duration,
}

impl McpConnection {
    async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_request_id();
        let waiter = Waiter::new();
        
        self.pending_requests.lock().insert(id, waiter.clone());
        self.transport.send_request(id, method, params).await?;
        
        // Wait for response with timeout
        waiter.wait(Duration::from_secs(30)).await
    }
    
    async fn handle_notification(&self, notif: Notification) {
        // Server can send unsolicited notifications (e.g., resource changed)
        self.server_notifications.lock().push_back(notif);
    }
}
```

## Failure Detection & Reconnection

Monitor server health:

```rust
async fn health_check_loop(&self, conn: &McpConnection) {
    let mut interval = tokio::time::interval(
        conn.health_check_interval
    );
    
    loop {
        interval.tick().await;
        
        match conn.send_request("ping", json!({})).await {
            Ok(_) => {
                conn.mark_healthy();
            }
            Err(e) => {
                log::warn!("Server {} health check failed: {}", conn.name, e);
                self.mark_unhealthy(&conn.id).await;
                
                // Trigger reconnect logic
                self.attempt_reconnect(&conn.id).await;
            }
        }
    }
}
```

## Graceful Shutdown

Cleanup on agent termination:

```rust
async fn shutdown_server(&self, server_id: &str) -> Result<()> {
    let conn = self.get_connection(server_id)?;
    
    // 1. Reject new requests
    conn.mark_shutdown();
    
    // 2. Wait for pending requests (timeout: 5s)
    let timeout = Duration::from_secs(5);
    match timeout_at(
        Instant::now() + timeout,
        self.wait_pending_requests(server_id),
    ).await {
        Ok(Ok(())) => log::info!("Server {} graceful shutdown", server_id),
        Ok(Err(e)) => log::warn!("Server {} shutdown error: {}", server_id, e),
        Err(_) => log::warn!("Server {} shutdown timeout", server_id),
    }
    
    // 3. Terminate subprocess
    self.kill_subprocess(server_id).await?;
    
    // 4. Remove from registry
    self.remove_connection(server_id).await;
    
    Ok(())
}
```

## Configuration

```toml
[[mcp.servers]]
name = "local-tools"
command = "python3 /path/to/server.py"
stdio = "pipe"  # or "pty" for terminal emulation
timeout_init_s = 10
timeout_request_s = 30
healthcheck_interval_s = 60

# Environment scrubbing: keep only these vars
allow_env_vars = ["PATH", "HOME", "RUST_LOG"]
```

## Integration Points

- [[002-agent-loop]] — MCP servers initialized during agent startup
- [[008-2-discovery]] — Server capabilities discovered after initialization
- [[008-3-security]] — Environment scrubbing occurs before spawn
- [[010-security]] — Subprocess isolation enforced here

## See Also

- [[008-mcp/spec]] — Parent
- [[008-2-discovery]] — Tool discovery after lifecycle setup
- [[008-3-security]] — Security constraints on subprocess spawning
