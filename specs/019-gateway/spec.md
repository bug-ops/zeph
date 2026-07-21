---
aliases:
  - HTTP Gateway
  - Webhook Gateway
tags:
  - sdd
  - spec
  - gateway
  - http
created: 2026-04-08
status: approved
related:
  - "[[MOC-specs]]"
  - "[[007-channels/spec]]"
---

# Spec: HTTP Gateway

> [!info]
> Webhook ingestion with bearer token authentication;
> zeph-gateway crate for incoming event integration.

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
- **Auth is mandatory, fail-closed (#6487, closes #6509's premise)**: `GatewayServer::serve()`
  refuses to start (`GatewayError::MissingAuthToken`) when `auth_token` is `None`, empty, or
  whitespace-only — since `POST /webhook` forwards its body directly into the agent's turn loop,
  an unauthenticated gateway lets any caller that reaches the listener inject content. This
  replaced the prior behavior of logging a warning and continuing with auth disabled.
  `build_router`'s `AuthConfig::require_auth` is derived from whether a non-empty token was
  actually passed in (no longer hardcoded `false`); direct `build_router()` callers (tests) can
  still construct a no-auth router, but `serve()` is the only production entry point and it
  always fails closed first.

Auth middleware flow:
1. `GatewayServer::serve()` fails closed at startup if no non-empty token is configured (see
   above) — the middleware's `None`-token skip-auth branch is only reachable via a direct
   `build_router()` call (tests), never through `serve()` in production.
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

## Startup Supervision

- `src/gateway_spawn.rs`'s `spawn_gateway_server` registers the `gateway_server` task through
  `TaskSupervisor::spawn_classified(..., Result::is_ok)` (#6510) and propagates `serve()`'s
  `Result` instead of discarding it into a log line. A startup failure (bind error, or
  `GatewayError::MissingAuthToken`) now surfaces as `TaskStatus::Failed` in
  `list_tasks()`/the TUI — previously every outcome, success or failure, was reported as
  `Completed`, making a failed gateway indistinguishable from a normal one at the supervision
  layer.
- `TaskSupervisor::spawn_classified` (`zeph-common`) is the general mechanism behind this: it
  classifies a supervised task's typed `Future::Output` via a caller-supplied `is_success`
  predicate, so a task whose `Fut::Output` encodes failure (typically `Result<T, E>`) is
  reported by its actual outcome rather than by whether the outer future merely resolved.

## Setup & Diagnostics

- `--init`'s Prometheus step, which auto-enables `[gateway]` as a side effect, now discloses that
  this also opens the `POST /webhook` endpoint and prints `zeph vault set ZEPH_GATEWAY_TOKEN
  <token>` instructions — the wizard never prompts for the raw token itself; it is resolved from
  the age vault at startup like other vault-only secrets (#6487).
- `zeph doctor` has a `gateway.auth` check that flags `[gateway] enabled = true` with no
  resolvable `auth_token` before a real launch attempt (#6487).

## Key Invariants

- Auth middleware runs before all handlers — enforced by middleware layer order in router build
- `GET /health` bypasses auth unconditionally — monitoring must work unauthenticated
- Token hash pre-computed at startup — never per-request
- `ct_eq()` mandatory for token comparison — `==` is banned
- `202 Accepted` returned immediately — gateway does not wait for agent to process
- Gateway only injects into `message_queue` via mpsc — never calls agent methods directly
- Max body size enforced (1 MiB default) — requests exceeding this are rejected with 413
- Plaintext token never stored, logged, or included in error messages
- **Fail-closed startup (#6487)**: `[gateway] enabled = true` with no non-empty `auth_token` MUST
  refuse to start (`GatewayError::MissingAuthToken`), never silently run open
- **Startup failure MUST surface as `TaskStatus::Failed` (#6510)**, never `Completed` — a
  supervised gateway that fails to bind or start is a review-visible anomaly, not silent success
