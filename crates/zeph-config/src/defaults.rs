// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

/// Legacy project-relative `SQLite` path (pre-XDG migration).
///
/// Used only for detecting and migrating old configs that still reference this path.
pub const DEFAULT_SQLITE_PATH: &str = ".zeph/data/zeph.db";
/// Legacy project-relative skills directory (pre-XDG migration).
pub const DEFAULT_SKILLS_DIR: &str = ".zeph/skills";
/// Legacy project-relative debug output directory (pre-XDG migration).
pub const DEFAULT_DEBUG_DIR: &str = ".zeph/debug";
/// Legacy project-relative log file path (pre-XDG migration).
pub const DEFAULT_LOG_FILE: &str = ".zeph/logs/zeph.log";

#[cfg(any(target_os = "macos", target_os = "windows"))]
const PLATFORM_APP_DIR_NAME: &str = "Zeph";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const PLATFORM_APP_DIR_NAME: &str = "zeph";

/// Platform default writable data root.
///
/// Examples:
/// - Linux: `~/.local/share/zeph`
/// - macOS: `~/Library/Application Support/Zeph`
/// - Windows: `%LOCALAPPDATA%\Zeph`
pub(crate) fn default_runtime_data_root() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(PLATFORM_APP_DIR_NAME)
}

/// Returns the platform-appropriate default path for the `SQLite` database.
///
/// # Examples
///
/// ```
/// let path = zeph_config::default_sqlite_path();
/// assert!(path.ends_with("zeph.db"));
/// ```
#[must_use]
pub fn default_sqlite_path() -> String {
    default_runtime_data_root()
        .join("data")
        .join("zeph.db")
        .to_string_lossy()
        .into_owned()
}

/// Returns the default vault/config directory for skills.
///
/// Mirrors the logic in zeph-core's `default_vault_dir` but without depending on that crate.
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

/// Returns the platform-appropriate default directory for user skills.
///
/// # Examples
///
/// ```
/// let dir = zeph_config::default_skills_dir();
/// assert!(dir.ends_with("skills"));
/// ```
#[must_use]
pub fn default_skills_dir() -> String {
    default_vault_dir()
        .join("skills")
        .to_string_lossy()
        .into_owned()
}

/// Returns the platform-appropriate default directory for debug output.
#[must_use]
pub fn default_debug_dir() -> PathBuf {
    default_runtime_data_root().join("debug")
}

/// Returns the platform-appropriate default path for the log file.
///
/// # Examples
///
/// ```
/// let path = zeph_config::default_log_file_path();
/// assert!(path.ends_with("zeph.log"));
/// ```
#[must_use]
pub fn default_log_file_path() -> String {
    default_runtime_data_root()
        .join("logs")
        .join("zeph.log")
        .to_string_lossy()
        .into_owned()
}

/// Returns the default `skills.paths` vector containing [`default_skills_dir`].
#[must_use]
pub fn default_skill_paths() -> Vec<String> {
    vec![default_skills_dir()]
}

pub(crate) fn default_log_file() -> String {
    default_log_file_path()
}

/// Alias for [`default_sqlite_path`] used as a serde default function.
#[must_use]
pub fn default_sqlite_path_field() -> String {
    default_sqlite_path()
}

pub(crate) fn default_debug_output_dir() -> PathBuf {
    default_debug_dir()
}

/// Returns `true` when `path` is the legacy project-relative `SQLite` path that must be migrated.
#[must_use]
pub fn is_legacy_default_sqlite_path(path: &str) -> bool {
    path == DEFAULT_SQLITE_PATH
}

/// Returns `true` when `path` is the legacy project-relative skills directory that must be migrated.
#[must_use]
pub fn is_legacy_default_skills_path(path: &str) -> bool {
    path == DEFAULT_SKILLS_DIR
}

/// Returns `true` when `path` is the legacy project-relative debug directory that must be migrated.
#[must_use]
pub fn is_legacy_default_debug_dir(path: &std::path::Path) -> bool {
    path == std::path::Path::new(DEFAULT_DEBUG_DIR)
}

/// Returns `true` when `path` is the legacy project-relative log file path that must be migrated.
#[must_use]
pub fn is_legacy_default_log_file(path: &str) -> bool {
    path == DEFAULT_LOG_FILE
}

/// Returns the platform-appropriate path for the plugin integrity registry.
///
/// The file is a sibling of `plugins/` inside the Zeph data root, not inside
/// the plugins directory itself.
///
/// - Linux: `~/.local/share/zeph/.plugin-integrity.toml`
/// - macOS: `~/Library/Application Support/Zeph/.plugin-integrity.toml`
/// - Windows: `%LOCALAPPDATA%\Zeph\.plugin-integrity.toml`
#[must_use]
pub fn default_integrity_registry_path() -> PathBuf {
    default_runtime_data_root().join(".plugin-integrity.toml")
}

pub(crate) fn default_true() -> bool {
    true
}
