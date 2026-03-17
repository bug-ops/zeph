# Spec: HTTP Gateway

## Sources

### Internal
| File | Contents |
|---|---|
| `crates/zeph-gateway/src/server.rs` | `GatewayServer`, builder pattern, shutdown |
| `crates/zeph-gateway/src/router.rs` | axum router, auth middleware, rate limit |
| `crates/zeph-gateway/src/handlers.rs` | Webhook ingestion, `/health` |
| `crates/zeph-gateway/src/error.rs` | `GatewayError` |

---

`crates/zeph-gateway/` (feature: `gateway`) — HTTP webhook ingestion with bearer auth.

## Architecture

```
GatewayServer (axum)
├── Middleware stack: auth → rate_limit → handlers
├── POST /webhook (or configured path) — ingest messages
├── GET  /health                        — liveness check (no auth)
└── AppState { webhook_tx: mpsc::Sender, started_at: Instant }
```

## Authentication

```
AuthConfig { auth_token: Option<blake3::Hash> }
```

- Token hash **pre-computed at `GatewayServer` creation** — O(1) memory, no per-request hashing of config
- Per-request: hash submitted bearer token with BLAKE3, compare via `ct_eq()` (constant-time)
- **Never use `==` for token comparison** — timing side-channel
- Only `bearer_hash` stored/logged — plaintext token never persisted
- Warning logged if binding to `0.0.0.0` without explicit acknowledgment

Auth middleware flow:
1. If `auth_token` is `None` → skip auth (no token configured)
2. Extract `Authorization: Bearer <token>` header
3. `blake3::hash(submitted_token).ct_eq(expected_hash)` — constant-time
4. Missing header or mismatch → 401 Unauthorized

## Rate Limiting

- Default: 120 req/min per connection (configurable via `with_rate_limit()`)
- Enforced as axum middleware layer — before handlers

## Message Ingestion

```
POST /webhook
  Body: JSON or plain text
  Max size: 1 MiB (configurable via with_max_body_size())
  → QueuedMessage { content, source, metadata }
  → webhook_tx.send() [mpsc]
  → 202 Accepted (immediate, no waiting for agent)
```

## Health Endpoint

- `GET /health` — always bypasses auth
- Returns 200 + uptime since `started_at`
- Monitoring probes must work without a bearer token

## Shutdown

- Shutdown via `watch::Receiver` — server listens for signal, gracefully closes connections
- Webhook sender (`webhook_tx`) is closed on shutdown

## Key Invariants

- Auth middleware runs before all handlers — enforced by middleware layer order in router build
- `GET /health` bypasses auth unconditionally — monitoring must work unauthenticated
- Token hash pre-computed at startup — never per-request
- `ct_eq()` mandatory for token comparison — `==` is banned
- `202 Accepted` returned immediately — gateway does not wait for agent to process
- Gateway only injects into `message_queue` via mpsc — never calls agent methods directly
- Max body size enforced (1 MiB default) — requests exceeding this are rejected with 413
- Plaintext token never stored, logged, or included in error messages
