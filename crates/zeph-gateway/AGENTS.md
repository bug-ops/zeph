# zeph-gateway Guide

HTTP gateway and webhook ingestion code lives here.

- Start with crate-local checks: `cargo build -p zeph-gateway`, `cargo nextest run -p zeph-gateway`, `cargo clippy -p zeph-gateway --all-targets -- -D warnings`.
- Read `specs/019-gateway/spec.md` before changing routing, auth, or rate-limiting; honor its `## Key Invariants` section.
- Treat auth, request validation, limits, and tracing as security-sensitive behavior.
- Bearer tokens and webhook secrets are resolved exclusively from the age vault — never hardcoded or passed via env vars.
- `rate_limit_middleware` must wrap `auth_middleware` in `build_router`, never the reverse — `auth_middleware` short-circuits with 401 without calling `next.run`, so if it sits outside the rate limiter, failed-auth requests bypass the per-IP counter entirely and the bearer token becomes brute-forceable (#6136).
- An empty-string bearer token must never be treated as "no auth configured" — normalize or reject it before hashing, never let it hash-match a missing header (#6282).
- `/webhook` and any other trust-boundary entry point must check `zeph_commands::is_recognized_command` on the raw body before sanitizing, and `GatewayChannel::supports_exit()` must stay `false` for webhook-sourced turns — webhook input is untrusted and must go through the same `requires_auth`/trusted dispatch gate as other channels, not be treated as CLI/TUI-trusted (#6039).
- Keep gateway behavior aligned with root CLI/config surfaces.
- If external behavior changes, update `crates/zeph-gateway/README.md` and `docs/src/advanced/gateway.md`.
