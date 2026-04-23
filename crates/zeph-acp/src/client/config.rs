// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

pub use zeph_config::{AcpSubagentsConfig, SubagentPresetConfig};

/// Configuration for a sub-agent subprocess.
///
/// Determines how the subprocess is spawned, what environment it sees, and
/// what working directories are used for the OS process and the ACP session.
///
/// # Examples
///
/// ```no_run
/// use zeph_acp::client::SubagentConfig;
///
/// let cfg = SubagentConfig {
///     command: "cargo run --quiet -- --acp".to_owned(),
///     ..SubagentConfig::default()
/// };
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubagentConfig {
    /// Shell command string to spawn (e.g. `"cargo run -- --acp"`).
    ///
    /// Split with `shell_words::split` to obtain the program and its arguments.
    pub command: String,

    /// Working directory for the spawned subprocess (`Command::current_dir`).
    ///
    /// When unset, the parent process's current directory is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_cwd: Option<PathBuf>,

    /// Working directory advertised to the sub-agent in the ACP `session/new` `cwd` field.
    ///
    /// Defaults to `process_cwd` when unset (the common case). Can differ when the agent
    /// binary lives elsewhere but should operate on a specific project directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_cwd: Option<PathBuf>,

    /// Extra environment variables to set in the subprocess.
    ///
    /// Applied *after* the whitelist expansion from `inherit_env`.
    /// `ZEPH_*` keys are rejected at spawn time.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Additional parent-process environment keys to forward to the subprocess.
    ///
    /// The subprocess starts with a cleared environment (`env_clear`) and then
    /// receives only these keys (if they exist in the parent) plus `env`.
    #[serde(default)]
    pub inherit_env: Vec<String>,

    /// Timeout in seconds for the `initialize` + `session/new` handshake. Default: 30.
    #[serde(default = "default_handshake_timeout_secs")]
    pub handshake_timeout_secs: u64,

    /// Timeout in seconds for the entire session (wall-clock since first prompt). Default: 1800.
    #[serde(default = "default_session_timeout_secs")]
    pub session_timeout_secs: u64,

    /// Timeout in seconds for a single prompt round-trip. Default: 600.
    #[serde(default = "default_prompt_timeout_secs")]
    pub prompt_timeout_secs: u64,

    /// When `true`, the permission handler automatically approves all permission requests.
    ///
    /// Should only be `true` in trusted contexts (tests, well-known agents).
    #[serde(default)]
    pub auto_approve_permissions: bool,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            process_cwd: None,
            session_cwd: None,
            env: BTreeMap::new(),
            inherit_env: Vec::new(),
            handshake_timeout_secs: default_handshake_timeout_secs(),
            session_timeout_secs: default_session_timeout_secs(),
            prompt_timeout_secs: default_prompt_timeout_secs(),
            auto_approve_permissions: false,
        }
    }
}

impl SubagentConfig {
    /// Resolve the effective process working directory.
    ///
    /// Returns `process_cwd` if set, otherwise falls back to the caller's current directory.
    #[must_use]
    pub fn effective_process_cwd(&self) -> PathBuf {
        self.process_cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// Resolve the effective ACP session working directory.
    ///
    /// Returns `session_cwd` if set, then `process_cwd`, then the caller's current directory.
    #[must_use]
    pub fn effective_session_cwd(&self) -> PathBuf {
        self.session_cwd
            .clone()
            .or_else(|| self.process_cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

fn default_handshake_timeout_secs() -> u64 {
    30
}

fn default_session_timeout_secs() -> u64 {
    1800
}

fn default_prompt_timeout_secs() -> u64 {
    600
}
