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
//! The registry is not locked against concurrent `plugin add`/`remove`. If two
//! processes install plugins simultaneously, the last writer's entry wins; the
//! other plugin will show as "integrity mismatch" on next startup until reinstalled.
//! TODO: file-level locking for M5 follow-up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

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
    pub(crate) fn save(&self, path: &Path) -> anyhow::Result<()> {
        let parent = path.parent().unwrap_or(std::path::Path::new("."));
        std::fs::create_dir_all(parent)?;

        let mut table = toml::value::Table::new();
        for (k, v) in &self.entries {
            table.insert(k.clone(), toml::Value::String(v.clone()));
        }
        let text = toml::to_string(&toml::Value::Table(table))?;

        // Atomic write: write to a sibling temp file, then rename into place.
        // Avoids a corrupt or empty registry if the process crashes mid-write.
        let tmp_path = path.with_extension("toml.tmp");
        std::fs::write(&tmp_path, &text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// Compute the sha256 of `toml_path` bytes and record it under `plugin_name`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read.
    pub(crate) fn record(&mut self, plugin_name: &str, toml_path: &Path) -> anyhow::Result<()> {
        let bytes = std::fs::read(toml_path)?;
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
    ) -> anyhow::Result<VerifyResult> {
        let Some(expected) = self.entries.get(plugin_name) else {
            tracing::debug!(
                plugin = %plugin_name,
                "no integrity record; manifest not verified (pre-feature install or interrupted install)"
            );
            return Ok(VerifyResult::Missing);
        };
        let bytes = std::fs::read(toml_path)?;
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

/// Compute the lowercase hex sha256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
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
        assert!(matches!(
            reg2.verify("test-plugin", &toml_path).unwrap(),
            VerifyResult::Match
        ));
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
        assert!(matches!(
            reg2.verify("test-plugin", &toml_path).unwrap(),
            VerifyResult::Mismatch { .. }
        ));
    }

    #[test]
    fn integrity_registry_missing_entry_allowed() {
        let dir = TempDir::new().unwrap();
        let reg_path = tmp_registry(&dir);
        let toml_path = dir.path().join("plugin.toml");
        std::fs::write(&toml_path, b"[plugin]\nname = \"test\"\n").unwrap();

        let reg = IntegrityRegistry::load(&reg_path);
        // No entry recorded — should be allowed.
        assert!(matches!(
            reg.verify("unknown-plugin", &toml_path).unwrap(),
            VerifyResult::Missing
        ));
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
    fn sha256_hex_stable() {
        // Regression: sha256 of empty bytes must be stable.
        let digest = sha256_hex(b"");
        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
