// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plugin packaging and management for Zeph.
//!
//! A plugin is a directory (local or remote git) containing:
//! - `plugin.toml` — manifest describing the plugin (name, version, skills, MCP servers, config overlay)
//! - one or more skill directories with `SKILL.md` files
//! - optional MCP server declarations
//!
//! Plugins are installed to `~/.local/share/zeph/plugins/<name>/` and loaded at agent startup.
//!
//! # Security Model
//!
//! - Plugin config overlays are **tighten-only**: they can add to `blocked_commands`,
//!   narrow `allowed_commands`, or raise `disambiguation_threshold` — never loosen constraints.
//! - Plugin MCP entries are validated against `mcp.allowed_commands` at install time.
//! - `.bundled` markers are stripped recursively from all plugin skill trees.
//! - Skill name conflicts with managed, bundled, or other plugin skills are hard-errors at install.

pub mod error;
pub mod manager;
pub mod manifest;
pub mod overlay;

pub use error::PluginError;
pub use manager::{AddResult, InstalledPlugin, PluginManager, RemoveResult};
pub use manifest::PluginManifest;
pub use overlay::{ResolvedOverlay, apply_plugin_config_overlays};
