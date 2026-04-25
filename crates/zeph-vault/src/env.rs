// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Environment-variable vault backend.
//!
//! [`EnvVaultProvider`] reads secrets directly from process environment variables.
//! Designed for quick local development and CI environments.

use std::future::Future;
use std::pin::Pin;

use zeph_common::secret::VaultError;

use crate::VaultProvider;

/// Vault backend that reads secrets from environment variables.
///
/// This backend is designed for quick local development and CI environments where injecting
/// environment variables is convenient. In production, prefer [`crate::AgeVaultProvider`].
///
/// [`get_secret`][VaultProvider::get_secret] reads any environment variable by name.
/// [`list_keys`][VaultProvider::list_keys] returns only variables whose names start with
/// `ZEPH_SECRET_`, preventing accidental exposure of unrelated process environment.
///
/// # Examples
///
/// ```no_run
/// use zeph_vault::{EnvVaultProvider, VaultProvider as _};
///
/// # async fn example() {
/// let vault = EnvVaultProvider;
/// // Returns None for variables that are not set.
/// let result = vault.get_secret("ZEPH_TEST_NONEXISTENT_99999").await.unwrap();
/// assert!(result.is_none());
/// # }
/// ```
pub struct EnvVaultProvider;

impl VaultProvider for EnvVaultProvider {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>> {
        let key = key.to_owned();
        Box::pin(async move { Ok(std::env::var(&key).ok()) })
    }

    fn list_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("ZEPH_SECRET_"))
            .map(|(k, _)| k)
            .collect();
        keys.sort_unstable();
        keys
    }
}
