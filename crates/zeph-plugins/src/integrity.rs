// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plugin manifest integrity registry.
//!
//! Stores sha256 digests of each installed `.plugin.toml` in a single
//! `<data_root>/.plugin-integrity.toml` file (sibling of `plugins_dir`).
//!
//! # Security model
//!
//! This is a *tamper-detection hint*, not a cryptographic integrity guarantee:
//! an attacker with write access to `<data_root>` can modify both the manifest
//! and the registry. The check raises the bar against accidental or opportunistic
//! TOCTOU attacks (e.g. replacing a validated manifest before it is loaded).
//!
//! # Forward compatibility
//!
//! Plugins installed with a pre-integrity build, or during an install interrupted
//! between manifest write and registry save, have no digest recorded and will load
//! without verification. To restore protection: `zeph plugin remove <name> && zeph
//! plugin add <source>`.
//!
//! # Concurrency
//!
//! Every mutation goes through [`IntegrityRegistry::update`], which holds an exclusive
//! advisory lock on a sibling `.lock` file for the whole load-modify-save sequence, so
//! concurrent `plugin add`/`remove` processes never lose each other's entries. The lock
//! file is never deleted (removing it would let two processes lock different inodes).

use std::collections::HashMap;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::error::PluginError;

/// Maximum time [`IntegrityRegistry::update`] waits for the registry lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// In-memory view of `<data_root>/.plugin-integrity.toml`.
///
/// Maps plugin name to the lowercase hex sha256 of its installed `.plugin.toml` bytes.
#[derive(Debug, Default)]
pub(crate) struct IntegrityRegistry {
    entries: HashMap<String, String>,
}

impl IntegrityRegistry {
    /// Default registry path: `<data_root>/.plugin-integrity.toml`.
    pub(crate) fn default_path() -> PathBuf {
        zeph_config::default_integrity_registry_path()
    }

    /// Load the registry from `path`.
    ///
    /// Missing file or unparseable content is silently treated as an empty registry
    /// (forward-compat with pre-integrity installs).
    pub(crate) fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "integrity registry read failed; treating as empty");
                return Self::default();
            }
        };
        let Ok(text) = String::from_utf8(bytes) else {
            tracing::warn!(path = %path.display(), "integrity registry invalid UTF-8; treating as empty");
            return Self::default();
        };
        let table: toml::Value = match toml::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "integrity registry unparseable; treating as empty");
                return Self::default();
            }
        };
        let entries = table
            .as_table()
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        Self { entries }
    }

    /// Persist the registry to `path`, creating parent directories as needed.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub(crate) fn save(&self, path: &Path) -> Result<(), PluginError> {
        let parent = path.parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| PluginError::Io {
            path: parent.to_owned(),
            source,
        })?;

        let mut table = toml::value::Table::new();
        for (k, v) in &self.entries {
            table.insert(k.clone(), toml::Value::String(v.clone()));
        }
        let text = toml::to_string(&toml::Value::Table(table))?;

        zeph_common::fs_secure::atomic_write_private(path, text.as_bytes()).map_err(|source| {
            PluginError::Io {
                path: path.to_owned(),
                source,
            }
        })
    }

    /// Atomically load, modify and save the registry at `path` under an exclusive lock.
    ///
    /// Waits up to [`LOCK_TIMEOUT`] for the sibling `<path>.lock` file (owner-only, symlinks
    /// refused) so concurrent callers, including other processes, are serialized. The lock is
    /// released when this function returns. `f` runs on the freshly loaded registry; the
    /// registry is saved only if `f` succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::LockTimeout`] if the lock is not acquired in time, an I/O error
    /// if the lock file cannot be opened or the registry saved, or whatever `f` returns.
    pub(crate) fn update(
        path: &Path,
        f: impl FnOnce(&mut Self) -> Result<(), PluginError>,
    ) -> Result<(), PluginError> {
        Self::update_with_timeout(path, LOCK_TIMEOUT, f)
    }

    fn update_with_timeout(
        path: &Path,
        timeout: Duration,
        f: impl FnOnce(&mut Self) -> Result<(), PluginError>,
    ) -> Result<(), PluginError> {
        let parent = path.parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| PluginError::Io {
            path: parent.to_owned(),
            source,
        })?;
        let lock_path = path.with_added_extension("lock");
        let _lock = acquire_lock(&lock_path, timeout)?;
        let mut registry = Self::load(path);
        f(&mut registry)?;
        registry.save(path)
    }

    /// Compute the sha256 of `toml_path` bytes and record it under `plugin_name`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read.
    pub(crate) fn record(
        &mut self,
        plugin_name: &str,
        toml_path: &Path,
    ) -> Result<(), PluginError> {
        let bytes = std::fs::read(toml_path).map_err(|source| PluginError::Io {
            path: toml_path.to_owned(),
            source,
        })?;
        let digest = sha256_hex(&bytes);
        self.entries.insert(plugin_name.to_owned(), digest);
        Ok(())
    }

    /// Remove the registry entry for `plugin_name`.
    pub(crate) fn remove(&mut self, plugin_name: &str) {
        self.entries.remove(plugin_name);
    }

    /// Verify `toml_path` against the recorded digest for `plugin_name`.
    ///
    /// - Missing entry → `Ok(true)` with a `debug!` log (pre-integrity install).
    /// - Digest match → `Ok(true)`.
    /// - Digest mismatch → `Ok(false)` with the expected and actual hex strings.
    /// - File unreadable → `Err(...)`.
    ///
    /// # Errors
    ///
    /// Returns an error if `toml_path` cannot be read.
    pub(crate) fn verify(
        &self,
        plugin_name: &str,
        toml_path: &Path,
    ) -> Result<VerifyResult, PluginError> {
        let Some(expected) = self.entries.get(plugin_name) else {
            tracing::debug!(
                plugin = %plugin_name,
                "no integrity record; manifest not verified (pre-feature install or interrupted install)"
            );
            return Ok(VerifyResult::Missing);
        };
        let bytes = std::fs::read(toml_path).map_err(|source| PluginError::Io {
            path: toml_path.to_owned(),
            source,
        })?;
        let actual = sha256_hex(&bytes);
        if &actual == expected {
            Ok(VerifyResult::Match)
        } else {
            Ok(VerifyResult::Mismatch {
                expected: expected.clone(),
                actual,
            })
        }
    }
}

