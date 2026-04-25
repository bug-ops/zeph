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

    /// Serialize and re-encrypt secrets to vault file using atomic write (temp + rename).
    ///
    /// Re-reads and re-parses the key file on each call. For CLI one-shot use this is
    /// acceptable; if used in a long-lived context consider caching the parsed identity.
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
    /// vault.set_secret_mut("MY_TOKEN".into(), "tok_abc123".into());
    /// vault.save()?;
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    pub fn save(&self) -> Result<(), AgeVaultError> {
        let key_str = Zeroizing::new(
            std::fs::read_to_string(&self.key_path).map_err(AgeVaultError::KeyRead)?,
        );
        let identity = parse_identity(&key_str)?;
        let ciphertext = encrypt_secrets(&identity, &self.secrets)?;
        atomic_write(&self.vault_path, &ciphertext)
    }

    /// Insert or update a secret in the in-memory map.
    ///
    /// Call [`save`][Self::save] afterwards to persist the change to disk.
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
    /// vault.set_secret_mut("API_KEY".into(), "sk-...".into());
    /// vault.save()?;
    /// # Ok::<_, zeph_vault::AgeVaultError>(())
    /// ```
    pub fn set_secret_mut(&mut self, key: String, value: String) {
        self.secrets.insert(key, Zeroizing::new(value));
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
