# zeph-gateway Guide

HTTP gateway and webhook ingestion code lives here.

- Start with crate-local checks: `cargo build -p zeph-gateway`, `cargo nextest run -p zeph-gateway`, `cargo clippy -p zeph-gateway --all-targets -- -D warnings`.
- Treat auth, request validation, limits, and tracing as security-sensitive behavior.
- Bearer tokens and webhook secrets are resolved exclusively from the age vault — never hardcoded or passed via env vars.
- Keep gateway behavior aligned with root CLI/config surfaces.
- If external behavior changes, update `crates/zeph-gateway/README.md` and `docs/src/advanced/gateway.md`.
