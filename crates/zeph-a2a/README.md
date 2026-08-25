# zeph-a2a

[![Crates.io](https://img.shields.io/crates/v/zeph-a2a)](https://crates.io/crates/zeph-a2a)
[![docs.rs](https://img.shields.io/docsrs/zeph-a2a)](https://docs.rs/zeph-a2a)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.98-blue)](https://www.rust-lang.org)

A2A protocol client and server with agent discovery for Zeph.

## Overview

Implements the Agent-to-Agent (A2A) protocol over JSON-RPC 2.0, enabling Zeph to discover, communicate with, and delegate tasks to remote agents. Feature-gated behind `a2a`; the server component requires the `server` sub-feature.

## Key Modules

- **client** — `A2aClient` for sending tasks and messages to remote agents
- **server** — `A2aServer` exposing an A2A-compliant endpoint with `ProcessorEvent` streaming via `mpsc::Sender` (requires `server` feature)
- **card** — `AgentCardBuilder` for constructing agent capability cards; includes `protocolVersion` field set to `A2A_PROTOCOL_VERSION` constant (`"0.2.1"`) in the default card served at `/.well-known/agent.json`
- **discovery** — `AgentRegistry` for agent lookup and registration, with an optional card-signing + URL-origin trust policy applied in `discover()` (see below)
- **card_signing** — `SigAlg`, `SignatureVerification`, `TrustedKey` for A2A 1.0.0 `AgentCardSignature` verification (requires `card-signing`)
- **ibct** — `Ibct`, `IbctKey`, `IbctError` for invocation-bound capability tokens (requires `ibct`)
- **jsonrpc** — JSON-RPC 2.0 request/response types
- **types** — shared protocol types (Task, Message, Artifact, etc.)
- **error** — `A2aError` error types

## IBCT (Invocation-Bound Capability Tokens)

IBCT is an opt-in, finer-grained authorization layer on top of the coarse bearer-token gate: HMAC-SHA256 capability tokens scoped to a specific `task_id` + `endpoint`, sent in the `X-Zeph-IBCT` request header.

**Server (`A2aServer::with_ibct_keys`)**: when configured with a non-empty key set, `zeph-a2a`'s router rejects every `/a2a` and `/a2a/stream` request that does not carry a valid `X-Zeph-IBCT` header — `401` if the header is missing or undecodable, `403` if it fails verification (bad signature, expired, unknown `key_id`, or scoped to the wrong endpoint/task). The expected `endpoint` is the server's own advertised `AgentCard::url`; the expected `task_id` is read from the request (`params.id` for `tasks/get`/`tasks/cancel`, `params.message.taskId` for `message/send`/`message/stream` — the empty-string sentinel for a brand-new task that has no server-assigned ID yet). An empty key set (the default) disables enforcement entirely.

**Client (`A2aClient::with_ibct_key`)**: when configured with an `IbctKey`, the client issues a token scoped to the target endpoint + task on every request and attaches it alongside the bearer token. Issuance failures are logged and the request proceeds without the header — the server decides whether to reject it.

Key rotation is supported via `key_id`: multiple keys can be configured on the server simultaneously, so an old signing key stays valid for verification until every token it signed has expired.

| Config field | Type | Default | Description |
|---|---|---|---|
| `ibct_keys` | `Vec<IbctKeyConfig>` | `[]` | Named HMAC keys (`{ key_id, key_hex }`) verified against incoming tokens |
| `ibct_signing_key_vault_ref` | string | `""` | Vault reference for the primary key (`key_id = "primary"`); takes precedence over `ibct_keys[0]` |
| `ibct_ttl_secs` | u64 | `300` | Token validity window in seconds, for callers issuing tokens with this TTL |

```toml
[a2a]
ibct_ttl_secs = 300
ibct_signing_key_vault_ref = "ZEPH_A2A_IBCT_KEY"

[[a2a.ibct_keys]]
key_id = "k1"
key_hex = "68656c6c6f2d7365637265742d6b6579"   # legacy inline path; prefer the vault ref above
```

**Note:** IBCT signing/verification requires the `ibct` feature flag. Without it, `Ibct::issue`/`Ibct::verify` always return `IbctError::FeatureDisabled` — a server configured with `ibct_keys` would then reject every request, and a client configured with `with_ibct_key` would log a warning and send no header on every request.

**Important — this ships the enforcement primitive, not an activated end-to-end control.** As of this writing, no caller bundled in this repository calls `A2aClient::with_ibct_key`: `src/tui_remote.rs`'s `A2aClient` usage (the `--connect` remote-TUI-over-A2A-SSE attach feature) does not issue IBCT tokens, and there is no delegation client that spawns subagent tasks over A2A with a token attached. Concretely:

- With the default `ibct_keys = []`, the server stays a no-op — enabling this fix alone does not change behavior for any existing deployment.
- Setting `ibct_keys` to a non-empty list makes the server require `X-Zeph-IBCT` on every `/a2a` and `/a2a/stream` request. Since nothing in this repository attaches that header, doing so will `401` `zeph --connect`'s own `tui_remote` client and any standard (non-Zeph) A2A peer that has no knowledge of this header — it does not, by itself, protect a delegated subagent task from a leaked bearer token, because no delegation client using IBCT exists yet to protect.
- To get real protection from IBCT, an operator (or a follow-up change) must build/wire a caller — most likely a task-delegation client for subagent orchestration — that calls `with_ibct_key` and scopes tokens to the tasks it delegates, *before* enabling `ibct_keys` on the receiving server.

## Agent Card trust policy (JWS signature verification)

`AgentRegistry` supports an optional, feature-gated (`card-signing`) A2A 1.0.0 `AgentCardSignature`
check applied inside `discover()`, closing the card-spoofing/impersonation gap where a peer card was
trusted unauthenticated with no cross-check between the queried base URL and the card's own `url`
field.

`AgentRegistry::with_trust(policy, trusted_keys)` configures a tri-state `CardTrustPolicy`
(`Ignore` / `Prefer` / `Require`, default `Ignore`) combining signature verification and
URL-origin consistency via most-severe-wins precedence, checked against an out-of-band
operator-configured trusted-key store (never the card-supplied `jku`, which would reopen an SSRF
surface this crate already guards against elsewhere). Not calling `with_trust` leaves the registry
at `Ignore` with no trusted keys — zero behavior change for existing callers.

```rust
use zeph_a2a::{AgentRegistry, CardTrustPolicy};
use std::time::Duration;

let registry = AgentRegistry::new(reqwest::Client::new(), Duration::from_secs(300))
    .with_trust(CardTrustPolicy::Prefer, vec![]);
```

`zeph --connect <URL>` — the only outbound A2A client path in the binary — wires
`AgentRegistry::discover` with the operator's configured `[a2a] card_trust_policy` and
`trusted_agent_keys`.

> [!WARNING]
> Canonicalization is implemented per the A2A spec text but has not been validated against a real
> `a2a-sdk`-produced signed-card vector — `require` may reject genuinely valid peers until this is
> proven (tracked in [#6201](https://github.com/bug-ops/zeph/issues/6201)).

## Authentication

`A2aServer` supports bearer token authentication via the `with_auth()` builder method. When `auth_token` is `None`, the server emits a `tracing::warn!` at startup indicating that the endpoint is unauthenticated.

```rust,ignore
use std::sync::Arc;
use tokio::sync::watch;
use zeph_a2a::{A2aServer, AgentCardBuilder};

let card = AgentCardBuilder::new("my-agent", "http://localhost:9090", "0.1.0").build();
let (_shutdown_tx, shutdown_rx) = watch::channel(false);

A2aServer::new(card, Arc::new(my_processor), "0.0.0.0", 9090, shutdown_rx)
    .with_auth(Some("secret-token"))
    .with_rate_limit(120)   // requests per 60s window per IP; 0 disables
    .serve()
    .await?;
```

The token is hashed once at construction time; each request compares blake3 hashes of both sides to prevent timing attacks. `A2aServer::with_require_auth(true)` rejects all requests when no token is configured. Failed-auth requests (missing/invalid bearer token or IBCT header) are also subject to the same per-IP rate limit as ordinary requests, closing a brute-force vector against the auth layer itself.

## Features

| Feature | Description |
|---------|-------------|
| `server` | Enables `A2aServer`, `TaskManager`, and `TaskProcessor` with an axum HTTP handler and bearer auth (requires `axum`, `tower`, `tower-http`) |
| `ibct`   | Enables `Ibct` token issuance and verification (HMAC-SHA256) |
| `card-signing` | Enables A2A 1.0.0 `AgentCardSignature` verification and the `CardTrustPolicy` trust check in `AgentRegistry::discover` (requires `p256`, `serde_json_canonicalizer`). Must be compiled in together with `zeph-config`'s matching marker feature — `card_trust_policy = "require"` fails config validation otherwise. |

## Installation

```bash
cargo add zeph-a2a

# With server component
cargo add zeph-a2a --features server
```

Enabled via the `a2a` feature flag on the root `zeph` crate.

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
