use std::fmt;
use std::future::Future;
use std::pin::Pin;

#[cfg(feature = "vault-age")]
use std::collections::HashMap;
#[cfg(feature = "vault-age")]
use std::io::Read as _;
#[cfg(feature = "vault-age")]
use std::path::Path;

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

#[cfg(feature = "vault-age")]
#[derive(Debug, thiserror::Error)]
pub enum AgeVaultError {
    #[error("failed to read key file: {0}")]
    KeyRead(std::io::Error),
    #[error("failed to parse age identity: {0}")]
    KeyParse(String),
    #[error("failed to read vault file: {0}")]
    VaultRead(std::io::Error),
    #[error("age decryption failed: {0}")]
    Decrypt(age::DecryptError),
    #[error("I/O error during decryption: {0}")]
    Io(std::io::Error),
    #[error("invalid JSON in vault: {0}")]
    Json(serde_json::Error),
}

#[cfg(feature = "vault-age")]
pub struct AgeVaultProvider {
    secrets: HashMap<String, String>,
}

#[cfg(feature = "vault-age")]
impl fmt::Debug for AgeVaultProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgeVaultProvider")
            .field("secrets", &format_args!("[{} secrets]", self.secrets.len()))
            .finish()
    }
}

#[cfg(feature = "vault-age")]
impl AgeVaultProvider {
    /// Decrypt an age-encrypted JSON secrets file.
    ///
    /// `key_path` — path to the age identity (private key) file.
    /// `vault_path` — path to the age-encrypted JSON file.
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on key/vault read failure, parse error, or decryption failure.
    pub fn new(key_path: &Path, vault_path: &Path) -> Result<Self, AgeVaultError> {
        let key_str = std::fs::read_to_string(key_path).map_err(AgeVaultError::KeyRead)?;
        let identity: age::x25519::Identity = key_str
            .trim()
            .parse()
            .map_err(|e: &str| AgeVaultError::KeyParse(e.to_owned()))?;
        let ciphertext = std::fs::read(vault_path).map_err(AgeVaultError::VaultRead)?;
        let decryptor = age::Decryptor::new(&ciphertext[..]).map_err(AgeVaultError::Decrypt)?;
        let mut reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .map_err(AgeVaultError::Decrypt)?;
        let mut plaintext = Vec::new();
        reader
            .read_to_end(&mut plaintext)
            .map_err(AgeVaultError::Io)?;
        let secrets: HashMap<String, String> =
            serde_json::from_slice(&plaintext).map_err(AgeVaultError::Json)?;
        Ok(Self { secrets })
    }
}

#[cfg(feature = "vault-age")]
impl VaultProvider for AgeVaultProvider {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + Send + '_>> {
        let result = self.secrets.get(key).cloned();
        Box::pin(async move { Ok(result) })
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

    #[test]
    fn secret_from_string() {
        let s = Secret::new(String::from("test"));
        assert_eq!(s.expose(), "test");
    }

    #[test]
    fn secret_clone() {
        let s1 = Secret::new("test");
        let s2 = s1.clone();
        assert_eq!(s1.expose(), s2.expose());
    }

    #[test]
    fn secret_deserialize() {
        let json = "\"my-secret-value\"";
        let secret: Secret = serde_json::from_str(json).unwrap();
        assert_eq!(secret.expose(), "my-secret-value");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
    }
}

#[cfg(all(test, feature = "vault-age"))]
mod age_tests {
    use std::io::Write as _;

    use age::secrecy::ExposeSecret;

    use super::*;

    fn encrypt_json(identity: &age::x25519::Identity, json: &serde_json::Value) -> Vec<u8> {
        let recipient = identity.to_public();
        let encryptor =
            age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                .expect("encryptor creation");
        let mut encrypted = vec![];
        let mut writer = encryptor.wrap_output(&mut encrypted).expect("wrap_output");
        writer
            .write_all(json.to_string().as_bytes())
            .expect("write plaintext");
        writer.finish().expect("finish encryption");
        encrypted
    }

