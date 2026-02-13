---
applyTo: "**/Cargo.toml"
---

# Cargo.toml Review

## Dependency Rules

- All versions defined in root `[workspace.dependencies]` — no version pinning in crate manifests
- Crates inherit via `{ workspace = true }` and normally add features locally
- Prefer dependencies sorted alphabetically where practical
- Prefer enabling features in consuming crates; only specify features in `[workspace.dependencies]` when a dependency is shared across multiple crates and must use a single, consistent feature set (e.g., `hf-hub`, `ollama-rs`, `teloxide`, `tokenizers`, `tracing-subscriber`)
- Reject `openssl-sys` or any OpenSSL dependency — use `rustls` exclusively
- New dependencies must be checked against `deny.toml` license allowlist

## Workspace Lints

- Each crate must have `[lints] workspace = true`
- No crate-level lint overrides without justification
