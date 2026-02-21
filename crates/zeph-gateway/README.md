# zeph-gateway

[![Crates.io](https://img.shields.io/crates/v/zeph-gateway)](https://crates.io/crates/zeph-gateway)
[![docs.rs](https://img.shields.io/docsrs/zeph-gateway)](https://docs.rs/zeph-gateway)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue)](https://www.rust-lang.org)

HTTP gateway for webhook ingestion with bearer auth for Zeph.

## Overview

Exposes an axum 0.8 HTTP server that accepts incoming webhooks, validates bearer tokens, and forwards payloads into the agent loop. Includes a `/health` endpoint for liveness probes. Feature-gated behind `gateway`.

## Key Modules

- **server** — `GatewayServer` startup and graceful shutdown
- **handlers** — request handlers for webhook and health routes
- **router** — axum router construction with auth middleware
- **error** — `GatewayError` error types

## Installation

```bash
cargo add zeph-gateway
```

Enabled via the `gateway` feature flag on the root `zeph` crate.

## License

MIT
