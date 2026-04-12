// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! OAuth credential store backed by Zeph's age vault.

use std::sync::Arc;

use rmcp::transport::auth::{AuthError, CredentialStore, StoredCredentials};
use tokio::sync::RwLock;

use zeph_core::vault::AgeVaultProvider;
use zeph_core::vault::VaultProvider as _;

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
    /// Key format: `ZEPH_MCP_OAUTH_{server_id.to_uppercase().replace('-', "_")}`.
    pub fn new(server_id: &str, vault: Arc<RwLock<AgeVaultProvider>>) -> Self {
        let normalized = server_id.to_uppercase().replace('-', "_");
        Self {
            vault_key: format!("ZEPH_MCP_OAUTH_{normalized}"),
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
        let guard = self.vault.read().await;
        let value = guard
            .get_secret(&self.vault_key)
            .await
            .map_err(|e| AuthError::InternalError(format!("vault read: {e}")))?;
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
            let mut guard = vault.blocking_write();
            guard.set_secret_mut(key, json);
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
            let mut guard = vault.blocking_write();
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
    #[test]
    fn vault_key_normalization_hyphen() {
        let key = format!(
            "ZEPH_MCP_OAUTH_{}",
            "my-server".to_uppercase().replace('-', "_")
        );
        assert_eq!(key, "ZEPH_MCP_OAUTH_MY_SERVER");
    }

    #[test]
    fn vault_key_collision_documented() {
        // "my-app" and "my_app" normalize to the same key — config validation must reject this.
        let a = "my-app".to_uppercase().replace('-', "_");
        let b = "my_app".to_uppercase().replace('-', "_");
        assert_eq!(
            a, b,
            "vault key collision exists for hyphens vs underscores"
        );
    }
}