/// Outcome of an [`IntegrityRegistry::verify`] call.
#[derive(Debug)]
pub(crate) enum VerifyResult {
    /// No entry recorded — forward-compat; load is allowed.
    Missing,
    /// Digest matches — manifest is intact.
    Match,
    /// Digest does not match — possible tampering.
    Mismatch { expected: String, actual: String },
}

/// Open `lock_path` owner-only (refusing symlinks) and take an exclusive advisory lock,
/// polling until `timeout` elapses. The lock is released when the returned file is dropped.
fn acquire_lock(lock_path: &Path, timeout: Duration) -> Result<File, PluginError> {
    let io_err = |source| PluginError::Io {
        path: lock_path.to_owned(),
        source,
    };
    if std::fs::symlink_metadata(lock_path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io_err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "lock file is a symlink",
        )));
    }
    let file = zeph_common::fs_secure::append_private(lock_path).map_err(io_err)?;
    #[cfg(unix)]
    zeph_common::fs_secure::restrict_to_owner(lock_path).map_err(io_err)?;

    let start = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::Error(e)) => return Err(io_err(e)),
            Err(TryLockError::WouldBlock) => {
                let waited = start.elapsed();
                if waited >= timeout {
                    return Err(PluginError::LockTimeout {
                        path: lock_path.to_owned(),
                        waited,
                    });
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
        }
    }
}