    fn write_temp_files(
        identity: &age::x25519::Identity,
        ciphertext: &[u8],
    ) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let key_path = dir.path().join("key.txt");
        let vault_path = dir.path().join("secrets.age");
        std::fs::write(&key_path, identity.to_string().expose_secret()).expect("write key");
        std::fs::write(&vault_path, ciphertext).expect("write vault");
        (dir, key_path, vault_path)
    }

    #[tokio::test]
    async fn age_vault_returns_existing_secret() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"KEY": "value"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        let result = vault.get_secret("KEY").await.unwrap();
        assert_eq!(result.as_deref(), Some("value"));
    }

    #[tokio::test]
    async fn age_vault_returns_none_for_missing() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"KEY": "value"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        let result = vault.get_secret("MISSING").await.unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn age_vault_bad_key_file() {
        let err = AgeVaultProvider::new(
            Path::new("/nonexistent/key.txt"),
            Path::new("/nonexistent/vault.age"),
        )
        .unwrap_err();
        assert!(matches!(err, AgeVaultError::KeyRead(_)));
    }

    #[test]
    fn age_vault_bad_key_parse() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("bad-key.txt");
        std::fs::write(&key_path, "not-a-valid-age-key").unwrap();

        let vault_path = dir.path().join("vault.age");
        std::fs::write(&vault_path, b"dummy").unwrap();

        let err = AgeVaultProvider::new(&key_path, &vault_path).unwrap_err();
        assert!(matches!(err, AgeVaultError::KeyParse(_)));
    }

    #[test]
    fn age_vault_bad_vault_file() {
        let dir = tempfile::tempdir().unwrap();
        let identity = age::x25519::Identity::generate();
        let key_path = dir.path().join("key.txt");
        std::fs::write(&key_path, identity.to_string().expose_secret()).unwrap();

        let err =
            AgeVaultProvider::new(&key_path, Path::new("/nonexistent/vault.age")).unwrap_err();
        assert!(matches!(err, AgeVaultError::VaultRead(_)));
    }

    #[test]
    fn age_vault_wrong_key() {
        let identity = age::x25519::Identity::generate();
        let wrong_identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"KEY": "value"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, _, vault_path) = write_temp_files(&identity, &encrypted);

        let dir2 = tempfile::tempdir().unwrap();
        let wrong_key_path = dir2.path().join("wrong-key.txt");
        std::fs::write(&wrong_key_path, wrong_identity.to_string().expose_secret()).unwrap();

        let err = AgeVaultProvider::new(&wrong_key_path, &vault_path).unwrap_err();
        assert!(matches!(err, AgeVaultError::Decrypt(_)));
    }

    #[test]
    fn age_vault_invalid_json() {
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        let encryptor =
            age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                .expect("encryptor");
        let mut encrypted = vec![];
        let mut writer = encryptor.wrap_output(&mut encrypted).expect("wrap");
        writer.write_all(b"not json").expect("write");
        writer.finish().expect("finish");

        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);
        let err = AgeVaultProvider::new(&key_path, &vault_path).unwrap_err();
        assert!(matches!(err, AgeVaultError::Json(_)));
    }

    #[tokio::test]
    async fn age_encrypt_decrypt_resolve_secrets_roundtrip() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({
            "ZEPH_CLAUDE_API_KEY": "sk-ant-test-123",
            "ZEPH_TELEGRAM_TOKEN": "tg-token-456"
        });
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        let mut config =
            crate::config::Config::load(Path::new("/nonexistent/config.toml")).unwrap();
        config.resolve_secrets(&vault).await.unwrap();

        assert_eq!(
            config.secrets.claude_api_key.as_ref().unwrap().expose(),
            "sk-ant-test-123"
        );
        let tg = config.telegram.unwrap();
        assert_eq!(tg.token.as_deref(), Some("tg-token-456"));
    }

    #[test]
    fn age_vault_debug_impl() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"KEY1": "value1", "KEY2": "value2"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        let debug = format!("{vault:?}");
        assert!(debug.contains("AgeVaultProvider"));
        assert!(debug.contains("[2 secrets]"));
        assert!(!debug.contains("value1"));
    }

    #[test]
    fn age_vault_error_display() {
        let key_err =
            AgeVaultError::KeyRead(std::io::Error::new(std::io::ErrorKind::NotFound, "test"));
        assert!(key_err.to_string().contains("failed to read key file"));

        let parse_err = AgeVaultError::KeyParse("bad key".into());
        assert!(
            parse_err
                .to_string()
                .contains("failed to parse age identity")
        );

        let vault_err =
            AgeVaultError::VaultRead(std::io::Error::new(std::io::ErrorKind::NotFound, "test"));
        assert!(vault_err.to_string().contains("failed to read vault file"));
    }
}
