// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_common::secret::VaultError;

/// Error type for configuration operations.
///
/// Covers file I/O, TOML parsing, validation, and vault resolution.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config validation failed: {0}")]
    Validation(String),
    #[error("vault error: {0}")]
    Vault(#[from] VaultError),
}
