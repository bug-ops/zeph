# A2A Protocol

Zeph includes an embedded [A2A protocol](https://github.com/a2aproject/A2A) server for agent-to-agent communication. When enabled, other agents can discover and interact with Zeph via the standard A2A JSON-RPC 2.0 API.

## Quick Start

```bash
ZEPH_A2A_ENABLED=true ZEPH_A2A_AUTH_TOKEN=secret ./target/release/zeph
```

## Endpoints

| Endpoint | Description | Auth |
|----------|-------------|------|
| `/.well-known/agent.json` | Agent discovery | Public (no auth) |
| `/a2a` | JSON-RPC endpoint (`message/send`, `tasks/get`, `tasks/cancel`) | Bearer token |
| `/a2a/stream` | SSE streaming endpoint | Bearer token |

> Set `ZEPH_A2A_AUTH_TOKEN` to secure the server with bearer token authentication. The agent card endpoint remains public per A2A spec.

### Agent Card

The `/.well-known/agent.json` response includes a `protocolVersion` field set to `"0.2.1"`. This allows discovery clients to verify compatibility before sending requests.

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
- **Rate limiting:** per-IP sliding window (default: 60 requests/minute) with TTL-based eviction (stale entries swept every 60s, hard cap at 10,000 entries)

## Task Processing

Incoming `message/send` requests are routed through `TaskProcessor`, which implements streaming via `ProcessorEvent`:

```rust
pub enum ProcessorEvent {
    StatusUpdate { state: TaskState, is_final: bool },
    ArtifactChunk { text: String, is_final: bool },
}
```

The processor sends events through an `mpsc::Sender<ProcessorEvent>`, enabling per-token SSE streaming to connected clients. In daemon mode, `AgentTaskProcessor` bridges A2A requests to the full agent loop (LLM, tools, memory, MCP) via `LoopbackChannel`, providing complete agent capabilities over the A2A protocol.

## Invocation-Bound Capability Tokens (IBCT)

IBCT are per-call security tokens that bind each A2A request to a specific task and endpoint. They prevent replayed or forwarded A2A requests from being accepted by other tasks or endpoints.

### Enabling IBCT

Gated on the `ibct` feature flag (enabled in the `full` feature set):

```toml
[a2a]
ibct_ttl_secs = 300          # Token validity window (default: 300 s)

# Option A: inline key (dev/test only — prefer vault ref in production)
[[a2a.ibct_keys]]
key_id = "k1"
key_bytes_hex = "73757065722d73656372657400000000000000000000000000000000000000"

# Option B: vault reference (recommended for production)
ibct_signing_key_vault_ref = "ZEPH_A2A_IBCT_KEY"
```

When `ibct_keys` or `ibct_signing_key_vault_ref` is set, outgoing A2A client calls include an `X-Zeph-IBCT` header containing a base64-encoded JSON token.

### Token Structure

Each token is HMAC-SHA256 signed and contains:

| Field | Description |
|-------|-------------|
| `key_id` | Key identifier (for rotation without downtime) |
| `task_id` | A2A task the token is scoped to |
| `endpoint` | Target endpoint URL |
| `issued_at` | Unix timestamp of issuance |
| `expires_at` | Expiry timestamp (`issued_at + ibct_ttl_secs`) |
| `signature` | HMAC-SHA256 over key_id + task_id + endpoint + timestamps |

### Key Rotation

Multiple keys can be listed in `[[a2a.ibct_keys]]`. The first key is used for signing; all keys are tried during verification. To rotate:

1. Add the new key as the first entry (it will be used for new tokens).
2. Keep the old key in the list temporarily (it will still verify existing tokens).
3. After `ibct_ttl_secs` has elapsed, remove the old key.

## A2A Client

Zeph can also connect to other A2A agents as a client:

- `A2aClient` wraps reqwest, uses JSON-RPC 2.0 for all RPC calls
- `AgentRegistry` with TTL-based cache for agent card discovery
- SSE streaming via `eventsource-stream` for real-time task updates
- Bearer token auth passed per-call to all client methods
