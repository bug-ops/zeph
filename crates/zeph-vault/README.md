# zeph-vault

[![Crates.io](https://img.shields.io/crates/v/zeph-vault)](https://crates.io/crates/zeph-vault)
[![docs.rs](https://img.shields.io/docsrs/zeph-vault)](https://docs.rs/zeph-vault)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.97-blue)](https://www.rust-lang.org)

`VaultProvider` trait and backends (env, age) for Zeph secret management.

## Overview

Provides a unified interface for resolving secrets needed by the agent (API keys, tokens) without embedding them in the config file. Two backends ship out of the box: an environment-variable backend for simple deployments and an age-encrypted file backend for production use. In-memory secret values are held in `zeroize::Zeroizing` buffers, so they are overwritten on drop.

## Key types

| Type | Description |
|------|-------------|
| `VaultProvider` | Async trait: `get_secret(key) -> Result<Option<String>, VaultError>` plus a `list_keys()` default method. Implement it to integrate a custom store |
| `EnvVaultProvider` | Development/testing backend that reads secrets from `ZEPH_SECRET_`-prefixed environment variables |
| `AgeVaultProvider` | Primary backend: reads/writes an age-encrypted JSON file. Constructed with a key path and a vault path (`new`, `load`, `load_async`) |
| `ArcAgeVaultProvider` | `Arc<RwLock<AgeVaultProvider>>` wrapper implementing `VaultProvider`, so the age vault can be a trait object while still supporting mutable operations |
| `Secret` / `VaultError` | Re-exported from `zeph-common`; `VaultError` variants are `NotFound`, `Backend`, and `Io` |
| `MockVaultProvider` | In-memory provider for tests (feature-gated: `mock`) |

## Usage

```rust,no_run
use std::path::Path;
use zeph_vault::AgeVaultProvider;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
// Load the age identity + encrypted vault file.
let mut vault = AgeVaultProvider::new(
    Path::new("/etc/zeph/vault-key.txt"),
    Path::new("/etc/zeph/secrets.age"),
)?;

// Store a secret, then persist the re-encrypted vault to disk.
vault.set_secret_mut("ZEPH_CLAUDE_API_KEY".to_owned(), "sk-ant-...".to_owned());
vault.save()?;

// Retrieve a secret synchronously via the direct getter.
if let Some(key) = vault.get("ZEPH_CLAUDE_API_KEY") {
    println!("Key length: {}", key.len());
}
# Ok(())
# }
```

CLI usage:

```bash
zeph vault set ZEPH_CLAUDE_API_KEY sk-ant-...
zeph vault get ZEPH_CLAUDE_API_KEY
zeph vault list
zeph vault delete ZEPH_CLAUDE_API_KEY
```

## Configuration

```toml
[vault]
backend = "age"   # "env" or "age"; default is "env"
```

The `env` backend resolves `ZEPH_SECRET_`-prefixed secrets directly from environment variables — no file needed. Use `age` for production deployments where secrets must be stored on disk. The age backend keeps its identity and encrypted store under `~/.config/zeph/` (`vault-key.txt` and `secrets.age`).

> [!IMPORTANT]
> The age identity key file (`~/.config/zeph/vault-key.txt`) is created with Unix `0o600` permissions (owner read/write only). Vault writes are atomic — a temporary file is written and renamed, so a crash during write never corrupts `secrets.age`. Keep the key file secure: losing it makes the vault unrecoverable.

## Features

| Feature | Description |
|---------|-------------|
| `mock` | Enables `MockVaultProvider` for downstream crate tests |

## Installation

```bash
cargo add zeph-vault
```

## Documentation

Full documentation: <https://bug-ops.github.io/zeph/>

## License

Licensed under either of [MIT](../../LICENSE) or [Apache License, Version 2.0](../../LICENSE-APACHE) at your option.
