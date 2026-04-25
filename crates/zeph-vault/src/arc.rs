// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `Arc<RwLock<AgeVaultProvider>>` wrapper that implements [`VaultProvider`].
//!
//! Allows the age vault to be stored as `Box<dyn VaultProvider>` for trait-object use
//! while the inner `Arc` is separately accessible for mutable operations such as OAuth
//! credential persistence.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zeph_common::secret::VaultError;

use crate::{AgeVaultProvider, VaultProvider};

/// [`VaultProvider`] wrapper around `Arc<RwLock<AgeVaultProvider>>`.
///
/// Allows the age vault `Arc` to be stored as `Box<dyn VaultProvider>` while the
/// underlying `Arc<RwLock<AgeVaultProvider>>` is separately held for OAuth credential
/// persistence via `VaultCredentialStore`.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use tokio::sync::RwLock;
/// use zeph_vault::{AgeVaultProvider, ArcAgeVaultProvider, VaultProvider};
/// use std::path::Path;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let age = AgeVaultProvider::new(
///     Path::new("/etc/zeph/vault-key.txt"),
///     Path::new("/etc/zeph/secrets.age"),
/// )?;
/// let shared = Arc::new(RwLock::new(age));
/// let provider: Box<dyn VaultProvider> = Box::new(ArcAgeVaultProvider(Arc::clone(&shared)));
///
/// // Both `provider` and `shared` are usable concurrently.
/// let value = provider.get_secret("MY_KEY").await?;
/// # Ok(())
/// # }
/// ```
pub struct ArcAgeVaultProvider(pub Arc<tokio::sync::RwLock<AgeVaultProvider>>);

impl VaultProvider for ArcAgeVaultProvider {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>> {
        let arc = Arc::clone(&self.0);
        let key = key.to_owned();
        Box::pin(async move {
            let guard = arc.read().await;
            Ok(guard.get(&key).map(str::to_owned))
        })
    }

    fn list_keys(&self) -> Vec<String> {
        // block_in_place is required because list_keys is a sync trait method that may be called
        // from within a tokio async context (e.g. resolve_secrets). blocking_read() panics there.
        let arc = Arc::clone(&self.0);
        let guard = tokio::task::block_in_place(|| arc.blocking_read());
        let mut keys: Vec<String> = guard.list_keys().iter().map(|s| (*s).to_owned()).collect();
        keys.sort_unstable();
        keys
    }
}
