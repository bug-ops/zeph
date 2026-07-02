// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plugin lifecycle management: install, remove, list, enable, disable, and auto-update.
//!
//! The implementation is split across focused submodules:
//! - `install` — add/remove/enable/disable and dependency guards
//! - `registry` — remote download, archive extraction, auto-update, ephemeral install
//! - `security` — URL/name/overlay validation, MCP allowlisting, archive safety, skill scanning
//! - `store` — filesystem state and manifest reading

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::PluginName;

/// Result of a successful `plugin add` operation.
#[derive(Debug)]
pub struct AddResult {
    /// Installed plugin name.
    pub name: PluginName,
    /// Absolute path to the installed plugin root.
    ///
    /// Callers should pass each entry in `installed_skill_dirs` to
    /// [`zeph_skills::registry::SkillRegistry::register_hub_dir`] so the registry treats plugin
    /// subtrees as non-bundled regardless of any residual `.bundled` markers (S2 defense).
    pub plugin_root: PathBuf,
    /// Skill names registered from this plugin.
    pub installed_skills: Vec<String>,
    /// MCP server IDs declared by this plugin (require agent restart).
    pub mcp_server_ids: Vec<String>,
    /// Non-fatal warnings produced at install time.
    ///
    /// Currently populated when a plugin's `allowed_commands` overlay will
    /// have no effect because the host's base `tools.shell.allowed_commands`
    /// is empty (see issue #3149 — tighten-only semantics mean plugins
    /// cannot widen an empty base allowlist). Callers should surface these
    /// to the user alongside the success message (`eprintln!` on the CLI,
    /// appended to the output string on the TUI).
    pub warnings: Vec<String>,
}

/// Result of a successful `plugin remove` operation.
#[derive(Debug, Default)]
pub struct RemoveResult {
    /// Skill names unregistered.
    pub removed_skills: Vec<String>,
    /// MCP server IDs that were declared (require agent restart).
    pub removed_mcp_ids: Vec<String>,
}

/// Result of a successful `plugin disable` operation.
///
/// When `--force` is used and dependents exist, the disable proceeds and the list of
/// overridden dependents is returned so callers can surface a warning to the user.
#[derive(Debug, Default)]
pub struct DisableResult {
    /// Names of enabled plugins that depended on the disabled plugin.
    ///
    /// Non-empty only when the operation was forced past a dependency guard.
    /// Callers should warn the user that these plugins may misbehave until re-enabled.
    pub forced_over_dependents: Vec<String>,
}

/// Plain-data input for the Stage-2 LLM semantic scanner.
///
/// Collected by [`PluginManager::scan_targets`] from a plugin source tree before any files
/// are copied. The caller (core/commands layer) runs the async LLM scan and only proceeds
/// with installation when all verdicts are non-blocking.
#[derive(Debug, Clone)]
pub struct SkillScanInput {
    /// Skill name as declared in `SKILL.md` frontmatter, or the manifest path as fallback.
    pub skill_name: String,
    /// One-sentence description from `SKILL.md` frontmatter; represents the declared purpose.
    pub declared_purpose: String,
    /// Full SKILL.md body (frontmatter + content).
    pub skill_md: String,
}

/// Installed plugin metadata as returned by `plugin list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    /// Plugin name.
    pub name: PluginName,
    /// Plugin version.
    pub version: String,
    /// Plugin description.
    pub description: String,
    /// Absolute path to the installed plugin root.
    pub path: PathBuf,
    /// Skill names provided by this plugin (collected at list time to avoid re-reading manifests).
    pub skill_names: Vec<String>,
    /// Whether automatic updates are enabled for this plugin.
    ///
    /// Mirrors `plugin.auto_update` from the installed manifest. Populated at list time so
    /// [`PluginManager::check_auto_updates`] can filter candidates without re-reading manifests.
    pub auto_update: bool,
}

/// Install-time source metadata persisted as `.plugin-source.toml` alongside `.plugin.toml`.
///
/// Separating this from [`crate::manifest::PluginMeta`] keeps the author-facing `plugin.toml`
/// schema clean: plugin authors never set these fields; they are written exclusively by
/// [`PluginManager::add_remote`] and read by [`PluginManager::check_auto_updates`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginSource {
    /// URL from which the plugin archive was originally downloaded.
    ///
    /// `None` for plugins installed from local paths via [`PluginManager::add`].
    pub url: Option<String>,
    /// Lowercase hex SHA-256 of the installed archive bytes.
    ///
    /// Used by [`PluginManager::check_auto_updates`] to skip reinstalls when the remote
    /// archive has not changed.
    pub sha256: Option<String>,
}

