// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Secret storage for Zeph with pluggable backends and age encryption.
//!
//! This crate provides:
//!
//! - [`VaultProvider`] — an async trait for secret retrieval, implemented by all backends.
//! - [`AgeVaultProvider`] — primary backend that stores secrets as an age-encrypted JSON file.
//! - [`EnvVaultProvider`] — development/testing backend that reads secrets from environment
//!   variables prefixed with `ZEPH_SECRET_`.
//! - [`ArcAgeVaultProvider`] — thin `Arc<RwLock<AgeVaultProvider>>` wrapper that implements
//!   [`VaultProvider`] so the age vault can be stored as a trait object while still being
//!   accessible for mutable operations (e.g. OAuth credential persistence).
//! - `MockVaultProvider` — in-memory backend available under the `mock` feature flag and in
//!   `#[cfg(test)]` contexts.
//!
//! [`Secret`] and [`VaultError`] live in `zeph-common` (layer 0) and are re-exported here so
//! callers only need to depend on `zeph-vault`.
//!
//! # Security model
//!
//! - Secrets are stored as a JSON object encrypted with [age](https://age-encryption.org) using
//!   an x25519 keypair. Only the holder of the private key file can decrypt the vault.
//! - In-memory secret values are kept in [`zeroize::Zeroizing`] buffers, which overwrite the
//!   memory on drop.
//! - The key file is created with Unix permission `0600` (owner-read/write only). On non-Unix
//!   platforms the file is created without access control restrictions.
//! - Vault writes are atomic: a temporary file is written first, then renamed, so a crash during
//!   write never corrupts the existing vault.
//!
//! # Vault file layout
//!
//! ```text
//! ~/.config/zeph/
//! ├── vault-key.txt   # age identity (private key), mode 0600
//! └── secrets.age     # age-encrypted JSON: {"KEY": "value", ...}
//! ```
//!
//! # Quick start
//!
//! ```no_run
//! use std::path::Path;
//! use zeph_vault::{AgeVaultProvider, VaultProvider as _};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let vault = AgeVaultProvider::new(
//!     Path::new("/etc/zeph/vault-key.txt"),
//!     Path::new("/etc/zeph/secrets.age"),
//! )?;
//!
//! // Synchronous access via the direct getter
//! if let Some(key) = vault.get("ZEPH_OPENAI_API_KEY") {
//!     println!("key length: {}", key.len());
//! }
//! # Ok(())
//! # }
//! ```

mod age;
mod arc;
#[cfg(any(test, feature = "env-vault"))]
mod env;
#[cfg(any(test, feature = "mock"))]
mod mock;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

// Secret and VaultError live in zeph-common (Layer 0) so that zeph-config (Layer 1)
// can reference them without creating a circular dependency.
pub use zeph_common::secret::{Secret, VaultError};

pub use age::{AgeVaultError, AgeVaultProvider};
pub use arc::ArcAgeVaultProvider;
#[cfg(any(test, feature = "env-vault"))]
pub use env::EnvVaultProvider;
#[cfg(any(test, feature = "mock"))]
pub use mock::MockVaultProvider;

/// Pluggable secret retrieval backend.
///
/// Implement this trait to integrate a custom secret store (e.g. `HashiCorp` Vault, `AWS` Secrets
/// Manager, `1Password`). The crate ships implementations out of the box:
/// [`AgeVaultProvider`], [`ArcAgeVaultProvider`], and (with `env-vault` feature) `EnvVaultProvider`.
///
/// # Implementing
///
/// ```
/// use std::pin::Pin;
/// use std::future::Future;
/// use zeph_vault::{VaultProvider, VaultError};
///
/// struct ConstantVault(&'static str);
///
/// impl VaultProvider for ConstantVault {
///     fn get_secret(
///         &self,
///         key: &str,
///     ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>> {
///         let value = if key == "MY_KEY" { Some(self.0.to_owned()) } else { None };
///         Box::pin(async move { Ok(value) })
///     }
/// }
/// ```
pub trait VaultProvider: Send + Sync {
    /// Retrieve a secret by key.
    ///
    /// Returns `Ok(None)` when the key does not exist. Returns `Err(VaultError)` on
    /// backend failures (I/O, decryption, network, etc.).
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>>;

