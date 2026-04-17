// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `plugin.toml` manifest schema.

use serde::{Deserialize, Serialize};

fn default_config_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

/// Top-level `plugin.toml` manifest.
///
/// # Example
///
/// ```toml
/// [plugin]
/// name = "git-workflows"
/// version = "0.1.0"
/// description = "Git workflow skills and MCP git server"
/// zeph-version = ">=0.19"
///
/// [[skills]]
/// path = "skills/git-commit"
///
/// [[mcp.servers]]
/// id = "git"
/// command = "mcp-git"
/// args = ["--repo", "."]
///
/// [config.tools]
/// blocked_commands = ["git push --force"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginManifest {
    /// Plugin metadata.
    pub plugin: PluginMeta,
    /// Skill entries bundled by this plugin.
    #[serde(default)]
    pub skills: Vec<SkillEntry>,
    /// MCP server declarations.
    #[serde(default)]
    pub mcp: McpSection,
    /// Tighten-only config overlay applied at startup.
    #[serde(default = "default_config_table")]
    pub config: toml::Value,
}

/// Plugin metadata from the `[plugin]` table.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginMeta {
    /// Canonical plugin name. Must be a valid identifier: `[a-z0-9][a-z0-9-]*`.
    pub name: String,
    /// Plugin version (informational).
    pub version: String,
    /// Short description shown in `zeph plugin list`.
    #[serde(default)]
    pub description: String,
    // zeph-version field intentionally omitted: version-gating is deferred to a future release
    // when the semver crate is added as a workspace dependency.
}

/// A single skill entry in `[[skills]]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillEntry {
    /// Relative path from the plugin root to the skill directory containing `SKILL.md`.
    pub path: String,
}

/// The `[mcp]` section.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpSection {
    /// MCP server declarations in `[[mcp.servers]]`.
    #[serde(default)]
    pub servers: Vec<PluginMcpServer>,
}

/// A single MCP server declaration in `[[mcp.servers]]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginMcpServer {
    /// Unique server ID. Used to de-duplicate across plugins.
    pub id: String,
    /// Command to spawn (stdio transport). Must be in `mcp.allowed_commands`.
    #[serde(default)]
    pub command: Option<String>,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
}
