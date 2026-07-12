// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Age-encrypted vault backend.
//!
//! This module provides [`AgeVaultProvider`], the primary secret storage backend, and the
//! associated [`AgeVaultError`] type. Secrets are stored as a JSON object encrypted with an
//! x25519 keypair using the [age](https://age-encryption.org) format.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;

use zeroize::Zeroizing;

use crate::VaultProvider;
use zeph_common::secret::VaultError;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during age vault operations.
///
/// Each variant wraps the underlying cause so callers can match on failure type without
/// parsing error strings.
///
/// # Examples
///
/// ```
/// use zeph_vault::AgeVaultError;
///
/// let err = AgeVaultError::KeyParse("no identity line found".into());
/// assert!(err.to_string().contains("failed to parse age identity"));
/// ```
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum AgeVaultError {
    /// The key file could not be read from disk.
    #[error("failed to read key file: {0}")]
    KeyRead(std::io::Error),
    /// The key file content could not be parsed as an age identity.
    #[error("failed to parse age identity: {0}")]
    KeyParse(String),
    /// The vault file could not be read from disk.
    #[error("failed to read vault file: {0}")]
    VaultRead(std::io::Error),
    /// The age decryption step failed (wrong key, corrupted file, etc.).
    #[error("age decryption failed: {0}")]
    Decrypt(age::DecryptError),
    /// An I/O error occurred while reading plaintext from the age stream.
    #[error("I/O error during decryption: {0}")]
    Io(std::io::Error),
    /// The decrypted bytes could not be parsed as JSON.
    #[error("invalid JSON in vault: {0}")]
    Json(serde_json::Error),
    /// The age encryption step failed.
    #[error("age encryption failed: {0}")]
    Encrypt(String),
    /// The vault file (or its temporary predecessor) could not be written to disk.
    #[error("failed to write vault file: {0}")]
    VaultWrite(std::io::Error),
    /// The key file could not be written to disk.
    #[error("failed to write key file: {0}")]
    KeyWrite(std::io::Error),
    /// [`AgeVaultProvider::set_secret_mut`] was called with `overwrite: false` for a key that
    /// already exists in the vault.
    #[error("secret key already exists: {0} (pass overwrite=true to replace it)")]
    AlreadyExists(String),
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Age-encrypted vault backend.
///
/// Secrets are stored as a JSON object (`{"KEY": "value", ...}`) encrypted with an x25519
/// keypair using the [age](https://age-encryption.org) format. The in-memory secret values
/// are held in [`zeroize::Zeroizing`] buffers.
///
/// # File layout
///
/// ```text
/// <dir>/vault-key.txt   # age identity (private key), Unix mode 0600
/// <dir>/secrets.age     # age-encrypted JSON object
/// ```
///
/// # Initialising a new vault
///
/// Use [`AgeVaultProvider::init_vault`] to generate a fresh keypair and create an empty vault:
///
/// ```no_run
/// use std::path::Path;
/// use zeph_vault::AgeVaultProvider;
///
/// AgeVaultProvider::init_vault(Path::new("/etc/zeph"))?;
/// // Produces:
/// //   /etc/zeph/vault-key.txt  (mode 0600)
/// //   /etc/zeph/secrets.age    (empty encrypted vault)
/// # Ok::<_, zeph_vault::AgeVaultError>(())
/// ```
///
/// # Atomic writes
///
/// [`save`][AgeVaultProvider::save] writes to a `.age.tmp` sibling file first, then renames it
/// atomically, so a crash during write never leaves the vault in a corrupted state.
pub struct AgeVaultProvider {
    pub(crate) secrets: BTreeMap<String, Zeroizing<String>>,
    pub(crate) key_path: PathBuf,
    pub(crate) vault_path: PathBuf,
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
    /// This is an alias for [`load`][Self::load] provided for ergonomic construction.
    ///
    /// # Arguments
    ///
    /// - `key_path` — path to the age identity (private key) file. Lines starting with `#`
    ///   and blank lines are ignored; the first non-comment line is parsed as the identity.
    /// - `vault_path` — path to the age-encrypted JSON file.
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on key/vault read failure, parse error, or decryption failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// let vault = AgeVaultProvider::new(
    ///     Path::new("/etc/zeph/vault-key.txt"),
    ///     Path::new("/etc/zeph/secrets.age"),
    /// )?;
    /// println!("{} secrets loaded", vault.list_keys().len());
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    pub fn new(key_path: &Path, vault_path: &Path) -> Result<Self, AgeVaultError> {
        Self::load(key_path, vault_path)
    }

    /// Load vault from disk, storing paths for subsequent write operations.
    ///
    /// Reads and decrypts the vault, then retains both paths so that
    /// [`save`][Self::save] can re-encrypt and persist changes without requiring callers to
    /// pass paths again.
    ///
    /// This method performs blocking I/O on the calling thread. Use [`load_async`][Self::load_async]
    /// when calling from an async context to avoid stalling the tokio executor.
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on key/vault read failure, parse error, or decryption failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// let vault = AgeVaultProvider::load(
    ///     Path::new("/etc/zeph/vault-key.txt"),
    ///     Path::new("/etc/zeph/secrets.age"),
    /// )?;
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    #[tracing::instrument(name = "vault.age.load", skip_all, err)]
    pub fn load(key_path: &Path, vault_path: &Path) -> Result<Self, AgeVaultError> {
        let key_str =
            Zeroizing::new(std::fs::read_to_string(key_path).map_err(AgeVaultError::KeyRead)?);
        let identity = parse_identity(&key_str)?;
        let ciphertext = std::fs::read(vault_path).map_err(AgeVaultError::VaultRead)?;
        let secrets = decrypt_secrets(&identity, &ciphertext)?;
        Ok(Self {
            secrets,
            key_path: key_path.to_owned(),
            vault_path: vault_path.to_owned(),
        })
    }

    /// Async variant of [`load`][Self::load] — offloads blocking I/O to a `spawn_blocking` thread.
    ///
    /// Use this when calling from an async context to avoid stalling the tokio executor.
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on key/vault read failure, parse error, decryption failure, or
    /// if the blocking task panics.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// # async fn example() -> Result<(), zeph_vault::AgeVaultError> {
    /// let vault = AgeVaultProvider::load_async(
    ///     Path::new("/etc/zeph/vault-key.txt"),
    ///     Path::new("/etc/zeph/secrets.age"),
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument(name = "vault.age.load_async", skip_all, err)]
    pub async fn load_async(key_path: &Path, vault_path: &Path) -> Result<Self, AgeVaultError> {
        let key_path = key_path.to_owned();
        let vault_path = vault_path.to_owned();
        tokio::task::spawn_blocking(move || Self::load(&key_path, &vault_path))
            .await
            .map_err(|e| {
                AgeVaultError::Io(std::io::Error::other(format!(
                    "spawn_blocking panicked: {e}"
                )))
            })?
    }

    /// Serialize and re-encrypt secrets to vault file using atomic write (temp + rename).
    ///
    /// Re-reads and re-parses the key file on each call. For CLI one-shot use this is
    /// acceptable; if used in a long-lived context consider caching the parsed identity.
    ///
    /// This method performs blocking I/O on the calling thread. Use [`save_async`][Self::save_async]
    /// when calling from an async context to avoid stalling the tokio executor.
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on encryption or write failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// let mut vault = AgeVaultProvider::load(
    ///     Path::new("/etc/zeph/vault-key.txt"),
    ///     Path::new("/etc/zeph/secrets.age"),
    /// )?;
    /// vault.set_secret_mut("MY_TOKEN".into(), "tok_abc123".into(), false)?;
    /// vault.save()?;
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    #[tracing::instrument(name = "vault.age.save", skip_all, err)]
    pub fn save(&self) -> Result<(), AgeVaultError> {
        let key_str = Zeroizing::new(
            std::fs::read_to_string(&self.key_path).map_err(AgeVaultError::KeyRead)?,
        );
        let identity = parse_identity(&key_str)?;
        let ciphertext = encrypt_secrets(&identity, &self.secrets)?;
        atomic_write(&self.vault_path, &ciphertext)
    }

    /// Async variant of [`save`][Self::save] — offloads blocking I/O to a `spawn_blocking` thread.
    ///
    /// Use this when calling from an async context to avoid stalling the tokio executor.
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on encryption or write failure, or if the blocking task panics.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// # async fn example() -> Result<(), zeph_vault::AgeVaultError> {
    /// let mut vault = AgeVaultProvider::load(
    ///     Path::new("/etc/zeph/vault-key.txt"),
    ///     Path::new("/etc/zeph/secrets.age"),
    /// )?;
    /// vault.set_secret_mut("MY_TOKEN".into(), "tok_abc123".into(), false)?;
    /// vault.save_async().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[tracing::instrument(name = "vault.age.save_async", skip_all, err)]
    pub async fn save_async(&self) -> Result<(), AgeVaultError> {
        let key_path = self.key_path.clone();
        let vault_path = self.vault_path.clone();
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || {
            let key_str =
                Zeroizing::new(std::fs::read_to_string(&key_path).map_err(AgeVaultError::KeyRead)?);
            let identity = parse_identity(&key_str)?;
            let ciphertext = encrypt_secrets(&identity, &secrets)?;
            atomic_write(&vault_path, &ciphertext)
        })
        .await
        .map_err(|e| {
            AgeVaultError::Io(std::io::Error::other(format!(
                "spawn_blocking panicked: {e}"
            )))
        })?
    }

    /// Insert or update a secret in the in-memory map.
    ///
    /// Refuses to replace an existing key unless `overwrite` is `true`, so that callers cannot
    /// silently destroy a previously-stored secret by accident — see #5955 (and the sibling
    /// incident #5874, which hit the same gap in the `zeph init` durable-execution wizard before
    /// this guard existed at the vault layer). Callers that intend an unconditional update (e.g.
    /// OAuth token refresh) pass `overwrite: true` explicitly.
    ///
    /// Call [`save`][Self::save] afterwards to persist the change to disk.
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError::AlreadyExists`] if `key` is already present and `overwrite` is
    /// `false`. The in-memory map is left untouched in that case.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// let mut vault = AgeVaultProvider::load(
    ///     Path::new("/etc/zeph/vault-key.txt"),
    ///     Path::new("/etc/zeph/secrets.age"),
    /// )?;
    /// vault.set_secret_mut("API_KEY".into(), "sk-...".into(), false)?;
    /// vault.save()?;
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    pub fn set_secret_mut(
        &mut self,
        key: String,
        value: String,
        overwrite: bool,
    ) -> Result<(), AgeVaultError> {
        if !overwrite && self.secrets.contains_key(&key) {
            return Err(AgeVaultError::AlreadyExists(key));
        }
        self.secrets.insert(key, Zeroizing::new(value));
        Ok(())
    }

    /// Remove a secret from the in-memory map.
    ///
    /// Returns `true` if the key existed and was removed, `false` if it was not present.
    /// Call [`save`][Self::save] afterwards to persist the removal to disk.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// let mut vault = AgeVaultProvider::load(
    ///     Path::new("/etc/zeph/vault-key.txt"),
    ///     Path::new("/etc/zeph/secrets.age"),
    /// )?;
    /// let removed = vault.remove_secret_mut("OLD_KEY");
    /// if removed {
    ///     vault.save()?;
    /// }
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    pub fn remove_secret_mut(&mut self, key: &str) -> bool {
        self.secrets.remove(key).is_some()
    }

    /// Return sorted list of secret keys (no values exposed).
    ///
    /// Keys are returned in ascending lexicographic order. Secret values are never included.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// let vault = AgeVaultProvider::load(
    ///     Path::new("/etc/zeph/vault-key.txt"),
    ///     Path::new("/etc/zeph/secrets.age"),
    /// )?;
    /// for key in vault.list_keys() {
    ///     println!("{key}");
    /// }
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    #[must_use]
    pub fn list_keys(&self) -> Vec<&str> {
        let mut keys: Vec<&str> = self.secrets.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    /// Look up a secret value by key, returning `None` if not present.
    ///
    /// Returns a borrowed `&str` tied to the lifetime of the vault. For async use across await
    /// points, use [`VaultProvider::get_secret`] instead, which returns an owned `String`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// let vault = AgeVaultProvider::load(
    ///     Path::new("/etc/zeph/vault-key.txt"),
    ///     Path::new("/etc/zeph/secrets.age"),
    /// )?;
    /// match vault.get("ZEPH_OPENAI_API_KEY") {
    ///     Some(key) => println!("key length: {}", key.len()),
    ///     None => println!("key not configured"),
    /// }
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.secrets.get(key).map(|v| v.as_str())
    }

    /// Generate a new x25519 keypair, write the key file (mode 0600), and create an empty
    /// encrypted vault.
    ///
    /// Creates `dir` and all missing parent directories before writing files. Existing files
    /// are not checked — calling this on an already-initialised directory will overwrite both
    /// the key and the vault, making the old key irrecoverable.
    ///
    /// # Output files
    ///
    /// | File | Contents | Unix mode |
    /// |------|----------|-----------|
    /// | `<dir>/vault-key.txt` | age identity (private + public key comment) | `0600` |
    /// | `<dir>/secrets.age`   | age-encrypted empty JSON object `{}` | default |
    ///
    /// # Errors
    ///
    /// Returns [`AgeVaultError`] on key/vault write failure or encryption failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use zeph_vault::AgeVaultProvider;
    ///
    /// AgeVaultProvider::init_vault(Path::new("/etc/zeph"))?;
    /// // /etc/zeph/vault-key.txt and /etc/zeph/secrets.age are now ready.
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    pub fn init_vault(dir: &Path) -> Result<(), AgeVaultError> {
        use age::secrecy::ExposeSecret as _;

        std::fs::create_dir_all(dir).map_err(AgeVaultError::KeyWrite)?;

        let identity = age::x25519::Identity::generate();
        let public_key = identity.to_public();

        let key_content = Zeroizing::new(format!(
            "# public key: {}\n{}\n",
            public_key,
            identity.to_string().expose_secret()
        ));

        let key_path = dir.join("vault-key.txt");
        write_private_file(&key_path, key_content.as_bytes())?;

        let vault_path = dir.join("secrets.age");
        let empty: BTreeMap<String, Zeroizing<String>> = BTreeMap::new();
        let ciphertext = encrypt_secrets(&identity, &empty)?;
        atomic_write(&vault_path, &ciphertext)?;

        println!("Vault initialized:");
        println!("  Key:   {}", key_path.display());
        println!("  Vault: {}", vault_path.display());

        Ok(())
    }
}

impl VaultProvider for AgeVaultProvider {
    fn get_secret(
        &self,
        key: &str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, VaultError>> + Send + '_>> {
        let result = self.secrets.get(key).map(|v| (**v).clone());
        Box::pin(async move { Ok(result) })
    }

    fn list_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.secrets.keys().cloned().collect();
        keys.sort_unstable();
        keys
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

pub(crate) fn parse_identity(key_str: &str) -> Result<age::x25519::Identity, AgeVaultError> {
    let key_line = key_str
        .lines()
        .find(|l| !l.starts_with('#') && !l.trim().is_empty())
        .ok_or_else(|| AgeVaultError::KeyParse("no identity line found".into()))?;
    key_line
        .trim()
        .parse()
        .map_err(|e: &str| AgeVaultError::KeyParse(e.to_owned()))
}

pub(crate) fn decrypt_secrets(
    identity: &age::x25519::Identity,
    ciphertext: &[u8],
) -> Result<BTreeMap<String, Zeroizing<String>>, AgeVaultError> {
    let decryptor = age::Decryptor::new(ciphertext).map_err(AgeVaultError::Decrypt)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(AgeVaultError::Decrypt)?;
    let mut plaintext = Zeroizing::new(Vec::with_capacity(ciphertext.len()));
    reader
        .read_to_end(&mut plaintext)
        .map_err(AgeVaultError::Io)?;
    let raw: BTreeMap<String, String> =
        serde_json::from_slice(&plaintext).map_err(AgeVaultError::Json)?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| (k, Zeroizing::new(v)))
        .collect())
}

pub(crate) fn encrypt_secrets(
    identity: &age::x25519::Identity,
    secrets: &BTreeMap<String, Zeroizing<String>>,
) -> Result<Vec<u8>, AgeVaultError> {
    let recipient = identity.to_public();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|e| AgeVaultError::Encrypt(e.to_string()))?;
    let plain: BTreeMap<&str, &str> = secrets
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let json = Zeroizing::new(serde_json::to_vec(&plain).map_err(AgeVaultError::Json)?);
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

pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> Result<(), AgeVaultError> {
    zeph_common::fs_secure::atomic_write_private(path, data).map_err(AgeVaultError::VaultWrite)
}

pub(crate) fn write_private_file(path: &Path, data: &[u8]) -> Result<(), AgeVaultError> {
    zeph_common::fs_secure::write_private(path, data).map_err(AgeVaultError::KeyWrite)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn init_temp_vault(dir: &Path) -> (PathBuf, PathBuf) {
        AgeVaultProvider::init_vault(dir).expect("init_vault failed");
        (dir.join("vault-key.txt"), dir.join("secrets.age"))
    }

    #[test]
    fn round_trip() {
        let dir = tempdir().unwrap();
        let (key_path, vault_path) = init_temp_vault(dir.path());

        let mut vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        vault
            .set_secret_mut("KEY".into(), "val".into(), false)
            .unwrap();
        vault.save().unwrap();

        let loaded = AgeVaultProvider::load(&key_path, &vault_path).unwrap();
        assert_eq!(loaded.get("KEY"), Some("val"));
    }

    #[test]
    fn remove_secret() {
        let dir = tempdir().unwrap();
        let (key_path, vault_path) = init_temp_vault(dir.path());

        let mut vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        vault
            .set_secret_mut("KEY".into(), "val".into(), false)
            .unwrap();

        assert!(vault.remove_secret_mut("KEY"));
        assert!(!vault.remove_secret_mut("KEY"));
        assert_eq!(vault.get("KEY"), None);
    }

    #[test]
    fn init_vault_creates_files() {
        let dir = tempdir().unwrap();
        AgeVaultProvider::init_vault(dir.path()).unwrap();

        assert!(dir.path().join("vault-key.txt").exists());
        assert!(dir.path().join("secrets.age").exists());
    }

    #[test]
    fn load_missing_vault_errors() {
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");

        let result = AgeVaultProvider::load(&key_path, &vault_path);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(unix)]
    fn key_file_has_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().unwrap();
        let (key_path, _) = init_temp_vault(dir.path());

        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "vault-key.txt must have mode 0600, got {mode:o}"
        );
    }

    #[test]
    fn load_blank_key_returns_key_parse_error() {
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");

        // Key file with only comments and blank lines — no valid identity line.
        std::fs::write(&key_path, "# comment\n\n# another comment\n").unwrap();
        // Vault file must exist so the error comes from key parsing, not vault read.
        std::fs::write(&vault_path, b"").unwrap();

        let result = AgeVaultProvider::load(&key_path, &vault_path);
        assert!(
            matches!(result, Err(AgeVaultError::KeyParse(_))),
            "expected KeyParse, got {result:?}",
        );
    }

    #[test]
    fn decrypt_corrupted_ciphertext_returns_decrypt_error() {
        let dir = tempdir().unwrap();
        let (key_path, vault_path) = init_temp_vault(dir.path());

        // Overwrite the encrypted vault with random garbage.
        std::fs::write(&vault_path, b"not valid age ciphertext at all").unwrap();

        let result = AgeVaultProvider::load(&key_path, &vault_path);
        assert!(
            matches!(result, Err(AgeVaultError::Decrypt(_))),
            "expected Decrypt, got {result:?}",
        );
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = tempdir().unwrap();
        let (key_path, vault_path) = init_temp_vault(dir.path());

        let mut vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        vault
            .set_secret_mut("TMP_TEST".into(), "value".into(), false)
            .unwrap();
        vault.save().unwrap();

        let tmp_path = vault_path.with_added_extension("tmp");
        assert!(!tmp_path.exists(), ".age.tmp must not exist after save()");
        assert!(vault_path.exists(), "secrets.age must exist after save()");
    }

    /// Regression for #5955: `set_secret_mut` must refuse to replace an existing key when
    /// `overwrite` is `false`, and must leave the previous value untouched.
    #[test]
    fn set_secret_mut_rejects_overwrite_when_not_requested() {
        let dir = tempdir().unwrap();
        let (key_path, vault_path) = init_temp_vault(dir.path());

        let mut vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        vault
            .set_secret_mut("KEY".into(), "original".into(), false)
            .unwrap();

        let result = vault.set_secret_mut("KEY".into(), "clobbered".into(), false);
        assert!(
            matches!(result, Err(AgeVaultError::AlreadyExists(ref k)) if k == "KEY"),
            "expected AlreadyExists(\"KEY\"), got {result:?}",
        );
        assert_eq!(vault.get("KEY"), Some("original"));
    }

    /// Regression for #5955: `overwrite: true` must replace an existing value.
    #[test]
    fn set_secret_mut_replaces_when_overwrite_requested() {
        let dir = tempdir().unwrap();
        let (key_path, vault_path) = init_temp_vault(dir.path());

        let mut vault = AgeVaultProvider::new(&key_path, &vault_path).unwrap();
        vault
            .set_secret_mut("KEY".into(), "original".into(), false)
            .unwrap();
        vault
            .set_secret_mut("KEY".into(), "updated".into(), true)
            .unwrap();

        assert_eq!(vault.get("KEY"), Some("updated"));
    }
}
