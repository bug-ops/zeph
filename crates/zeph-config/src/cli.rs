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
pub struct CliConfig {
    /// Enable bare mode (skip skills, memory, MCP, scheduler, watchers).
    pub bare: bool,
    /// Emit structured JSON events (JSONL) to stdout. Forces logs to stderr.
    pub json: bool,
    /// Auto-approve trust-gate prompts (`-y` / `--auto`).
    pub auto: bool,
    /// Loop command configuration.
    #[serde(rename = "loop")]
    pub loop_: LoopConfig,
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
