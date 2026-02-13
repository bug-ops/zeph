---
applyTo: "**/Cargo.toml"
---

# Cargo.toml Review

## Dependency Rules

- All versions defined in root `[workspace.dependencies]` — no version pinning in crate manifests
- Crates inherit via `{ workspace = true }` and add features locally
- Dependencies sorted alphabetically
- No features specified at workspace level — only in consuming crate
- Reject `openssl-sys` or any OpenSSL dependency — use `rustls` exclusively
- New dependencies must be checked against `deny.toml` license allowlist

## Workspace Lints

- Each crate must have `[lints] workspace = true`
- No crate-level lint overrides without justification
