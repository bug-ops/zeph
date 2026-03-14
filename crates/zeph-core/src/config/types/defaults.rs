// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

/// Legacy project-relative runtime paths kept for compatibility checks and migration messaging.
pub const DEFAULT_SQLITE_PATH: &str = ".zeph/data/zeph.db";
pub const DEFAULT_SKILLS_DIR: &str = ".zeph/skills";
pub const DEFAULT_DEBUG_DIR: &str = ".zeph/debug";
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
pub(super) fn default_runtime_data_root() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(PLATFORM_APP_DIR_NAME)
}

#[must_use]
pub fn default_sqlite_path() -> String {
    default_runtime_data_root()
        .join("data")
        .join("zeph.db")
        .to_string_lossy()
        .into_owned()
}

#[must_use]
pub fn default_skills_dir() -> String {
    // Skills remain under the config-style root (`default_vault_dir`) so the default
    // path stays compatible with existing managed skill installation behavior.
    crate::vault::default_vault_dir()
        .join("skills")
        .to_string_lossy()
        .into_owned()
}

#[must_use]
pub fn default_debug_dir() -> PathBuf {
    default_runtime_data_root().join("debug")
}

#[must_use]
pub fn default_log_file_path() -> String {
    default_runtime_data_root()
        .join("logs")
        .join("zeph.log")
        .to_string_lossy()
        .into_owned()
}

pub(super) fn default_skill_paths() -> Vec<String> {
    vec![default_skills_dir()]
}

pub(super) fn default_log_file() -> String {
    default_log_file_path()
}

pub(super) fn default_sqlite_path_field() -> String {
    default_sqlite_path()
}

pub(super) fn default_debug_output_dir() -> PathBuf {
    default_debug_dir()
}

#[must_use]
pub fn is_legacy_default_sqlite_path(path: &str) -> bool {
    path == DEFAULT_SQLITE_PATH
}

#[must_use]
pub fn is_legacy_default_skills_path(path: &str) -> bool {
    path == DEFAULT_SKILLS_DIR
}

#[must_use]
pub fn is_legacy_default_debug_dir(path: &std::path::Path) -> bool {
    path == std::path::Path::new(DEFAULT_DEBUG_DIR)
}

#[must_use]
pub fn is_legacy_default_log_file(path: &str) -> bool {
    path == DEFAULT_LOG_FILE
}

pub(super) fn default_true() -> bool {
    true
}
