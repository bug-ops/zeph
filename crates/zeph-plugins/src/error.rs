// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for plugin operations.

use std::path::PathBuf;

/// Errors that can occur during plugin install, remove, or list operations.
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
}
