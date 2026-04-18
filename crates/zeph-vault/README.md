# zeph-vault

[![Crates.io](https://img.shields.io/crates/v/zeph-vault)](https://crates.io/crates/zeph-vault)
[![docs.rs](https://img.shields.io/docsrs/zeph-vault)](https://docs.rs/zeph-vault)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.94-blue)](https://www.rust-lang.org)

`VaultProvider` trait and backends (env, age) for Zeph secret management.

## Overview

Provides a unified interface for resolving secrets needed by the agent (API keys, tokens) without embedding them in the config file. Two backends ship out of the box: an environment-variable backend for simple deployments and an age-encrypted file backend for production use. All secret values are held as `Zeroizing<String>` — they are zeroed in memory on drop and never implement `Clone`.

## Key types

| Type | Description |
|------|-------------|
| `VaultProvider` | Trait: `get(key) -> Option<Zeroizing<String>>`, `set(key, value)`, `delete(key)`, `list_keys()`, `save()` |
| `EnvVaultProvider` | Reads secrets from environment variables; writes are no-ops |
| `AgeVaultProvider` | Reads/writes an age-encrypted JSON file (`~/.config/zeph/vault.age`) |
| `AnyVaultProvider` | Enum dispatch over all provider variants |
| `VaultError` | Typed error enum (`Io`, `Decrypt`, `Encrypt`, `Parse`, `KeyNotFound`) |
| `MockVaultProvider` | In-memory provider for tests (feature-gated: `mock`) |

## Usage

```rust
use zeph_vault::{AgeVaultProvider, VaultProvider};

// Open (or create) the age-encrypted vault
let mut vault = AgeVaultProvider::open("~/.config/zeph/vault.age")?;

// Store a secret
vault.set("ZEPH_CLAUDE_API_KEY", "sk-ant-...".into());
vault.save().await?;

// Retrieve a secret — returned as Zeroizing<String>
if let Some(key) = vault.get("ZEPH_CLAUDE_API_KEY") {
    println!("Key length: {}", key.len());
    // key is zeroed when dropped
}
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
backend = "age"                           # "env" or "age"
path = "~/.config/zeph/vault.age"         # only used by "age" backend
```

The `env` backend resolves secrets directly from environment variables — no file needed. Use `age` for production deployments where secrets must be stored on disk.

> [!IMPORTANT]
> Age-encrypted vault files are created with `0o600` permissions (owner read/write only), independent of the process umask, using `fs_secure` atomic write helpers from `zeph-common`. Ensure the key file (`~/.config/zeph/age_key`) is kept secure. Losing the key makes the vault unrecoverable. Run `zeph doctor` to verify vault file permissions at any time.

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

MIT
