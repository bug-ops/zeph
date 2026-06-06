# zeph-vault Guide

Secret storage with pluggable backends and age encryption (`VaultProvider` trait, age backend, env backend for tests) lives here.

- Start with crate-local checks: `cargo build -p zeph-vault`, `cargo nextest run -p zeph-vault`, `cargo clippy -p zeph-vault --all-targets -- -D warnings`.
- Treat every change here as highest-sensitivity: this crate is the only authorized path for secret access in the workspace.
- `Secret<T>` values must never appear in logs, `Debug` output, error messages, or serialized payloads — audit any new `impl` that touches the inner value.
- The `env` backend is for testing only and must never be enabled in production configs (`ZEPH_VAULT_BACKEND=env` is forbidden outside test contexts).
- Do not add new secret resolution paths outside this crate; callers must go through `VaultProvider`.
