// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `Arc<RwLock<AgeVaultProvider>>` wrapper that implements [`VaultProvider`].
//!
//! Allows the age vault to be stored as `Box<dyn VaultProvider>` for trait-object use
//! while the inner `Arc` is separately accessible for mutable operations such as OAuth
//! credential persistence.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use zeph_common::secret::VaultError;

use crate::{AgeVaultProvider, VaultProvider};

/// [`VaultProvider`] wrapper around `Arc<RwLock<AgeVaultProvider>>`.
///
/// Uses `std::sync::RwLock` so that `list_keys()` — a synchronous trait method — can
/// acquire the lock without touching the async runtime. Calling `block_in_place` or
/// `Handle::current().block_on(...)` from a sync trait method panics when the caller is
/// already inside a `current_thread` runtime; a standard lock has no such restriction.
///
/// Write operations (OAuth credential persistence) call `vault.write().unwrap()` inside
/// `tokio::task::spawn_blocking` to keep blocking I/O off async threads.
///
/// # Examples
///
/// ```no_run
/// use std::sync::{Arc, RwLock};
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
pub struct ArcAgeVaultProvider(pub Arc<RwLock<AgeVaultProvider>>);

impl VaultProvider for ArcAgeVaultProvider {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>> {
        let value = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .map(str::to_owned);
        Box::pin(async move { Ok(value) })
    }

    fn list_keys(&self) -> Vec<String> {
        let guard = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut keys: Vec<String> = guard.list_keys().iter().map(|s| (*s).to_owned()).collect();
        keys.sort_unstable();
        keys
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use tempfile::tempdir;

    use crate::{AgeVaultProvider, VaultProvider as _};

    use super::ArcAgeVaultProvider;

    fn make_provider_with_keys(keys: &[(&str, &str)]) -> ArcAgeVaultProvider {
        let dir = tempdir().unwrap();
        AgeVaultProvider::init_vault(dir.path()).unwrap();
        let mut age = AgeVaultProvider::new(
            &dir.path().join("vault-key.txt"),
            &dir.path().join("secrets.age"),
        )
        .unwrap();
        for (k, v) in keys {
            age.set_secret_mut((*k).to_owned(), (*v).to_owned(), false)
                .unwrap();
        }
        // Keep tempdir alive by leaking — tests are short-lived, no I/O after this.
        std::mem::forget(dir);
        ArcAgeVaultProvider(Arc::new(RwLock::new(age)))
    }

    /// `list_keys()` must not panic when called directly from a `current_thread` runtime.
    #[tokio::test(flavor = "current_thread")]
    async fn list_keys_works_on_current_thread_runtime() {
        let provider = make_provider_with_keys(&[("ALPHA", "a"), ("BETA", "b")]);
        let keys = provider.list_keys();
        assert_eq!(keys, vec!["ALPHA".to_owned(), "BETA".to_owned()]);
    }

    /// `list_keys()` works correctly on the multi-thread runtime.
    #[tokio::test]
    async fn list_keys_works_on_multi_thread_runtime() {
        let provider = make_provider_with_keys(&[("Z_KEY", "z"), ("A_KEY", "a")]);
        let keys = provider.list_keys();
        assert_eq!(keys, vec!["A_KEY".to_owned(), "Z_KEY".to_owned()]);
    }

    /// `list_keys()` on empty vault returns empty vec.
    #[tokio::test(flavor = "current_thread")]
    async fn list_keys_empty_vault() {
        let provider = make_provider_with_keys(&[]);
        let keys = provider.list_keys();
        assert!(keys.is_empty());
    }

    /// `list_keys()` works correctly from `spawn_blocking` on the multi-thread runtime.
    #[tokio::test]
    async fn list_keys_works_via_spawn_blocking_on_multi_thread_runtime() {
        let provider = Arc::new(make_provider_with_keys(&[("Z_KEY", "z"), ("A_KEY", "a")]));
        let keys = tokio::task::spawn_blocking(move || provider.list_keys())
            .await
            .unwrap();
        assert_eq!(keys, vec!["A_KEY".to_owned(), "Z_KEY".to_owned()]);
    }

    /// `list_keys()` works correctly from `spawn_blocking` on `current_thread` runtime.
    #[tokio::test(flavor = "current_thread")]
    async fn list_keys_works_via_spawn_blocking_on_current_thread_runtime() {
        let provider = Arc::new(make_provider_with_keys(&[("ALPHA", "a"), ("BETA", "b")]));
        let keys = tokio::task::spawn_blocking(move || provider.list_keys())
            .await
            .unwrap();
        assert_eq!(keys, vec!["ALPHA".to_owned(), "BETA".to_owned()]);
    }

    /// `list_keys()` on empty vault returns empty vec (`spawn_blocking` variant).
    #[tokio::test(flavor = "current_thread")]
    async fn list_keys_empty_vault_via_spawn_blocking() {
        let provider = Arc::new(make_provider_with_keys(&[]));
        let keys = tokio::task::spawn_blocking(move || provider.list_keys())
            .await
            .unwrap();
        assert!(keys.is_empty());
    }

    /// `get_secret()` delegates to the inner `AgeVaultProvider`.
    #[tokio::test]
    async fn get_secret_delegates_to_inner() {
        let provider = make_provider_with_keys(&[("MY_SECRET", "secret_value")]);
        let result = provider.get_secret("MY_SECRET").await.unwrap();
        assert_eq!(result.as_deref(), Some("secret_value"));
    }

    /// `get_secret()` returns `None` for missing key.
    #[tokio::test]
    async fn get_secret_returns_none_for_missing() {
        let provider = make_provider_with_keys(&[]);
        let result = provider.get_secret("NONEXISTENT").await.unwrap();
        assert!(result.is_none());
    }
}