    /// Return all known secret keys.
    ///
    /// Used internally for scanning `ZEPH_SECRET_*` prefixes and for the `vault list` CLI
    /// subcommand. The default implementation returns an empty `Vec`; override it when the
    /// backend supports key enumeration.
    fn list_keys(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Return the default vault directory for the current platform.
///
/// Resolution order:
/// 1. `$XDG_CONFIG_HOME/zeph` (Linux / BSD)
/// 2. `$APPDATA/zeph` (Windows)
/// 3. `$HOME/.config/zeph` (macOS fallback and others)
///
/// # Examples
///
/// ```
/// let dir = zeph_vault::default_vault_dir();
/// // Ends with "zeph" on all platforms.
/// assert!(dir.ends_with("zeph"));
/// ```
#[must_use]
pub fn default_vault_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("zeph");
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("zeph");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join(".config").join("zeph")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::doc_markdown)]

    use super::*;

    #[test]
    fn atomic_write_uses_age_tmp_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.age");
        age::atomic_write(&path, b"data").unwrap();
        assert!(path.exists());
        let tmp = path.with_added_extension("tmp");
        assert_eq!(tmp.file_name().unwrap(), "vault.age.tmp");
    }

    #[cfg(unix)]
    #[test]
    fn init_vault_sets_0600_on_both_files() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        AgeVaultProvider::init_vault(dir.path()).unwrap();
        let key_mode = std::fs::metadata(dir.path().join("vault-key.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let vault_mode = std::fs::metadata(dir.path().join("secrets.age"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(key_mode, 0o600, "vault-key.txt must be 0o600");
        assert_eq!(vault_mode, 0o600, "secrets.age must be 0o600");
    }

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

    #[allow(unsafe_code)]
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
    fn secret_expose_roundtrip() {
        let s = Secret::new("test");
        let owned = s.expose().to_owned();
        let s2 = Secret::new(owned);
        assert_eq!(s.expose(), s2.expose());
    }

    #[test]
    fn secret_deserialize() {
        let json = "\"my-secret-value\"";
        let secret: Secret = serde_json::from_str(json).unwrap();
        assert_eq!(secret.expose(), "my-secret-value");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
    }

    #[test]
    fn mock_vault_list_keys_sorted() {
        let vault = MockVaultProvider::new()
            .with_secret("B_KEY", "v2")
            .with_secret("A_KEY", "v1")
            .with_secret("C_KEY", "v3");
        let mut keys = vault.list_keys();
        keys.sort_unstable();
        assert_eq!(keys, vec!["A_KEY", "B_KEY", "C_KEY"]);
    }

    #[test]
    fn mock_vault_list_keys_empty() {
        let vault = MockVaultProvider::new();
        assert!(vault.list_keys().is_empty());
    }

    #[allow(unsafe_code)]
    #[test]
    fn env_vault_list_keys_filters_zeph_secret_prefix() {
        let key = "ZEPH_SECRET_TEST_LISTKEYS_UNIQUE_9999";
        unsafe { std::env::set_var(key, "v") };
        let vault = EnvVaultProvider;
        let keys = vault.list_keys();
        assert!(keys.contains(&key.to_owned()));
        unsafe { std::env::remove_var(key) };
    }
}

#[cfg(test)]
mod age_tests {
    use std::io::Write as _;

    // NOTE: use `::age` (not bare `age`) to reference the external crate — the local
    // `mod age` submodule would otherwise shadow it when `use super::*` is in scope.
    use ::age;
    use age::secrecy::ExposeSecret;

