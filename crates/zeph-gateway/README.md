# zeph-gateway

[![Crates.io](https://img.shields.io/crates/v/zeph-gateway)](https://crates.io/crates/zeph-gateway)
[![docs.rs](https://img.shields.io/docsrs/zeph-gateway)](https://docs.rs/zeph-gateway)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.98-blue)](https://www.rust-lang.org)

HTTP gateway for webhook ingestion with bearer auth for Zeph.

## Overview

Exposes an axum 0.8 HTTP server that accepts incoming webhooks, validates bearer tokens, and forwards payloads into the agent loop. Includes a `/health` endpoint for liveness probes. Feature-gated behind `gateway`.

## Key Modules

- **server** — `GatewayServer` startup and graceful shutdown
- **handlers** — request handlers for webhook and health routes; `WebhookMessage` (the
  `{ sender, channel, body }` payload forwarded into the agent)
- **router** — axum router construction with auth middleware
- **error** — `GatewayError` error types

**Public API:** `GatewayServer`, `WebhookMessage`, `GatewayError`.

Endpoints:

| Endpoint | Method | Auth required | Purpose |
|---|---|---|---|
| `/health` | GET | No | Liveness check; returns uptime in seconds |
| `/webhook` | POST | Yes | Ingest external events into the agent |

## Activation

`GatewayServer` starts automatically in daemon mode when the `gateway` feature is enabled and `[gateway]` is configured:

```toml
[gateway]
bind = "0.0.0.0:8090"
auth_token = "your-secret-token"   # mandatory, see authentication below
```

```bash
cargo run --features gateway -- --daemon   # starts agent + gateway server
```

The gateway is wired via `src/gateway_spawn.rs` into both `daemon.rs` and `runner.rs`. A background `forward_webhooks` task drains incoming webhook payloads and forwards each one into the agent's input queue as a `ChannelMessage`: a payload recognized as a known slash command is forwarded as-is (subject to the same `CommandHandler::requires_auth` authorization as any other channel); every other payload is sanitized via `ContentSanitizer` (classified `ExternalUntrusted`) before it reaches the agent loop, since a valid bearer token proves only that the sender knows the shared secret, not that the content is safe.

## Authentication

`GatewayServer` requires bearer token authentication, configured via the `with_auth()` builder method.

```rust,no_run
use tokio::sync::{mpsc, watch};
use zeph_gateway::{GatewayServer, WebhookMessage};

let (webhook_tx, _webhook_rx) = mpsc::channel::<WebhookMessage>(64);
let (_shutdown_tx, shutdown_rx) = watch::channel(false);

GatewayServer::new("127.0.0.1", 8080, webhook_tx, shutdown_rx)
    .with_auth(Some("secret-token".to_string()))
    .with_rate_limit(120)              // requests per 60s window per IP; 0 disables
    .with_max_body_size(1_048_576)     // reject larger POST /webhook bodies
    .with_webhook_timeout(std::time::Duration::from_secs(5))   // else 503 Service Unavailable
    .with_trusted_proxy_cidrs(vec!["10.0.0.0/8".to_string()])  // rate-limit by X-Forwarded-For
    .serve()
    .await?;
```

Middleware order is body-size limit → auth → rate limiting. When `with_trusted_proxy_cidrs` is non-empty, the rate limiter resolves the real client IP from `X-Forwarded-For` using the rightmost-untrusted algorithm; otherwise it keys on the raw TCP peer address.

> [!IMPORTANT]
> A bearer token is mandatory. `serve()` refuses to start and returns `GatewayError::MissingAuthToken` when no non-empty token is configured, since `/webhook` forwards its body directly into the agent's turn loop.

Token comparison uses BLAKE3 + `subtle::ConstantTimeEq` to prevent timing attacks. The rate limiter wraps the auth check (not the reverse), so requests with a missing or invalid bearer token still count against the per-IP limit — a brute-force attempt against the token cannot bypass rate limiting.

With the `prometheus` feature, `with_metrics_registry(registry, path)` mounts an extra route that renders the registry as OpenMetrics 1.0.0 text. That endpoint is unauthenticated and bypasses rate limiting — do not expose it publicly.

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
