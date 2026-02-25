// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use crate::bootstrap::VaultArgs;
use crate::config::Config;

pub fn resolve_config_path(cli_override: Option<&Path>) -> PathBuf {
    if let Some(path) = cli_override {
        return path.to_owned();
    }
    if let Ok(path) = std::env::var("ZEPH_CONFIG") {
        return PathBuf::from(path);
    }
    PathBuf::from("config/default.toml")
}

/// Priority: CLI flag > `ZEPH_VAULT_*` env > config.vault.* > defaults
pub fn parse_vault_args(
    config: &Config,
    cli_backend: Option<&str>,
    cli_key_path: Option<&Path>,
    cli_vault_path: Option<&Path>,
) -> VaultArgs {
    let env_backend = std::env::var("ZEPH_VAULT_BACKEND").ok();
    let backend = cli_backend
        .map(String::from)
        .or(env_backend)
        .unwrap_or_else(|| config.vault.backend.clone());

    let env_key = std::env::var("ZEPH_VAULT_KEY").ok();
    let default_dir = crate::vault::default_vault_dir();
    let key_path = cli_key_path
        .map(|p| p.to_string_lossy().into_owned())
        .or(env_key)
        .or_else(|| {
            if backend == "age" {
                Some(
                    default_dir
                        .join("vault-key.txt")
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                None
            }
        });

    let env_vault = std::env::var("ZEPH_VAULT_PATH").ok();
    let vault_path = cli_vault_path
        .map(|p| p.to_string_lossy().into_owned())
        .or(env_vault)
        .or_else(|| {
            if backend == "age" {
                Some(
                    default_dir
                        .join("secrets.age")
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                None
            }
        });

    VaultArgs {
        backend,
        key_path,
        vault_path,
    }
}
