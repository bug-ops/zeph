# zeph-config Guide

Configuration structs, TOML loader, `ZEPH_*` env var overrides, validation, and migration helpers live here. Vault secret resolution is handled upstream in `zeph-core`.

- Start with crate-local checks: `cargo build -p zeph-config`, `cargo nextest run -p zeph-config`, `cargo clippy -p zeph-config --all-targets -- -D warnings`.
- `ZEPH_*` env var overrides are for non-secret values only — secrets are resolved from the age vault, never from env vars or config files.
- When adding or renaming config keys, add a `--migrate-config` migration step so existing configs upgrade automatically.
- Keep config structs, `config/default.toml`, docs, and the `--init` wizard in sync for every new field.
- If the config surface changes, update `crates/zeph-config/README.md` and `docs/src/` reference pages.
