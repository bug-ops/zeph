// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use zeph_core::vault::AgeVaultProvider;

use crate::cli::VaultCommand;

pub(crate) fn default_vault_dir() -> PathBuf {
    zeph_core::vault::default_vault_dir()
}

pub(crate) fn handle_vault_command(
    cmd: VaultCommand,
    key_path: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let dir = default_vault_dir();
    let key_path_owned = key_path.map_or_else(|| dir.join("vault-key.txt"), PathBuf::from);
    let vault_path_owned = vault_path.map_or_else(|| dir.join("secrets.age"), PathBuf::from);

    match cmd {
        VaultCommand::Init => {
            AgeVaultProvider::init_vault(&dir)
                .map_err(|e| anyhow::anyhow!("vault init failed: {e}"))?;
        }
        VaultCommand::Set { key, value } => {
            let mut provider = AgeVaultProvider::load(&key_path_owned, &vault_path_owned)
                .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
            provider.set_secret_mut(key, value);
            provider
                .save()
                .map_err(|e| anyhow::anyhow!("failed to save vault: {e}"))?;
        }
        VaultCommand::Get { key } => {
            let provider = AgeVaultProvider::load(&key_path_owned, &vault_path_owned)
                .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
            if let Some(val) = provider.get(&key) {
                println!("{val}"); // lgtm[rust/cleartext-logging]
            } else {
                anyhow::bail!("key not found: {key}");
            }
        }
        VaultCommand::List => {
            let provider = AgeVaultProvider::load(&key_path_owned, &vault_path_owned)
                .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
            for key in provider.list_keys() {
                println!("{key}");
            }
        }
        VaultCommand::Rm { key } => {
            let mut provider = AgeVaultProvider::load(&key_path_owned, &vault_path_owned)
                .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
            if !provider.remove_secret_mut(&key) {
                anyhow::bail!("key not found: {key}");
            }
            provider
                .save()
                .map_err(|e| anyhow::anyhow!("failed to save vault: {e}"))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // R-02: default_vault_dir() env var code paths
    #[test]
    #[serial]
    fn default_vault_dir_xdg_config_home() {
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-test");
        }
        let dir = default_vault_dir();
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        assert_eq!(dir, PathBuf::from("/tmp/xdg-test/zeph"));
    }

    #[test]
    #[serial]
    fn default_vault_dir_appdata() {
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::set_var("APPDATA", "/tmp/appdata-test");
        }
        let dir = default_vault_dir();
        unsafe {
            std::env::remove_var("APPDATA");
        }
        assert_eq!(dir, PathBuf::from("/tmp/appdata-test/zeph"));
    }

    #[test]
    #[serial]
    fn default_vault_dir_home_fallback() {
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("APPDATA");
            std::env::set_var("HOME", "/tmp/home-test");
        }
        let dir = default_vault_dir();
        unsafe {
            std::env::remove_var("HOME");
        }
        assert_eq!(dir, PathBuf::from("/tmp/home-test/.config/zeph"));
    }

    // R-01: handle_vault_command() dispatch branches
    #[test]
    fn handle_vault_command_set_get_list_rm() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");

        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();

        handle_vault_command(
            VaultCommand::Set {
                key: "FOO".into(),
                value: "bar".into(),
            },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap();

        handle_vault_command(VaultCommand::List, Some(&key_path), Some(&vault_path)).unwrap();

        handle_vault_command(
            VaultCommand::Get { key: "FOO".into() },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap();

        handle_vault_command(
            VaultCommand::Rm { key: "FOO".into() },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap();
    }

    // R-04: Get/Rm missing-key error paths
    #[test]
    fn handle_vault_command_get_missing_key_errors() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();

        let err = handle_vault_command(
            VaultCommand::Get {
                key: "NONEXISTENT".into(),
            },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap_err();
        assert!(err.to_string().contains("key not found"));
    }

    #[test]
    fn handle_vault_command_rm_missing_key_errors() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();

        let err = handle_vault_command(
            VaultCommand::Rm {
                key: "NONEXISTENT".into(),
            },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap_err();
        assert!(err.to_string().contains("key not found"));
    }
}