/// Outcome of a single auto-update attempt.
///
/// One `AutoUpdateResult` is returned per plugin that had `auto_update = true`
/// when [`PluginManager::check_auto_updates`] ran.
#[derive(Debug)]
pub struct AutoUpdateResult {
    /// Plugin name.
    pub name: PluginName,
    /// Specific outcome for this plugin.
    pub status: AutoUpdateStatus,
}

/// Status of an individual auto-update attempt.
#[non_exhaustive]
#[derive(Debug)]
pub enum AutoUpdateStatus {
    /// Plugin was successfully updated.
    Updated {
        /// Version before the update.
        old_version: String,
        /// Version after the update.
        new_version: String,
    },
    /// Remote archive SHA-256 matches the installed copy — no action taken.
    UpToDate,
    /// Plugin has no persisted source URL (installed from a local path).
    NoSource,
    /// Update failed; plugin remains at its current version.
    Failed(String),
}

/// Manages plugin lifecycle: install, remove, list.
///
/// All operations are synchronous. Plugin watchers and agent config overlays are
/// applied separately by the agent bootstrap layer.
pub struct PluginManager {
    /// Root directory where plugins are installed (`~/.local/share/zeph/plugins/`).
    plugins_dir: PathBuf,
    /// Directory where managed (user-installed) skills live.
    managed_skills_dir: PathBuf,
    /// `mcp.allowed_commands` from the agent config. Used to validate plugin MCP entries.
    mcp_allowed_commands: Vec<String>,
    /// Host's base `tools.shell.allowed_commands`. Used to warn when a
    /// plugin overlay will be silently dropped because the base is empty
    /// (see issue #3149).
    base_allowed_commands: Vec<String>,
    /// Path to the integrity registry file. Injected so tests can use isolated paths.
    integrity_registry_path: PathBuf,
    /// Timeout in seconds for each HTTP phase of [`Self::add_remote`] (connect + body read).
    download_timeout_secs: u64,
}

impl PluginManager {
    /// Returns the canonical default plugins directory: `~/.local/share/zeph/plugins/`.
    ///
    /// Both the CLI and TUI must use this helper so they always point to the same directory.
    #[must_use]
    pub fn default_plugins_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("zeph")
            .join("plugins")
    }

    /// Create a new manager.
    ///
    /// # Parameters
    ///
    /// - `plugins_dir` — root installation directory for plugins.
    /// - `managed_skills_dir` — directory for user-managed skills (conflict detection).
    /// - `mcp_allowed_commands` — allowlist for MCP server commands from agent config.
    /// - `base_allowed_commands` — host's `tools.shell.allowed_commands`.
    ///   Used to emit a non-fatal warning when a plugin overlay would be
    ///   silently dropped at load time (tighten-only invariant).
    #[must_use]
    pub fn new(
        plugins_dir: PathBuf,
        managed_skills_dir: PathBuf,
        mcp_allowed_commands: Vec<String>,
        base_allowed_commands: Vec<String>,
    ) -> Self {
        let integrity_registry_path = crate::integrity::IntegrityRegistry::default_path();
        Self {
            plugins_dir,
            managed_skills_dir,
            mcp_allowed_commands,
            base_allowed_commands,
            integrity_registry_path,
            download_timeout_secs: 30,
        }
    }

    /// Override the HTTP download timeout used by [`Self::add_remote`].
    ///
    /// Each phase (connect and body read) is independently bounded by this value.
    /// The default is 30 seconds.
    #[must_use]
    pub fn with_download_timeout_secs(mut self, secs: u64) -> Self {
        self.download_timeout_secs = secs;
        self
    }

    /// Override the integrity registry path. Intended for tests only.
    #[cfg(test)]
    #[must_use]
    pub fn with_integrity_registry_path(mut self, path: PathBuf) -> Self {
        self.integrity_registry_path = path;
        self
    }
}

mod install;
mod registry;
mod security;
mod store;

#[cfg(test)]
mod tests;

pub use registry::{MAX_ARCHIVE_BYTES, download_and_extract};
#[cfg(test)]
pub(crate) use security::extract_archive;
pub use security::validate_url_scheme_ephemeral;
pub(crate) use security::{
    check_allowed_commands_overlay_effect, extract_archive_safe, scan_skill_entries,
    validate_manifest_for_install, validate_mcp_commands, validate_overlay_keys,
    validate_plugin_name, validate_url_scheme,
};
pub(crate) use store::{
    collect_skill_names, copy_dir_all, load_installed_manifest, parse_frontmatter_meta,
    strip_bundled_markers,
};
