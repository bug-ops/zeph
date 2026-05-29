// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for plugin operations.

use std::path::PathBuf;

/// Errors that can occur during plugin install, remove, or list operations.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The plugin manifest (`plugin.toml`) is missing or cannot be parsed.
    #[error("invalid plugin manifest: {0}")]
    InvalidManifest(String),

    /// The plugin name is invalid (empty, contains path separators, or reserved).
    #[error("invalid plugin name {name:?}: {reason}")]
    InvalidName { name: String, reason: String },

    /// A plugin MCP entry declares a command not in `mcp.allowed_commands`.
    #[error(
        "plugin MCP server {id:?} spawns command {command:?}, which is not in mcp.allowed_commands"
    )]
    DisallowedMcpCommand { id: String, command: String },

    /// A plugin skill name conflicts with an existing managed (user) skill.
    #[error("plugin skill {name:?} conflicts with an existing managed skill")]
    SkillNameConflictWithManaged { name: String },

    /// A plugin skill name conflicts with a compile-time bundled skill.
    #[error("plugin skill {name:?} conflicts with a bundled skill")]
    SkillNameConflictWithBundled { name: String },

    /// A plugin skill name conflicts with a skill from another installed plugin.
    #[error("plugin skill {name:?} conflicts with skill from plugin {plugin:?}")]
    SkillNameConflictWithPlugin { name: String, plugin: String },

    /// A plugin's `[config]` section contains a key not in the tighten-only safelist.
    #[error(
        "plugin config overlay key {key:?} is not allowed; only tools.blocked_commands, tools.allowed_commands, and skills.disambiguation_threshold may be overridden"
    )]
    UnsafeOverlay { key: String },

    /// A `[[skills]] path` entry does not contain a valid `SKILL.md` file.
    #[error("plugin skill entry at {path:?} does not contain a SKILL.md file")]
    SkillEntryMissing { path: PathBuf },

    /// The plugin directory does not exist or cannot be read.
    #[error("plugin not found: {name}")]
    NotFound { name: String },

    /// The plugin source path or URL is invalid.
    #[error("invalid plugin source {path:?}: {reason}")]
    InvalidSource { path: String, reason: String },

    /// A filesystem operation failed.
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// TOML serialization/deserialization error.
    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),

    /// TOML serialization error.
    #[error("TOML serialization error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    /// The SKILL.md semantic compliance scan rejected the skill.
    ///
    /// Raised for Stage-1 regex matches treated as blocking in the ephemeral install
    /// context (`add_remote_ephemeral`). Stage-2 LLM scan returns a string error
    /// via the command layer, not this variant.
    #[error("skill {skill:?} failed semantic compliance scan: {reason}")]
    SemanticViolation { skill: String, reason: String },

    /// SHA-256 digest of a downloaded archive does not match the expected value.
    ///
    /// Returned by [`crate::manager::PluginManager::add_remote`] when the caller
    /// supplies an `expected_sha256` and the download does not match.
    /// Do not install or extract the archive — it may have been tampered with.
    #[error(
        "plugin archive integrity check failed: expected sha256={expected}, got sha256={actual}"
    )]
    IntegrityCheckFailed { expected: String, actual: String },

    /// HTTP download of a remote plugin archive failed.
    #[error("failed to download plugin from {url}: {reason}")]
    DownloadFailed { url: String, reason: String },

    /// Attempted to remove or disable a plugin that other enabled plugins depend on.
    #[error("Plugin '{name}' is required by: {dependents}. Disable them first:\n{hints}")]
    DependencyRequired {
        /// The plugin that was requested to be removed or disabled.
        name: String,
        /// Comma-separated list of dependent plugin names.
        dependents: String,
        /// Newline-separated disable hints, one per dependent.
        hints: String,
    },

    /// A dependency cycle was detected while enabling a plugin.
    #[error("dependency cycle detected while enabling plugin '{name}': {cycle}")]
    DependencyCycle {
        /// The plugin being enabled when the cycle was found.
        name: String,
        /// Human-readable description of the cycle path.
        cycle: String,
    },

    /// A declared dependency plugin is not installed.
    #[error("plugin '{name}' requires dependency '{dependency}' which is not installed")]
    MissingDependency {
        /// The plugin declaring the dependency.
        name: String,
        /// The missing dependency name.
        dependency: String,
    },

    /// The URL supplied to `--plugin-url` uses a non-HTTPS scheme.
    ///
    /// Only `https://` is accepted for ephemeral plugin loading. `http://` and
    /// any other scheme are rejected to prevent MITM attacks against session-scoped
    /// plugin archives (security invariant INV-EPH-1).
    #[error("insecure URL scheme for --plugin-url: {0} (only https:// is accepted)")]
    InsecureUrl(String),
}
