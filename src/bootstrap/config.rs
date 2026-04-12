// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};

use crate::bootstrap::VaultArgs;
use zeph_core::config::Config;

pub fn resolve_config_path(cli_override: Option<&Path>) -> PathBuf {
    let cwd_default = Path::new("config/default.toml");
    resolve_config_path_impl(
        cli_override,
        |name| std::env::var(name).ok(),
        cwd_default.exists(),
    )
}

fn resolve_config_path_impl(
    cli_override: Option<&Path>,
    get_env: impl Fn(&str) -> Option<String>,
    cwd_default_exists: bool,
) -> PathBuf {
    if let Some(path) = cli_override {
        tracing::debug!("config resolved via CLI flag: {}", path.display());
        return path.to_owned();
    }
    if let Some(val) = get_env("ZEPH_CONFIG") {
        let path = PathBuf::from(&val);
        tracing::debug!(
            "config resolved via ZEPH_CONFIG env var: {}",
            path.display()
        );
        return path;
    }
    if cwd_default_exists {
        tracing::debug!("config resolved via CWD default: config/default.toml");
        return PathBuf::from("config/default.toml");
    }
    let xdg = dirs::config_dir()
        .unwrap_or_else(|| {
            get_env("HOME")
                .map_or_else(|| PathBuf::from("~"), PathBuf::from)
                .join(".config")
        })
        .join("zeph")
        .join("config.toml");
    tracing::debug!("config resolved via XDG fallback: {}", xdg.display());
    xdg
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
    let default_dir = zeph_core::vault::default_vault_dir();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn cli_override_takes_precedence() {
        let path = Path::new("/custom/config.toml");
        let result = resolve_config_path_impl(Some(path), no_env, false);
        assert_eq!(result, PathBuf::from("/custom/config.toml"));
    }

    #[test]
    fn env_var_used_when_no_cli() {
        let result = resolve_config_path_impl(
            None,
            |name| {
                if name == "ZEPH_CONFIG" {
                    Some("/env/config.toml".to_owned())
                } else {
                    None
                }
            },
            false,
        );
        assert_eq!(result, PathBuf::from("/env/config.toml"));
    }

    #[test]
    fn cwd_default_returned_when_exists() {
        let result = resolve_config_path_impl(None, no_env, true);
        assert_eq!(result, PathBuf::from("config/default.toml"));
    }

    #[test]
    fn xdg_fallback_path_constructed() {
        // dirs::config_dir() reads the real environment (HOME / XDG_CONFIG_HOME).
        // We only assert the path ends with the expected platform-independent suffix.
        let result = resolve_config_path_impl(None, no_env, false);
        assert!(
            result.ends_with("zeph/config.toml"),
            "unexpected path: {}",
            result.display()
        );
    }
}
