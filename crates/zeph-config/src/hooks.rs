// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::subagent::HookDef;

fn default_debounce_ms() -> u64 {
    500
}

/// Configuration for hooks triggered when watched files change.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct FileChangedConfig {
    /// Paths to watch for changes. Resolved relative to the project root (cwd at startup).
    pub watch_paths: Vec<PathBuf>,
    /// Debounce interval in milliseconds. Default: 500.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    /// Hooks fired when a watched file changes.
    #[serde(default)]
    pub hooks: Vec<HookDef>,
}

impl Default for FileChangedConfig {
    fn default() -> Self {
        Self {
            watch_paths: Vec::new(),
            debounce_ms: default_debounce_ms(),
            hooks: Vec::new(),
        }
    }
}

/// Top-level hooks configuration section.
///
/// Each sub-section corresponds to a lifecycle event. All sections default to
/// empty (no hooks). Events fire in the order hooks are listed.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HooksConfig {
    /// Hooks fired when the agent's working directory changes via `set_working_directory`.
    pub cwd_changed: Vec<HookDef>,
    /// File-change watcher configuration with associated hooks.
    pub file_changed: Option<FileChangedConfig>,
    /// Hooks fired when a tool execution is blocked by a `RuntimeLayer::before_tool` check.
    ///
    /// Environment variables set for `Command` hooks:
    /// - `ZEPH_DENIED_TOOL` — the name of the tool that was blocked.
    /// - `ZEPH_DENY_REASON` — human-readable reason string from the layer.
    pub permission_denied: Vec<HookDef>,
}

impl HooksConfig {
    /// Returns `true` when no hooks are configured (all sections are empty or absent).
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::hooks::HooksConfig;
    ///
    /// assert!(HooksConfig::default().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cwd_changed.is_empty()
            && self.file_changed.is_none()
            && self.permission_denied.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::HookAction;

    fn cmd_hook(command: &str) -> HookDef {
        HookDef {
            action: HookAction::Command {
                command: command.into(),
            },
            timeout_secs: 10,
            fail_closed: false,
        }
    }

    #[test]
    fn hooks_config_default_is_empty() {
        let cfg = HooksConfig::default();
        assert!(cfg.is_empty());
    }

    #[test]
    fn file_changed_config_default_debounce() {
        let cfg = FileChangedConfig::default();
        assert_eq!(cfg.debounce_ms, 500);
        assert!(cfg.watch_paths.is_empty());
        assert!(cfg.hooks.is_empty());
    }

    #[test]
    fn hooks_config_parses_from_toml() {
        let toml = r#"
[[cwd_changed]]
type = "command"
command = "echo changed"
timeout_secs = 10
fail_closed = false

[file_changed]
watch_paths = ["src/", "Cargo.toml"]
debounce_ms = 300
[[file_changed.hooks]]
type = "command"
command = "cargo check"
timeout_secs = 30
fail_closed = false

[[permission_denied]]
type = "command"
command = "echo denied"
timeout_secs = 5
fail_closed = false
"#;
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.cwd_changed.len(), 1);
        assert!(
            matches!(&cfg.cwd_changed[0].action, HookAction::Command { command } if command == "echo changed")
        );
        let fc = cfg.file_changed.as_ref().unwrap();
        assert_eq!(fc.watch_paths.len(), 2);
        assert_eq!(fc.debounce_ms, 300);
        assert_eq!(fc.hooks.len(), 1);
        assert_eq!(cfg.permission_denied.len(), 1);
        assert!(
            matches!(&cfg.permission_denied[0].action, HookAction::Command { command } if command == "echo denied")
        );
    }

    #[test]
    fn hooks_config_parses_mcp_tool_hook() {
        let toml = r#"
[[permission_denied]]
type = "mcp_tool"
server = "policy"
tool = "audit"
[permission_denied.args]
severity = "high"
"#;
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.permission_denied.len(), 1);
        assert!(matches!(
            &cfg.permission_denied[0].action,
            HookAction::McpTool { server, tool, .. } if server == "policy" && tool == "audit"
        ));
    }

    #[test]
    fn hooks_config_not_empty_with_cwd_hooks() {
        let cfg = HooksConfig {
            cwd_changed: vec![cmd_hook("echo hi")],
            file_changed: None,
            permission_denied: Vec::new(),
        };
        assert!(!cfg.is_empty());
    }

    #[test]
    fn hooks_config_not_empty_with_permission_denied_hooks() {
        let cfg = HooksConfig {
            cwd_changed: Vec::new(),
            file_changed: None,
            permission_denied: vec![cmd_hook("echo denied")],
        };
        assert!(!cfg.is_empty());
    }
}
