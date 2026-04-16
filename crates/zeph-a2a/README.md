# zeph-a2a

[![Crates.io](https://img.shields.io/crates/v/zeph-a2a)](https://crates.io/crates/zeph-a2a)
[![docs.rs](https://img.shields.io/docsrs/zeph-a2a)](https://docs.rs/zeph-a2a)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

A2A protocol client and server with agent discovery for Zeph.

## Overview

Implements the Agent-to-Agent (A2A) protocol over JSON-RPC 2.0, enabling Zeph to discover, communicate with, and delegate tasks to remote agents. Feature-gated behind `a2a`; the server component requires the `server` sub-feature.

## Key Modules

- **client** — `A2aClient` for sending tasks and messages to remote agents
- **server** — `A2aServer` exposing an A2A-compliant endpoint with `ProcessorEvent` streaming via `mpsc::Sender` (requires `server` feature)
- **card** — `AgentCardBuilder` for constructing agent capability cards; includes `protocolVersion` field set to `A2A_PROTOCOL_VERSION` constant (`"0.2.1"`) in the default card served at `/.well-known/agent.json`
- **discovery** — `AgentRegistry` for agent lookup and registration
- **jsonrpc** — JSON-RPC 2.0 request/response types
- **types** — shared protocol types (Task, Message, Artifact, etc.)
- **error** — `A2aError` error types

## IBCT (Invocation-Bound Capability Tokens)

When the `ibct` feature is enabled, Zeph signs outbound A2A requests with HMAC-SHA256 capability tokens. Each token is bound to a single invocation and expires after a configurable TTL, preventing replay attacks.

The token is sent in the `X-Zeph-IBCT` request header. Remote agents that support IBCT can validate the token using the shared key.

Key rotation is supported via `key_id`: multiple keys can be configured simultaneously; Zeph signs with the current signing key and includes `key_id` in the token so the receiver knows which key to verify against.

| Config field | Type | Default | Description |
|---|---|---|---|
| `ibct_keys` | `Vec<IbctKey>` | `[]` | Named HMAC keys (`{ key_id, secret }`) |
| `ibct_signing_key_vault_ref` | string | `""` | Vault reference for the active signing key secret |
| `ibct_ttl_secs` | u64 | `60` | Token validity window in seconds |

```toml
[a2a]
ibct_ttl_secs = 60
ibct_signing_key_vault_ref = "ZEPH_A2A_IBCT_KEY"

[[a2a.ibct_keys]]
key_id = "k1"
secret = ""   # resolved from vault via ibct_signing_key_vault_ref
```

> [!NOTE]
> IBCT is gated behind the `ibct` feature flag. When the feature is disabled, the `X-Zeph-IBCT` header is never sent.

## Authentication

`A2aServer` supports bearer token authentication via the `with_auth()` builder method. When `auth_token` is `None`, the server emits a `tracing::warn!` at startup indicating that the endpoint is unauthenticated.

```rust
A2aServer::new(addr, sender)
    .with_auth(Some("secret-token".to_string()))
    .serve()
    .await?;
```

Token comparison uses `subtle::ConstantTimeEq` to prevent timing attacks.

## Features

| Feature | Description |
|---------|-------------|
| `server` | Enables `A2aServer` with axum HTTP handler and bearer auth (requires `axum`, `blake3`, `tower`) |

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

MIT
