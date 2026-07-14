# zeph-a2a Guide

Protocol client/server work for A2A lives here.

- Start with crate-local checks: `cargo build -p zeph-a2a`, `cargo nextest run -p zeph-a2a`, `cargo clippy -p zeph-a2a --all-targets -- -D warnings`.
- Read `specs/014-a2a/spec.md` before changing discovery, trust policy, or IBCT; honor its `## Key Invariants` and `NEVER` sections.
- Keep changes isolated to A2A transport, discovery, JSON-RPC, and server behavior unless a shared API truly requires cross-crate edits.
- Preserve protocol-facing behavior unless the task explicitly calls for a spec-aligned change.
- The trust anchor for `AgentCard` signature verification is only the operator-configured `[a2a_client].trusted_agent_keys` store — never a card-supplied `jku` URL (self-signed-forgery + SSRF risk).
- IBCT tokens (`ibct.rs`) are HMAC-signed bearer credentials scoped to `task_id` + endpoint origin — never log or dump a raw token, and keep `IbctKey`/`IbctKeyConfig` on hand-written `Debug`/`Serialize` impls that redact `key_hex`/`key_bytes`; a derived impl reopens the leak (#6005, #6165).
- `rate_limit_middleware` must wrap `auth_middleware`, never the reverse, in `build_router_with_full_config` — `auth_middleware` short-circuits with 401 without calling `next.run`, so if it sits outside the rate limiter, failed-auth requests bypass the per-IP counter entirely (#6136).
- An empty-string bearer/vault token must never be treated as "no auth configured" — normalize or reject it before hashing, never let it hash-match a missing header (#6282).
- If external behavior changes, update `crates/zeph-a2a/README.md` and the relevant docs in `docs/src/advanced/a2a.md`.
