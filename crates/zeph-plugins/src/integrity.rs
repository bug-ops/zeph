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

    /// Load the registry from `path`, failing on any damage.
    ///
    /// A missing file is an empty registry (first install). Every other problem (read error,
    /// invalid UTF-8, invalid TOML, a non-string value) is an error, so a read-modify-write
    /// never overwrites digests it could not read and verification never treats damage as
    /// "nothing recorded".
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::IntegrityRegistryCorrupt`] when the file exists but is unusable.
    pub(crate) fn load_strict(path: &Path) -> Result<Self, PluginError> {
        let corrupt = |reason: String| PluginError::IntegrityRegistryCorrupt {
            path: path.to_owned(),
            reason,
        };
        let Some(table) = read_table(path).map_err(|e| corrupt(e.to_string()))? else {
            return Ok(Self::default());
        };
        let mut entries = HashMap::with_capacity(table.len());
        for (k, v) in table {
            match v {
                toml::Value::String(s) => {
                    entries.insert(k, s);
                }
                other => {
                    return Err(corrupt(format!(
                        "entry {k:?} is a {}, expected a string",
                        other.type_str()
                    )));
                }
            }
        }
        Ok(Self { entries })
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
    /// Returns [`PluginError::LockTimeout`] if the lock is not acquired in time,
    /// [`PluginError::IntegrityRegistryCorrupt`] if the existing registry cannot be read (the
    /// file is left untouched), an I/O error if the lock file cannot be opened or the registry
    /// saved, or whatever `f` returns.
    pub(crate) fn update(
        path: &Path,
        f: impl FnOnce(&mut Self) -> Result<(), PluginError>,
    ) -> Result<(), PluginError> {
        Self::update_with_timeout(path, LOCK_TIMEOUT, f)
    }

    /// Like [`Self::update`], but turns a failure into a user-facing warning instead of an error.
    ///
    /// For callers whose filesystem change has already happened and cannot be rolled back
    /// (add, remove, auto-update swap): the failure is logged and returned as `Some(text)` for
    /// the caller to surface. `effect` states what the failure leaves behind.
    pub(crate) fn update_reporting(
        path: &Path,
        effect: &str,
        f: impl FnOnce(&mut Self) -> Result<(), PluginError>,
    ) -> Option<String> {
        let e = Self::update(path, f).err()?;
        tracing::warn!(error = %e, effect, "failed to update integrity registry");
        Some(warning_text(effect, &e))
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
        let mut registry = Self::load_strict(path)?;
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

    /// Verify `bytes` against the recorded digest for `plugin_name`.
    ///
    /// Takes the buffer the caller parsed, not a path, so the digest covers exactly the content
    /// that gets used (no second read, no TOCTOU window).
    ///
    /// - Missing entry → [`VerifyResult::Missing`] with a `debug!` log (pre-integrity install).
    /// - Digest match → [`VerifyResult::Match`].
    /// - Digest mismatch → [`VerifyResult::Mismatch`] with the expected and actual hex strings.
    pub(crate) fn verify_bytes(&self, plugin_name: &str, bytes: &[u8]) -> VerifyResult {
        let Some(expected) = self.entries.get(plugin_name) else {
            tracing::debug!(
                plugin = %plugin_name,
                "no integrity record; manifest not verified (pre-feature install or interrupted install)"
            );
            return VerifyResult::Missing;
        };
        let actual = sha256_hex(bytes);
        if &actual == expected {
            VerifyResult::Match
        } else {
            VerifyResult::Mismatch {
                expected: expected.clone(),
                actual,
            }
        }
    }
}

/// Why [`read_table`] failed.
#[derive(Debug, thiserror::Error)]
enum TableError {
    #[error("read failed: {0}")]
    Read(std::io::Error),
    #[error("{0}")]
    Content(String),
}

/// User-facing warning for a failed registry update. Only a damaged registry gets repair advice;
/// a lock timeout or I/O failure is transient, so the advice there is to retry, never to delete.
fn warning_text(effect: &str, e: &PluginError) -> String {
    match e {
        PluginError::IntegrityRegistryCorrupt { reason, .. } => format!(
            "{effect}: integrity registry unusable ({reason}); restore it, or delete it and run plugin add for each installed plugin before restarting"
        ),
        other => format!(
            "{effect}: integrity registry could not be updated ({other}); fix the cause and run plugin add again for this plugin"
        ),
    }
}

/// Reason text for `e`: the inner reason for a corrupt registry (the variant's own message
/// carries repair advice that callers would duplicate), the full message otherwise.
pub(crate) fn damage_reason(e: &PluginError) -> String {
    match e {
        PluginError::IntegrityRegistryCorrupt { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

/// Read `path` as a TOML table. `Ok(None)` means the file does not exist.
fn read_table(path: &Path) -> Result<Option<toml::value::Table>, TableError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(TableError::Read(e)),
    };
    let text =
        String::from_utf8(bytes).map_err(|_| TableError::Content("not valid UTF-8".to_owned()))?;
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| TableError::Content(format!("invalid TOML: {e}")))
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

        let mut reg = IntegrityRegistry::load_strict(&reg_path).unwrap();
        reg.record("test-plugin", &toml_path).unwrap();
        reg.save(&reg_path).unwrap();

        let reg2 = IntegrityRegistry::load_strict(&reg_path).unwrap();
        assert_matches!(
            reg2.verify_bytes("test-plugin", &std::fs::read(&toml_path).unwrap()),
            VerifyResult::Match
        );
    }

    #[test]
    fn integrity_registry_mismatch_detected() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"[plugin]\nname = \"original\"\n").unwrap();

        let mut reg = IntegrityRegistry::load_strict(&reg_path).unwrap();
        reg.record("test-plugin", &toml_path).unwrap();
        reg.save(&reg_path).unwrap();

        // Tamper the manifest.
        std::fs::write(&toml_path, b"[plugin]\nname = \"tampered\"\n").unwrap();

        let reg2 = IntegrityRegistry::load_strict(&reg_path).unwrap();
        assert_matches!(
            reg2.verify_bytes("test-plugin", &std::fs::read(&toml_path).unwrap()),
            VerifyResult::Mismatch { .. }
        );
    }

    #[test]
    fn verify_bytes_hashes_the_given_buffer_not_the_file_on_disk() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"[plugin]\nname = \"original\"\n").unwrap();

        let mut reg = IntegrityRegistry::load_strict(&reg_path).unwrap();
        reg.record("test-plugin", &toml_path).unwrap();

        assert_matches!(
            reg.verify_bytes("test-plugin", b"[plugin]\nname = \"swapped\"\n"),
            VerifyResult::Mismatch { .. }
        );
        assert_matches!(
            reg.verify_bytes("test-plugin", b"[plugin]\nname = \"original\"\n"),
            VerifyResult::Match
        );
    }

    #[test]
    fn integrity_registry_missing_entry_allowed() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"[plugin]\nname = \"test\"\n").unwrap();

        let reg = IntegrityRegistry::load_strict(&reg_path).unwrap();
        // No entry recorded — should be allowed.
        assert_matches!(
            reg.verify_bytes("unknown-plugin", b"[plugin]\nname = \"test\"\n"),
            VerifyResult::Missing
        );
    }

    #[test]
    fn corrupt_integrity_registry_load_errors() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        std::fs::write(&reg_path, b"not valid toml !!!").unwrap();

        assert_matches!(
            IntegrityRegistry::load_strict(&reg_path),
            Err(PluginError::IntegrityRegistryCorrupt { .. })
        );
    }

    #[test]
    fn warning_text_for_transient_failures_never_advises_deleting() {
        let lock = PluginError::LockTimeout {
            path: PathBuf::from("r.lock"),
            waited: Duration::from_secs(10),
        };
        let io = PluginError::Io {
            path: PathBuf::from("r"),
            source: std::io::Error::other("disk"),
        };
        for e in [lock, io] {
            let text = warning_text("effect", &e);
            assert!(text.contains("could not be updated"), "{text}");
            assert!(!text.contains("delete"), "{text}");
            assert!(!text.contains("unusable"), "{text}");
        }
    }

    #[test]
    fn update_reporting_warns_when_registry_path_is_a_directory() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        std::fs::create_dir(&reg_path).unwrap();

        let warning = IntegrityRegistry::update_reporting(&reg_path, "effect", |_| Ok(()))
            .expect("unreadable registry must produce a warning");
        assert!(warning.starts_with("effect: "), "{warning}");
        assert!(reg_path.is_dir());
    }

    #[test]
    fn update_reporting_warns_on_corrupt_registry_and_is_silent_on_success() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        assert!(IntegrityRegistry::update_reporting(&reg_path, "effect", |_| Ok(())).is_none());

        std::fs::write(&reg_path, b"not valid toml !!!").unwrap();
        let warning = IntegrityRegistry::update_reporting(&reg_path, "effect", |_| Ok(()))
            .expect("corrupt registry must produce a warning");
        assert!(warning.starts_with("effect: integrity registry unusable (invalid TOML"));
        assert!(
            !warning.contains("is unusable"),
            "inner reason only: {warning}"
        );
    }

    #[test]
    fn load_strict_missing_file_is_empty() {
        let dir = TempDir::new().unwrap();
        let reg = IntegrityRegistry::load_strict(&tmp_registry(&dir)).unwrap();
        assert!(reg.entries.is_empty());
    }

    #[test]
    fn update_on_corrupt_registry_errors_and_leaves_file_untouched() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"x").unwrap();
        let corrupt: &[&[u8]] = &[
            b"not valid toml !!!",
            b"\xff\xfe",
            b"good = \"abc\"\nbad = 5\n",
        ];
        for content in corrupt {
            std::fs::write(&reg_path, content).unwrap();
            let err =
                IntegrityRegistry::update(&reg_path, |r| r.record("p", &toml_path)).unwrap_err();
            assert_matches!(err, PluginError::IntegrityRegistryCorrupt { .. });
            assert!(err.to_string().contains(&reg_path.display().to_string()));
            assert_eq!(std::fs::read(&reg_path).unwrap(), *content);
        }
    }

    #[cfg(unix)]
    #[test]
    fn update_on_unreadable_registry_errors_and_leaves_file_untouched() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        std::fs::write(&reg_path, b"p = \"abc\"\n").unwrap();
        std::fs::set_permissions(&reg_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&reg_path).is_ok() {
            return; // running as root: permissions do not apply
        }

        let err = IntegrityRegistry::update(&reg_path, |_| Ok(())).unwrap_err();
        std::fs::set_permissions(&reg_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_matches!(err, PluginError::IntegrityRegistryCorrupt { .. });
        assert_eq!(std::fs::read(&reg_path).unwrap(), b"p = \"abc\"\n");
    }

    #[test]
    fn integrity_registry_remove_clears_entry() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"[plugin]\nname = \"test\"\n").unwrap();

        let mut reg = IntegrityRegistry::load_strict(&reg_path).unwrap();
        reg.record("test-plugin", &toml_path).unwrap();
        reg.save(&reg_path).unwrap();

        let mut reg2 = IntegrityRegistry::load_strict(&reg_path).unwrap();
        reg2.remove("test-plugin");
        reg2.save(&reg_path).unwrap();

        let reg3 = IntegrityRegistry::load_strict(&reg_path).unwrap();
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
        assert_eq!(
            IntegrityRegistry::load_strict(&reg_path)
                .unwrap()
                .entries
                .len(),
            1
        );
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
        assert!(
            IntegrityRegistry::load_strict(&reg_path)
                .unwrap()
                .entries
                .is_empty()
        );
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
