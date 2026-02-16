# HTTP Gateway

The HTTP gateway exposes a webhook endpoint for external services to send messages into Zeph. It provides bearer token authentication, per-IP rate limiting, body size limits, and a health check endpoint.

## Feature Flag

Enable with `--features gateway` at build time:

```bash
cargo build --release --features gateway
```

## Configuration

Add the `[gateway]` section to `config/default.toml`:

```toml
[gateway]
enabled = true
bind = "127.0.0.1"
port = 8090
# auth_token = "secret"  # optional, from vault ZEPH_GATEWAY_TOKEN
rate_limit = 120          # max requests/minute per IP (0 = unlimited)
max_body_size = 1048576   # 1 MB
```

Set `bind = "0.0.0.0"` to accept connections from all interfaces. The gateway logs a warning when binding to `0.0.0.0` to prevent accidental exposure.

### Authentication

When `auth_token` is set (or resolved from vault via `ZEPH_GATEWAY_TOKEN`), all requests to `/webhook` must include a bearer token:

```
Authorization: Bearer <token>
```

Token comparison uses constant-time hashing (blake3 + `subtle`) to prevent timing attacks. The `/health` endpoint is always unauthenticated.

## Endpoints

### `GET /health`

Returns the gateway status and uptime. No authentication required.

```json
{
  "status": "ok",
  "uptime_secs": 3600
}
```

### `POST /webhook`

Accepts a JSON payload and forwards it to the agent loop.

```json
{
  "channel": "discord",
  "sender": "user1",
  "body": "hello from webhook"
}
```

On success, returns `200` with `{"status": "accepted"}`. Returns `401` if the token is missing or invalid, `429` if rate-limited, and `413` if the body exceeds `max_body_size`.

## Rate Limiting

The gateway tracks requests per source IP with a 60-second sliding window. When a client exceeds the configured `rate_limit`, subsequent requests receive `429 Too Many Requests` until the window resets. The rate limiter evicts stale entries when the tracking map exceeds 10,000 IPs.

## Architecture

The gateway is built on [axum](https://docs.rs/axum) with `tower-http` middleware:

- **Auth middleware** -- validates bearer tokens on protected routes
- **Rate limit middleware** -- per-IP counters with automatic eviction
- **Body limit layer** -- `tower_http::limit::RequestBodyLimitLayer`
- **Graceful shutdown** -- listens on the global `watch::Receiver<bool>` shutdown signal
