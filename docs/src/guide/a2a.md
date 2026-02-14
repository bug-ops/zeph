# A2A Protocol

Zeph includes an embedded [A2A protocol](https://github.com/a2aproject/A2A) server for agent-to-agent communication. When enabled, other agents can discover and interact with Zeph via the standard A2A JSON-RPC 2.0 API.

## Quick Start

```bash
ZEPH_A2A_ENABLED=true ZEPH_A2A_AUTH_TOKEN=secret ./target/release/zeph
```

## Endpoints

| Endpoint | Description | Auth |
|----------|-------------|------|
| `/.well-known/agent-card.json` | Agent discovery | Public (no auth) |
| `/a2a` | JSON-RPC endpoint (`message/send`, `tasks/get`, `tasks/cancel`) | Bearer token |
| `/a2a/stream` | SSE streaming endpoint | Bearer token |

> Set `ZEPH_A2A_AUTH_TOKEN` to secure the server with bearer token authentication. The agent card endpoint remains public per A2A spec.

## Configuration

```toml
[a2a]
enabled = true
host = "0.0.0.0"
port = 8080
public_url = "https://agent.example.com"
auth_token = "secret"
rate_limit = 60
```

## Network Security

- **TLS enforcement:** `a2a.require_tls = true` rejects HTTP endpoints (HTTPS only)
- **SSRF protection:** `a2a.ssrf_protection = true` blocks private IP ranges (RFC 1918, loopback, link-local) via DNS resolution
- **Payload limits:** `a2a.max_body_size` caps request body (default: 1 MiB)
- **Rate limiting:** per-IP sliding window (default: 60 requests/minute)

## Task Processing

Incoming `message/send` requests are routed through `AgentTaskProcessor`, which forwards the message to the configured LLM provider for real inference. The processor creates a task, sends the user message to the LLM, and returns the model response as a completed A2A task artifact.

> Current limitation: the A2A task processor runs inference only (no tool execution or memory context).

## A2A Client

Zeph can also connect to other A2A agents as a client:

- `A2aClient` wraps reqwest, uses JSON-RPC 2.0 for all RPC calls
- `AgentRegistry` with TTL-based cache for agent card discovery
- SSE streaming via `eventsource-stream` for real-time task updates
- Bearer token auth passed per-call to all client methods
