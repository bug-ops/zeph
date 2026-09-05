// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! OAuth credential store backed by Zeph's age vault.

use std::sync::{Arc, RwLock};

use rmcp::transport::auth::{AuthError, CredentialStore, StoredCredentials};

use zeph_core::vault::AgeVaultProvider;

/// `CredentialStore` backed by Zeph's age vault.
///
/// Vault key naming: `ZEPH_MCP_OAUTH_{SERVER_ID}` (uppercased, hyphens → underscores).
/// Value: JSON-serialized `StoredCredentials`.
///
/// Uses `Arc<RwLock<AgeVaultProvider>>` directly because saving requires `&mut self`
/// (`set_secret_mut` + `save`), and the `VaultProvider` trait only exposes `&self`.
pub struct VaultCredentialStore {
    vault_key: String,
    vault: Arc<RwLock<AgeVaultProvider>>,
}

impl VaultCredentialStore {
    /// Derive vault key and create the store.
    ///
    /// Key format: `ZEPH_MCP_OAUTH_{server_id.to_uppercase().replace('-', "_")}`, derived via
    /// [`zeph_config::oauth_vault_key`] — the same helper `Config::validate_mcp_servers` uses
    /// to detect a collision, so the two can never drift apart.
    pub fn new(server_id: &str, vault: Arc<RwLock<AgeVaultProvider>>) -> Self {
        Self {
            vault_key: zeph_config::oauth_vault_key(server_id),
            vault,
        }
    }

    /// Return the vault key this store uses.
    #[must_use]
    #[allow(dead_code)]
    pub fn vault_key(&self) -> &str {
        &self.vault_key
    }
}

#[async_trait::async_trait]
impl CredentialStore for VaultCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let value = self
            .vault
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.vault_key)
            .map(str::to_owned);
        match value {
            None => Ok(None),
            Some(json) => {
                let creds: StoredCredentials = serde_json::from_str(&json)
                    .map_err(|e| AuthError::InternalError(format!("vault deserialize: {e}")))?;
                Ok(Some(creds))
            }
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let json = serde_json::to_string(&credentials)
            .map_err(|e| AuthError::InternalError(format!("vault serialize: {e}")))?;
        let vault = Arc::clone(&self.vault);
        let key = self.vault_key.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = vault
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // OAuth credential refresh is an intentional update of the store's own managed
            // entry, not a user-facing secret set — always overwrite.
            guard
                .set_secret_mut(key, json, true)
                .map_err(|e| AuthError::InternalError(format!("vault save: {e}")))?;
            guard
                .save()
                .map_err(|e| AuthError::InternalError(format!("vault save: {e}")))
        })
        .await
        .map_err(|e| AuthError::InternalError(format!("spawn_blocking: {e}")))?
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let vault = Arc::clone(&self.vault);
        let key = self.vault_key.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = vault
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.remove_secret_mut(&key);
            guard
                .save()
                .map_err(|e| AuthError::InternalError(format!("vault clear: {e}")))
        })
        .await
        .map_err(|e| AuthError::InternalError(format!("spawn_blocking: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use zeph_config::oauth_vault_key;

    #[test]
    fn vault_key_normalization_hyphen() {
        assert_eq!(oauth_vault_key("my-server"), "ZEPH_MCP_OAUTH_MY_SERVER");
    }

    #[test]
    fn vault_key_collision_documented() {
        // "my-app" and "my_app" normalize to the same key via the shared helper —
        // `Config::validate_mcp_servers` rejects this at config-load time (see
        // zeph-config's `validate_mcp_servers_rejects_oauth_vault_key_collision` test).
        assert_eq!(oauth_vault_key("my-app"), oauth_vault_key("my_app"));
    }
}
