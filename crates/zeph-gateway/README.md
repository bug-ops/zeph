# zeph-gateway

HTTP gateway for webhook ingestion with bearer auth and health endpoint.

## Overview

Exposes an axum 0.8 HTTP server that accepts incoming webhooks, validates bearer tokens, and forwards payloads into the agent loop. Includes a `/health` endpoint for liveness probes. Feature-gated behind `gateway`.

## Key Modules

- **server** — `GatewayServer` startup and graceful shutdown
- **handlers** — request handlers for webhook and health routes
- **router** — axum router construction with auth middleware
- **error** — `GatewayError` error types

## Usage

```toml
# Cargo.toml (workspace root)
zeph-gateway = { path = "crates/zeph-gateway" }
```

Enabled via the `gateway` feature flag on the root `zeph` crate.

## License

MIT
