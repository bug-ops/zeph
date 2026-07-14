# zeph-config Guide

Configuration structs, TOML loader, `ZEPH_*` env var overrides, validation, and migration helpers live here. Vault secret resolution is handled upstream in `zeph-core`.

- Start with crate-local checks: `cargo build -p zeph-config`, `cargo nextest run -p zeph-config`, `cargo clippy -p zeph-config --all-targets -- -D warnings`.
- `ZEPH_*` env var overrides are for non-secret values only — secrets are resolved from the age vault, never from env vars or config files.
- When adding, renaming, or removing config keys, add a `--migrate-config` migration step so existing configs upgrade (or drop leftover keys with a warning) automatically — see #6218 for the removal case.
- Keep config structs, `config/default.toml`, docs, and the `--init` wizard in sync for every new field.
- If the config surface changes, update `crates/zeph-config/README.md` and `docs/src/` reference pages.
- Any config struct with a secret-shaped field (API key, token, credential) must hand-write `Debug` (and audit `Serialize`) to redact it — plain `#[derive(Debug)]` has leaked plaintext secrets into logs/panics repeatedly (#6004, #6005, #6165, #6173); never add a new secret field without a redaction test.
- New optional subsystems that touch the network or vault (e.g. the skill/plugin registry marketplace, #5910) must default fully off — zero network or vault access unless explicitly enabled in config.
