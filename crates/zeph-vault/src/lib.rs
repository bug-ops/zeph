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

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use zeroize::Zeroizing;

// Secret and VaultError live in zeph-common (Layer 0) so that zeph-config (Layer 1)
// can reference them without creating a circular dependency.
pub use zeph_common::secret::{Secret, VaultError};

/// Pluggable secret retrieval backend.
///
/// Implement this trait to integrate a custom secret store (e.g. `HashiCorp` Vault, `AWS` Secrets
/// Manager, `1Password`). The crate ships three implementations out of the box:
/// [`AgeVaultProvider`], [`EnvVaultProvider`], and [`ArcAgeVaultProvider`].
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

/// Vault backend that reads secrets from environment variables.
///
/// This backend is designed for quick local development and CI environments where injecting
/// environment variables is convenient. In production, prefer [`AgeVaultProvider`].
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
    secrets: BTreeMap<String, Zeroizing<String>>,
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

fn encrypt_secrets(
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
#[cfg(any(test, feature = "mock"))]
#[derive(Default)]
pub struct MockVaultProvider {
    secrets: std::collections::BTreeMap<String, String>,
    /// Keys returned by `list_keys()` but absent from secrets (simulates `get_secret` returning
    /// `None`).
    listed_only: Vec<String>,
}

#[cfg(any(test, feature = "mock"))]
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

#[cfg(any(test, feature = "mock"))]
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

#[cfg(test)]
mod tests {
    #![allow(clippy::doc_markdown)]

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
