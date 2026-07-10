# zeph-gateway

[![Crates.io](https://img.shields.io/crates/v/zeph-gateway)](https://crates.io/crates/zeph-gateway)
[![docs.rs](https://img.shields.io/docsrs/zeph-gateway)](https://docs.rs/zeph-gateway)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.96-blue)](https://www.rust-lang.org)

HTTP gateway for webhook ingestion with bearer auth for Zeph.

## Overview

Exposes an axum 0.8 HTTP server that accepts incoming webhooks, validates bearer tokens, and forwards payloads into the agent loop. Includes a `/health` endpoint for liveness probes. Feature-gated behind `gateway`.

## Key Modules

- **server** — `GatewayServer` startup and graceful shutdown
- **handlers** — request handlers for webhook and health routes
- **router** — axum router construction with auth middleware
- **error** — `GatewayError` error types

## Activation

`GatewayServer` starts automatically in daemon mode when the `gateway` feature is enabled and `[gateway]` is configured:

```toml
[gateway]
bind = "0.0.0.0:8090"
auth_token = "your-secret-token"   # optional, see authentication below
```

```bash
cargo run --features gateway -- --daemon   # starts agent + gateway server
```

The gateway is wired via `src/gateway_spawn.rs` into both `daemon.rs` and `runner.rs`. A background drain task logs incoming webhook payloads; agent loopback forwarding is a planned follow-up.

## Authentication

`GatewayServer` supports bearer token authentication via the `with_auth()` builder method. When `auth_token` is `None`, the server emits a `tracing::warn!` at startup indicating that the endpoint is unauthenticated.

```rust
use tokio::sync::{mpsc, watch};
use zeph_gateway::GatewayServer;

let (webhook_tx, _webhook_rx) = mpsc::channel::<String>(64);
let (_shutdown_tx, shutdown_rx) = watch::channel(false);

GatewayServer::new("127.0.0.1", 8080, webhook_tx, shutdown_rx)
    .with_auth(Some("secret-token".to_string()))
    .with_rate_limit(120)   // requests per 60s window per IP; 0 disables
    .serve()
    .await?;
```

Token comparison uses BLAKE3 + `subtle::ConstantTimeEq` to prevent timing attacks.

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `prometheus` | — | Exposes a Prometheus metrics endpoint via `prometheus-client` |

## Installation

```bash
cargo add zeph-gateway
```

At the application level the server is activated via the `gateway` feature flag on the root `zeph` crate.

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
