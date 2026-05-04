// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Named execution environment configuration (`[execution]` TOML section).
//!
//! Provides [`ExecutionConfig`] — the configuration type for the top-level `[execution]`
//! section — and [`EnvironmentConfig`] for each `[[execution.environments]]` entry.
//!
//! The `ShellExecutor` calls `ExecutionConfig::build_registry` at construction time to
//! produce a `HashMap<String, ExecutionContext>` that is consulted on every tool call.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Top-level `[execution]` configuration section.
///
/// # Config example
///
/// ```toml
/// [execution]
/// default_env = "repo"
///
/// [[execution.environments]]
/// name = "repo"
/// cwd = "/Users/me/Dev/myproject"
/// env = { CARGO_TARGET_DIR = "/tmp/cargo-target" }
///
/// [[execution.environments]]
/// name = "scratch"
/// cwd = "/tmp/scratch"
/// ```
///
/// # Note on case sensitivity
///
/// Environment names are **case-sensitive**. Convention is lowercase (`"repo"`, `"scratch"`).
/// An unknown `default_env` or `context.name` is a hard error at resolution time.
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct ExecutionConfig {
    /// Name of the environment applied when a `ToolCall` carries no explicit context
    /// and no `default_env` would otherwise be used.  This is the least-specific
    /// fallback layer in the CWD/env precedence stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_env: Option<String>,

    /// Named execution environments.  Each entry becomes a registry key that
    /// `ShellExecutor` consults when `ToolCall::context.name` is set.
    #[serde(default, rename = "environments")]
    pub environments: Vec<EnvironmentConfig>,
}

/// A single named execution environment entry (`[[execution.environments]]`).
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct EnvironmentConfig {
    /// Registry key.  Case-sensitive; convention is lowercase.
    pub name: String,
    /// Absolute or relative working directory for commands using this environment.
    ///
    /// Relative paths are resolved relative to the process CWD at registry-build time
    /// (i.e. agent startup).  Non-existent paths are a hard error.
    pub cwd: String,
    /// Extra environment variables injected into the subprocess for this environment.
    ///
    /// Because these originate from operator-authored TOML, registry contexts are
    /// *trusted*: the `env_blocklist` final filter pass is skipped.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}