    use super::*;
    use crate::age::decrypt_secrets;

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
        use std::path::Path;
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
        use std::path::Path;
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
    fn age_vault_get_method_returns_str() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"FOO": "bar"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let vault = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        assert_eq!(vault.get("FOO"), Some("bar"));
        assert_eq!(vault.get("MISSING"), None);
    }

    #[test]
    fn age_vault_empty_secret_value() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"EMPTY": ""});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let vault = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        assert_eq!(vault.get("EMPTY"), Some(""));
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

    #[tokio::test]
    async fn age_vault_keys_sorted_after_roundtrip() {
        let identity = age::x25519::Identity::generate();
        // Insert keys intentionally out of lexicographic order.
        let json = serde_json::json!({"ZEBRA": "z", "APPLE": "a", "MANGO": "m"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let vault = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        let keys = vault.list_keys();
        assert_eq!(keys, vec!["APPLE", "MANGO", "ZEBRA"]);
    }

    #[test]
    fn age_vault_save_preserves_key_order() {
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"Z_KEY": "z", "A_KEY": "a", "M_KEY": "m"});
        let encrypted = encrypt_json(&identity, &json);
        let (_dir, key_path, vault_path) = write_temp_files(&identity, &encrypted);

        let mut vault = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        vault.set_secret_mut("B_KEY".to_owned(), "b".to_owned());
        vault.save().unwrap();

        let reloaded = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        let keys = reloaded.list_keys();
        assert_eq!(keys, vec!["A_KEY", "B_KEY", "M_KEY", "Z_KEY"]);
    }

    #[test]
    fn age_vault_decrypt_returns_btreemap_sorted() {
        let identity = age::x25519::Identity::generate();
        // Provide keys in reverse order; BTreeMap must sort them on deserialization.
        let json_str = r#"{"zoo":"z","bar":"b","alpha":"a"}"#;
        let recipient = identity.to_public();
        let encryptor =
            age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                .expect("encryptor");
        let mut encrypted = vec![];
        let mut writer = encryptor.wrap_output(&mut encrypted).expect("wrap");
        writer.write_all(json_str.as_bytes()).expect("write");
        writer.finish().expect("finish");

        let ciphertext = encrypted;
        let secrets = decrypt_secrets(&identity, &ciphertext).unwrap();
        let keys: Vec<&str> = secrets.keys().map(String::as_str).collect();
        // BTreeMap guarantees lexicographic order regardless of insertion order.
        assert_eq!(keys, vec!["alpha", "bar", "zoo"]);
    }

    #[test]
    fn age_vault_into_iter_consumes_all_entries() {
        // Regression: drain() was replaced with into_iter(). Verify all entries
        // are consumed and values are accessible without data loss.
        let identity = age::x25519::Identity::generate();
        let json = serde_json::json!({"K1": "v1", "K2": "v2", "K3": "v3"});
        let encrypted = encrypt_json(&identity, &json);
        let ciphertext = encrypted;
        let secrets = decrypt_secrets(&identity, &ciphertext).unwrap();

        let mut pairs: Vec<(String, String)> = secrets
            .into_iter()
            .map(|(k, v)| (k, v.as_str().to_owned()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], ("K1".to_owned(), "v1".to_owned()));
        assert_eq!(pairs[1], ("K2".to_owned(), "v2".to_owned()));
        assert_eq!(pairs[2], ("K3".to_owned(), "v3".to_owned()));
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn secret_value_roundtrip(s in ".*") {
            let secret = Secret::new(s.clone());
            assert_eq!(secret.expose(), s.as_str());
        }

        #[test]
        fn secret_debug_always_redacted(s in ".*") {
            let secret = Secret::new(s);
            assert_eq!(format!("{secret:?}"), "[REDACTED]");
        }

        #[test]
        fn secret_display_always_redacted(s in ".*") {
            let secret = Secret::new(s);
            assert_eq!(format!("{secret}"), "[REDACTED]");
        }
    }
}
