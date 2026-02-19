use std::fmt;
use std::future::Future;
use std::io::Write as _;
use std::pin::Pin;

use std::collections::HashMap;

use std::io::Read as _;

use std::path::{Path, PathBuf};

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
    #[error("age encryption failed: {0}")]
    Encrypt(String),
    #[error("failed to write vault file: {0}")]
    VaultWrite(std::io::Error),
    #[error("failed to write key file: {0}")]
    KeyWrite(std::io::Error),
}

pub struct AgeVaultProvider {
    secrets: HashMap<String, String>,
    key_path: PathBuf,
    vault_path: PathBuf,
}

impl fmt::Debug for AgeVaultProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgeVaultProvider")
            .field("secrets", &format_args!("[{} secrets]", self.secrets.len()))
            .field("key_path", &self.key_path)
            .field("vault_path", &self.vault_path)
            .finish()
    }
}

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
        Self::load(key_path, vault_path)
    }

    /// Load vault from disk, storing paths for subsequent write operations.
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on key/vault read failure, parse error, or decryption failure.
    pub fn load(key_path: &Path, vault_path: &Path) -> Result<Self, AgeVaultError> {
        let key_str = std::fs::read_to_string(key_path).map_err(AgeVaultError::KeyRead)?;
        let identity = parse_identity(&key_str)?;
        let ciphertext = std::fs::read(vault_path).map_err(AgeVaultError::VaultRead)?;
        let secrets = decrypt_secrets(&identity, &ciphertext)?;
        Ok(Self {
            secrets,
            key_path: key_path.to_owned(),
            vault_path: vault_path.to_owned(),
        })
    }

    /// Serialize and re-encrypt secrets to vault file using atomic write (temp + rename).
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on encryption or write failure.
    ///
    /// Note: re-reads and re-parses the key file on each call. For CLI one-shot use this
    /// is acceptable; if used in a long-lived context consider caching the parsed identity.
    pub fn save(&self) -> Result<(), AgeVaultError> {
        let key_str = std::fs::read_to_string(&self.key_path).map_err(AgeVaultError::KeyRead)?;
        let identity = parse_identity(&key_str)?;
        let ciphertext = encrypt_secrets(&identity, &self.secrets)?;
        atomic_write(&self.vault_path, &ciphertext)
    }

    /// Insert or update a secret in the in-memory map.
    pub fn set_secret_mut(&mut self, key: String, value: String) {
        self.secrets.insert(key, value);
    }

    /// Remove a secret from the in-memory map. Returns `true` if the key existed.
    pub fn remove_secret_mut(&mut self, key: &str) -> bool {
        self.secrets.remove(key).is_some()
    }

    /// Return sorted list of secret keys (no values exposed).
    #[must_use]
    pub fn list_keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self.secrets.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    /// Look up a secret value by key, returning `None` if not present.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.secrets.get(key).map(String::as_str)
    }

    /// Generate a new x25519 keypair, write key file (mode 0600), and create an empty encrypted vault.
    ///
    /// Outputs:
    /// - `<dir>/vault-key.txt` — age identity (private + public key comment)
    /// - `<dir>/secrets.age`  — age-encrypted empty JSON object
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on key/vault write failure or encryption failure.
    pub fn init_vault(dir: &Path) -> Result<(), AgeVaultError> {
        use age::secrecy::ExposeSecret as _;

        std::fs::create_dir_all(dir).map_err(AgeVaultError::KeyWrite)?;

        let identity = age::x25519::Identity::generate();
        let public_key = identity.to_public();

        let key_content = format!(
            "# public key: {}\n{}\n",
            public_key,
            identity.to_string().expose_secret()
        );

        let key_path = dir.join("vault-key.txt");
        write_private_file(&key_path, key_content.as_bytes())?;

        let vault_path = dir.join("secrets.age");
        let empty: HashMap<String, String> = HashMap::new();
        let ciphertext = encrypt_secrets(&identity, &empty)?;
        atomic_write(&vault_path, &ciphertext)?;

        println!("Vault initialized:");
        println!("  Key:   {}", key_path.display());
        println!("  Vault: {}", vault_path.display());

        Ok(())
    }
}

fn parse_identity(key_str: &str) -> Result<age::x25519::Identity, AgeVaultError> {
    let key_line = key_str
        .lines()
        .find(|l| !l.starts_with('#') && !l.trim().is_empty())
        .ok_or_else(|| AgeVaultError::KeyParse("no identity line found".into()))?;
    key_line
        .trim()
        .parse()
        .map_err(|e: &str| AgeVaultError::KeyParse(e.to_owned()))
}

