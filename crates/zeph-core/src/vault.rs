use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;

/// Wrapper for sensitive strings with redacted Debug/Display.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Pluggable secret retrieval backend.
pub trait VaultProvider: Send + Sync {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send + '_>>;
}

/// MVP vault backend that reads secrets from environment variables.
pub struct EnvVaultProvider;

impl VaultProvider for EnvVaultProvider {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send + '_>> {
        let key = key.to_owned();
        Box::pin(async move { Ok(std::env::var(&key).ok()) })
    }
}

/// Test helper with HashMap-based secret storage.
#[cfg(test)]
#[derive(Default)]
pub struct MockVaultProvider {
    secrets: std::collections::HashMap<String, String>,
}

#[cfg(test)]
impl MockVaultProvider {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_secret(mut self, key: &str, value: &str) -> Self {
        self.secrets.insert(key.to_owned(), value.to_owned());
        self
    }
}

#[cfg(test)]
impl VaultProvider for MockVaultProvider {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send + '_>> {
        let result = self.secrets.get(key).cloned();
        Box::pin(async move { Ok(result) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_expose_returns_inner() {
        let secret = Secret::new("my-api-key");
        assert_eq!(secret.expose(), "my-api-key");
    }

    #[test]
    fn secret_debug_is_redacted() {
        let secret = Secret::new("my-api-key");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
    }

    #[test]
    fn secret_display_is_redacted() {
        let secret = Secret::new("my-api-key");
        assert_eq!(format!("{secret}"), "[REDACTED]");
    }

    #[tokio::test]
    async fn env_vault_returns_set_var() {
        let key = "ZEPH_TEST_VAULT_SECRET_SET";
        unsafe { std::env::set_var(key, "test-value") };
        let vault = EnvVaultProvider;
        let result = vault.get_secret(key).await.unwrap();
        unsafe { std::env::remove_var(key) };
        assert_eq!(result.as_deref(), Some("test-value"));
    }

    #[tokio::test]
    async fn env_vault_returns_none_for_unset() {
        let vault = EnvVaultProvider;
        let result = vault
            .get_secret("ZEPH_TEST_VAULT_NONEXISTENT_KEY_12345")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn mock_vault_returns_configured_secret() {
        let vault = MockVaultProvider::new().with_secret("API_KEY", "secret-123");
        let result = vault.get_secret("API_KEY").await.unwrap();
        assert_eq!(result.as_deref(), Some("secret-123"));
    }

    #[tokio::test]
    async fn mock_vault_returns_none_for_missing() {
        let vault = MockVaultProvider::new();
        let result = vault.get_secret("MISSING").await.unwrap();
        assert!(result.is_none());
    }
}