/// Compute the lowercase hex sha256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use tempfile::TempDir;

    use super::*;

    fn tmp_registry(dir: &TempDir) -> PathBuf {
        dir.path().join("registry.toml")
    }

    #[test]
    fn integrity_registry_round_trip() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"[plugin]\nname = \"test\"\n").unwrap();

        let mut reg = IntegrityRegistry::load(&reg_path);
        reg.record("test-plugin", &toml_path).unwrap();
        reg.save(&reg_path).unwrap();

        let reg2 = IntegrityRegistry::load(&reg_path);
        assert_matches!(
            reg2.verify("test-plugin", &toml_path).unwrap(),
            VerifyResult::Match
        );
    }

    #[test]
    fn integrity_registry_mismatch_detected() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"[plugin]\nname = \"original\"\n").unwrap();

        let mut reg = IntegrityRegistry::load(&reg_path);
        reg.record("test-plugin", &toml_path).unwrap();
        reg.save(&reg_path).unwrap();

        // Tamper the manifest.
        std::fs::write(&toml_path, b"[plugin]\nname = \"tampered\"\n").unwrap();

        let reg2 = IntegrityRegistry::load(&reg_path);
        assert_matches!(
            reg2.verify("test-plugin", &toml_path).unwrap(),
            VerifyResult::Mismatch { .. }
        );
    }

    #[test]
    fn integrity_registry_missing_entry_allowed() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"[plugin]\nname = \"test\"\n").unwrap();

        let reg = IntegrityRegistry::load(&reg_path);
        // No entry recorded — should be allowed.
        assert_matches!(
            reg.verify("unknown-plugin", &toml_path).unwrap(),
            VerifyResult::Missing
        );
    }

    #[test]
    fn corrupt_integrity_registry_treated_as_empty() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        std::fs::write(&reg_path, b"not valid toml !!!").unwrap();

        let reg = IntegrityRegistry::load(&reg_path);
        // Should not panic; entries should be empty.
        assert!(reg.entries.is_empty());
    }

    #[test]
    fn integrity_registry_remove_clears_entry() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"[plugin]\nname = \"test\"\n").unwrap();

        let mut reg = IntegrityRegistry::load(&reg_path);
        reg.record("test-plugin", &toml_path).unwrap();
        reg.save(&reg_path).unwrap();

        let mut reg2 = IntegrityRegistry::load(&reg_path);
        reg2.remove("test-plugin");
        reg2.save(&reg_path).unwrap();

        let reg3 = IntegrityRegistry::load(&reg_path);
        assert!(reg3.entries.is_empty());
    }

    #[test]
    fn update_waits_for_held_lock_then_proceeds() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"x").unwrap();

        let held =
            zeph_common::fs_secure::append_private(&reg_path.with_added_extension("lock")).unwrap();
        held.lock().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let reg_path = reg_path.clone();
            std::thread::spawn(move || {
                let r = IntegrityRegistry::update(&reg_path, |r| r.record("p", &toml_path));
                tx.send(()).unwrap();
                r
            })
        };

        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(held);
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap().unwrap();
        assert_eq!(IntegrityRegistry::load(&reg_path).entries.len(), 1);
    }

    #[test]
    fn update_times_out_when_lock_held() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let held =
            zeph_common::fs_secure::append_private(&reg_path.with_added_extension("lock")).unwrap();
        held.lock().unwrap();

        let err =
            IntegrityRegistry::update_with_timeout(&reg_path, Duration::from_millis(100), |_| {
                Ok(())
            })
            .unwrap_err();
        assert_matches!(err, PluginError::LockTimeout { .. });
        assert!(!reg_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn update_refuses_symlinked_lock_file() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let target = dir.path().join("victim");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, reg_path.with_added_extension("lock")).unwrap();

        assert!(IntegrityRegistry::update(&reg_path, |_| Ok(())).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn lock_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let lock_path = reg_path.with_added_extension("lock");
        std::fs::write(&lock_path, b"").unwrap();
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        IntegrityRegistry::update(&reg_path, |_| Ok(())).unwrap();
        let mode = std::fs::metadata(&lock_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn update_failure_does_not_save_and_does_not_block_later_runs() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let missing = dir.path().join("missing.toml");

        let err = IntegrityRegistry::update(&reg_path, |r| r.record("p", &missing));
        assert!(err.is_err());
        assert!(!reg_path.exists());

        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"x").unwrap();
        IntegrityRegistry::update(&reg_path, |r| r.record("p", &toml_path)).unwrap();
        IntegrityRegistry::update(&reg_path, |r| {
            r.remove("p");
            Ok(())
        })
        .unwrap();
        assert!(IntegrityRegistry::load(&reg_path).entries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_owner_only_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        IntegrityRegistry::update(&reg_path, |_| Ok(())).unwrap();
        let mode = std::fs::metadata(&reg_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn sha256_hex_stable() {
        // Regression: sha256 of empty bytes must be stable.
        let digest = sha256_hex(b"");
        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
