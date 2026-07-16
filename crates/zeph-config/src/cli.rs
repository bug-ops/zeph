// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Session-scoped CLI configuration: bare mode, JSON output, and auto-approval flags.

use serde::{Deserialize, Serialize};

/// Session-scoped CLI overrides loaded from the `[cli]` TOML section.
///
/// Command-line flags take priority over these values. This section has no
/// effect on Telegram, Discord, Slack, or ACP sessions.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
pub struct CliConfig {
    /// Enable bare mode (skip skills, memory, MCP, scheduler, watchers).
    pub bare: bool,
    /// Enable safe mode (skip ZEPH.md/CLAUDE.md/AGENTS.md, plugins, skills,
    /// hooks, and MCP servers for this session). Session-scoped only — never
    /// persisted to `config.toml` (`#[serde(skip)]`), mirroring `--bare`'s
    /// troubleshooting-flag precedent but gating a disjoint set of subsystems.
    #[serde(skip)]
    pub safe_mode: bool,
    /// Force MCP image passthrough (spec-072) off for this session (`--no-mcp-media`),
    /// regardless of per-server `media_passthrough` or `[mcp.media]` config. Session-scoped
    /// only — never persisted to `config.toml` (`#[serde(skip)]`), mirroring `safe_mode`.
    #[serde(skip)]
    pub no_mcp_media: bool,
    /// Emit structured JSON events (JSONL) to stdout. Forces logs to stderr.
    pub json: bool,
    /// Auto-approve trust-gate prompts (`-y` / `--auto`).
    pub auto: bool,
    /// Loop command configuration.
    #[serde(rename = "loop")]
    pub loop_: LoopConfig,
    /// Tool allowlist for CLI/TUI sessions. `None` means all tools are permitted.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

/// Configuration for the `/loop` command.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LoopConfig {
    /// Minimum allowed interval between loop ticks (seconds). Floor enforced at parse time.
    pub min_interval_secs: u64,
    /// Maximum number of concurrent loops. Reserved for future use; always 1 in v1.
    pub max_concurrent: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            min_interval_secs: 5,
            max_concurrent: 1,
        }
    }
}