fn decrypt_secrets(
    identity: &age::x25519::Identity,
    ciphertext: &[u8],
) -> Result<HashMap<String, String>, AgeVaultError> {
    let decryptor = age::Decryptor::new(ciphertext).map_err(AgeVaultError::Decrypt)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(AgeVaultError::Decrypt)?;
    let mut plaintext = Vec::with_capacity(ciphertext.len());
    reader
        .read_to_end(&mut plaintext)
        .map_err(AgeVaultError::Io)?;
    // TODO: zeroize plaintext buffer after use once zeroize is added to workspace deps.
    serde_json::from_slice(&plaintext).map_err(AgeVaultError::Json)
}

fn encrypt_secrets(
    identity: &age::x25519::Identity,
    secrets: &HashMap<String, String>,
) -> Result<Vec<u8>, AgeVaultError> {
    let recipient = identity.to_public();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|e| AgeVaultError::Encrypt(e.to_string()))?;
    let json = serde_json::to_vec(secrets).map_err(AgeVaultError::Json)?;
    let mut ciphertext = Vec::with_capacity(json.len() + 64);
    let mut writer = encryptor
        .wrap_output(&mut ciphertext)
        .map_err(|e| AgeVaultError::Encrypt(e.to_string()))?;
    writer.write_all(&json).map_err(AgeVaultError::Io)?;
    writer
        .finish()
        .map_err(|e| AgeVaultError::Encrypt(e.to_string()))?;
    Ok(ciphertext)
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<(), AgeVaultError> {
    let tmp_path = path.with_extension("age.tmp");
    std::fs::write(&tmp_path, data).map_err(AgeVaultError::VaultWrite)?;
    std::fs::rename(&tmp_path, path).map_err(AgeVaultError::VaultWrite)
}

#[cfg(unix)]
fn write_private_file(path: &Path, data: &[u8]) -> Result<(), AgeVaultError> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(AgeVaultError::KeyWrite)?;
    file.write_all(data).map_err(AgeVaultError::KeyWrite)
}

// TODO: Windows does not enforce file permissions via mode bits; the key file is created
// without access control restrictions. Consider using Windows ACLs in a follow-up.
#[cfg(not(unix))]
fn write_private_file(path: &Path, data: &[u8]) -> Result<(), AgeVaultError> {
    std::fs::write(path, data).map_err(AgeVaultError::KeyWrite)
}

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

#[cfg(test)]
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

    #[tokio::test]
    async fn age_vault_key_file_with_comments() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"KEY": "value"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let key_with_comments = format!(
            "# created: 2026-02-11T12:00:00+03:00\n# public key: {}\n{}\n",
            identity.to_public(),
            identity.to_string().expose_secret()
        );
        std::fs::write(&key_path, &key_with_comments).unwrap();

        let vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        let result = vault.get_secret("KEY").await.unwrap();
        assert_eq!(result.as_deref(), Some("value"));
    }

    #[test]
    fn age_vault_key_file_only_comments() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("comments-only.txt");
        std::fs::write(&key_path, "# comment\n# another\n").unwrap();
        let vault_path = dir.path().join("vault.age");
        std::fs::write(&vault_path, b"dummy").unwrap();

        let err = AgeVaultProvider::new(&key_path, &vault_path).unwrap_err();
        assert!(matches!(err, AgeVaultError::KeyParse(_)));
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

        let enc_err = AgeVaultError::Encrypt("bad".into());
        assert!(enc_err.to_string().contains("age encryption failed"));

        let write_err = AgeVaultError::VaultWrite(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "test",
        ));
        assert!(write_err.to_string().contains("failed to write vault file"));
    }

    #[test]
    fn age_vault_set_and_list_keys() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"A": "1"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let mut vault = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        vault.set_secret_mut("B".to_owned(), "2".to_owned());
        vault.set_secret_mut("C".to_owned(), "3".to_owned());

        let keys = vault.list_keys();
        assert_eq!(keys, vec!["A", "B", "C"]);
    }

    #[test]
    fn age_vault_remove_secret() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"X": "val", "Y": "val2"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let mut vault = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        assert!(vault.remove_secret_mut("X"));
        assert!(!vault.remove_secret_mut("NONEXISTENT"));
        assert_eq!(vault.list_keys(), vec!["Y"]);
    }

    #[tokio::test]
    async fn age_vault_save_roundtrip() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"ORIG": "value"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let mut vault = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        vault.set_secret_mut("NEW_KEY".to_owned(), "new_value".to_owned());
        vault.save().unwrap();

        let reloaded = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        let result = reloaded.get_secret("NEW_KEY").await.unwrap();
        assert_eq!(result.as_deref(), Some("new_value"));

        let orig = reloaded.get_secret("ORIG").await.unwrap();
        assert_eq!(orig.as_deref(), Some("value"));
    }

    #[test]
    fn age_vault_init_vault() {
        let dir = tempfile::tempdir().unwrap();
        AgeVaultProvider::init_vault(dir.path()).unwrap();

        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");
        assert!(key_path.exists());
        assert!(vault_path.exists());

        let vault = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        assert_eq!(vault.list_keys(), Vec::<&str>::new());
    }
}
