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

## Startup Auto-Retry with Exponential Backoff (#3578)

MCP server startup is unreliable in practice: a server process may crash before
completing the `initialize` handshake, or a network MCP server may be temporarily
unavailable at agent start time. Without retry, a single failed server blocks agent
startup or silently reduces the tool catalog.

### Retry Contract

`McpManager::start_with_retry(config)` wraps `start_server()` in an exponential
backoff loop:

```
attempt 1: immediate
attempt 2: base_delay_ms (default 200 ms)
attempt 3: base_delay_ms × backoff_factor (default 2.0)
...
attempt N: min(base_delay_ms × backoff_factor^(N-2), max_delay_ms)
```

On exhaustion (all `max_startup_retries` attempts failed):

- **`critical = false` servers**: log `ERROR`, skip server, agent starts without it.
  The missing server's tools are absent from the catalog until a `/mcp reconnect` command.
- **`critical = true` servers**: return `Err(McpError::CriticalServerStartFailed)`,
  aborting agent startup.

### Jitter

Each backoff delay is jittered by `±25%` (uniform random) to prevent thundering herds
when multiple MCP servers restart simultaneously after a crash.

### Tracing

Each retry attempt emits a `tracing::warn!` with attempt number, server name, and
error. The initial failure emits `tracing::info!` (not warn — first attempt failure is
expected in slow-start environments).

### Config

```toml
[[mcp.servers]]
name = "local-tools"
command = "python3 /path/to/server.py"
stdio = "pipe"  # or "pty" for terminal emulation
timeout_init_s = 10
timeout_request_s = 30
healthcheck_interval_s = 60
critical = false                    # if true, startup failure aborts the agent
max_startup_retries = 3             # total attempts (1 initial + N-1 retries); 0 = no retry
startup_retry_base_delay_ms = 200   # base delay before first retry
startup_retry_max_delay_ms = 5000   # cap on exponential backoff
startup_retry_backoff_factor = 2.0  # multiplier applied per attempt

# Environment scrubbing: keep only these vars
allow_env_vars = ["PATH", "HOME", "RUST_LOG"]
```

### Key Invariants

- Retry delay is bounded by `startup_retry_max_delay_ms` — backoff cannot grow unbounded
- `critical = true` servers abort startup on first failure (no retry is attempted before aborting)
  — override: set `max_startup_retries > 0` to retry even critical servers before aborting
- NEVER silently swallow a critical server failure — `Err` must propagate to `McpManager::start_all`
- Jitter is applied on retries only, not on the initial attempt
- The TUI startup spinner shows per-server retry status when `max_startup_retries > 0`

## Configuration (Legacy)

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
