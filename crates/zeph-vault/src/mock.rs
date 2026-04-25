// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! In-memory vault backend for tests and mocking.
//!
//! Available when the `mock` feature is enabled or in `#[cfg(test)]` contexts.

use std::future::Future;
use std::pin::Pin;

use zeph_common::secret::VaultError;

use crate::VaultProvider;

/// In-memory vault backend for tests and mocking.
///
/// Available when the `mock` feature is enabled or in `#[cfg(test)]` contexts.
///
/// Secrets are stored in a plain `BTreeMap`. An additional `listed_only` list allows tests
/// to simulate keys that appear in [`list_keys`][VaultProvider::list_keys] but for which
/// [`get_secret`][VaultProvider::get_secret] returns `None` (e.g. to test missing-key
/// handling in callers that enumerate keys before fetching).
///
/// # Examples
///
/// ```no_run
/// use zeph_vault::{MockVaultProvider, VaultProvider as _};
///
/// # #[tokio::main]
/// # async fn example() {
/// let vault = MockVaultProvider::new()
///     .with_secret("API_KEY", "sk-test-123")
///     .with_listed_key("GHOST_KEY");
///
/// let val = vault.get_secret("API_KEY").await.unwrap();
/// assert_eq!(val.as_deref(), Some("sk-test-123"));
///
/// // GHOST_KEY appears in list_keys() but get_secret returns None
/// assert!(vault.list_keys().contains(&"GHOST_KEY".to_owned()));
/// let ghost = vault.get_secret("GHOST_KEY").await.unwrap();
/// assert!(ghost.is_none());
/// # }
/// ```
#[derive(Default)]
pub struct MockVaultProvider {
    secrets: std::collections::BTreeMap<String, String>,
    /// Keys returned by `list_keys()` but absent from secrets (simulates `get_secret` returning
    /// `None`).
    listed_only: Vec<String>,
}

impl MockVaultProvider {
    /// Create a new empty mock vault.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_vault::{MockVaultProvider, VaultProvider as _};
    ///
    /// let vault = MockVaultProvider::new();
    /// assert!(vault.list_keys().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a secret key-value pair to the mock vault.
    ///
    /// Follows the builder pattern so calls can be chained.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_vault::{MockVaultProvider, VaultProvider as _};
    ///
    /// let vault = MockVaultProvider::new()
    ///     .with_secret("A", "alpha")
    ///     .with_secret("B", "beta");
    /// assert!(vault.list_keys().contains(&"A".to_owned()));
    /// assert!(vault.list_keys().contains(&"B".to_owned()));
    /// ```
    #[must_use]
    pub fn with_secret(mut self, key: &str, value: &str) -> Self {
        self.secrets.insert(key.to_owned(), value.to_owned());
        self
    }

    /// Add a key to `list_keys()` without a corresponding `get_secret()` value.
    ///
    /// Useful for testing callers that enumerate keys before fetching values — allows
    /// simulation of race conditions or partially-visible key sets.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_vault::{MockVaultProvider, VaultProvider as _};
    ///
    /// let vault = MockVaultProvider::new().with_listed_key("PHANTOM");
    /// // PHANTOM is enumerable but has no stored value.
    /// assert!(vault.list_keys().contains(&"PHANTOM".to_owned()));
    /// ```
    #[must_use]
    pub fn with_listed_key(mut self, key: &str) -> Self {
        self.listed_only.push(key.to_owned());
        self
    }
}

impl VaultProvider for MockVaultProvider {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>> {
        let result = self.secrets.get(key).cloned();
        Box::pin(async move { Ok(result) })
    }

    fn list_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .secrets
            .keys()
            .cloned()
            .chain(self.listed_only.iter().cloned())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }
}
