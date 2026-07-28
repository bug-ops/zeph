// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `zeph store {get,put,list,delete}` — CLI surface for the cross-thread key-value store
//! (spec-080, #6363, FR-A-010/FR-A-011).

use crate::cli::StoreCommand;

pub(crate) async fn handle_store_command(
    cmd: StoreCommand,
    config_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use crate::bootstrap::{load_config_or_default, resolve_config_path};
    use zeph_memory::store::SqliteStore;

    let config_file = resolve_config_path(config_path);
    let config = load_config_or_default(&config_file)?;
    if !config.memory.store.enabled {
        anyhow::bail!(
            "cross-thread store is disabled ([memory.store].enabled = false in {}); \
             enable it before using `zeph store`",
            config_file.display()
        );
    }

    let sqlite = SqliteStore::new(crate::db_url::resolve_db_url(&config))
        .await
        .map_err(|e| anyhow::anyhow!("failed to open SQLite: {e}"))?;
    let max_value_bytes = config.memory.store.max_value_bytes;

    match cmd {
        StoreCommand::Get {
            namespace,
            key,
            owner_key,
        } => {
            match sqlite
                .store_get(&owner_key, &namespace, &key)
                .await
                .map_err(|e| anyhow::anyhow!("store get failed: {e}"))?
            {
                Some(item) => println!("{}", item.value),
                None => anyhow::bail!("no value found for {owner_key}/{namespace}/{key}"),
            }
        }
        StoreCommand::Put {
            namespace,
            key,
            value,
            owner_key,
            expected_version,
        } => {
            let item = sqlite
                .store_put(
                    &owner_key,
                    &namespace,
                    &key,
                    &value,
                    max_value_bytes,
                    expected_version,
                )
                .await
                .map_err(|e| anyhow::anyhow!("store put failed: {e}"))?;
            println!(
                "put {owner_key}/{namespace}/{key} -> version {}",
                item.version
            );
        }
        StoreCommand::List {
            namespace_prefix,
            owner_key,
            limit,
        } => {
            let items = sqlite
                .store_list(&owner_key, &namespace_prefix, limit)
                .await
                .map_err(|e| anyhow::anyhow!("store list failed: {e}"))?;
            if items.is_empty() {
                println!("(no rows)");
            } else {
                for item in items {
                    println!(
                        "{}/{} = {} (v{}, updated_at={})",
                        item.namespace, item.key, item.value, item.version, item.updated_at
                    );
                }
            }
        }
        StoreCommand::Delete {
            namespace,
            key,
            owner_key,
        } => {
            let deleted = sqlite
                .store_delete(&owner_key, &namespace, &key)
                .await
                .map_err(|e| anyhow::anyhow!("store delete failed: {e}"))?;
            if deleted {
                println!("deleted {owner_key}/{namespace}/{key}");
            } else {
                anyhow::bail!("no value found for {owner_key}/{namespace}/{key}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(
        dir: &std::path::Path,
        mutate: impl FnOnce(&mut zeph_core::config::Config),
    ) -> std::path::PathBuf {
        let mut config = zeph_core::config::Config::default();
        config.memory.sqlite_path = dir.join("zeph.db").display().to_string();
        mutate(&mut config);
        let toml = toml::to_string_pretty(&config).expect("serialize config");
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, toml).expect("write config");
        config_path
    }

    fn config_with_store_enabled() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path(), |config| {
            config.memory.store.enabled = true;
        });
        (dir, config_path)
    }

    #[tokio::test]
    async fn put_get_list_delete_roundtrip() {
        let (_dir, config_path) = config_with_store_enabled();

        handle_store_command(
            StoreCommand::Put {
                namespace: "orch/g1".into(),
                key: "finding".into(),
                value: "{\"x\":1}".into(),
                owner_key: "local".into(),
                expected_version: None,
            },
            Some(&config_path),
        )
        .await
        .unwrap();

        handle_store_command(
            StoreCommand::Get {
                namespace: "orch/g1".into(),
                key: "finding".into(),
                owner_key: "local".into(),
            },
            Some(&config_path),
        )
        .await
        .unwrap();

        handle_store_command(
            StoreCommand::List {
                namespace_prefix: "orch/".into(),
                owner_key: "local".into(),
                limit: 0,
            },
            Some(&config_path),
        )
        .await
        .unwrap();

        handle_store_command(
            StoreCommand::Delete {
                namespace: "orch/g1".into(),
                key: "finding".into(),
                owner_key: "local".into(),
            },
            Some(&config_path),
        )
        .await
        .unwrap();

        let err = handle_store_command(
            StoreCommand::Get {
                namespace: "orch/g1".into(),
                key: "finding".into(),
                owner_key: "local".into(),
            },
            Some(&config_path),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no value found"));
    }

    #[tokio::test]
    async fn get_missing_key_errors() {
        let (_dir, config_path) = config_with_store_enabled();

        let err = handle_store_command(
            StoreCommand::Get {
                namespace: "ns".into(),
                key: "no-such".into(),
                owner_key: "local".into(),
            },
            Some(&config_path),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no value found"));
    }

    #[tokio::test]
    async fn delete_missing_key_errors() {
        let (_dir, config_path) = config_with_store_enabled();

        let err = handle_store_command(
            StoreCommand::Delete {
                namespace: "ns".into(),
                key: "no-such".into(),
                owner_key: "local".into(),
            },
            Some(&config_path),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no value found"));
    }

    #[tokio::test]
    async fn disabled_store_rejects_every_subcommand() {
        let dir = tempfile::tempdir().unwrap();
        // `enabled` already defaults to `false` on `CrossThreadStoreConfig::default()` —
        // no mutation needed.
        let config_path = write_config(dir.path(), |_| {});

        let err = handle_store_command(
            StoreCommand::Get {
                namespace: "ns".into(),
                key: "k".into(),
                owner_key: "local".into(),
            },
            Some(&config_path),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("disabled"));
    }

    #[tokio::test]
    async fn owner_key_isolation_via_cli() {
        let (_dir, config_path) = config_with_store_enabled();

        handle_store_command(
            StoreCommand::Put {
                namespace: "ns".into(),
                key: "k".into(),
                value: "owner-a-value".into(),
                owner_key: "owner-a".into(),
                expected_version: None,
            },
            Some(&config_path),
        )
        .await
        .unwrap();

        let err = handle_store_command(
            StoreCommand::Get {
                namespace: "ns".into(),
                key: "k".into(),
                owner_key: "owner-b".into(),
            },
            Some(&config_path),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no value found"));
    }
}
