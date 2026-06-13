// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::subagent::{HookDef, HookMatcher};

fn default_debounce_ms() -> u64 {
    500
}

fn default_hook_block_cap() -> usize {
    8
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
///
/// Hooks are declared **inline in `config.toml`** under the `[hooks]` table.
/// No separate `settings.json` or external file is required. Both `command`
/// and `mcp_tool` action types are supported for every event.
///
/// # Examples
///
/// ```toml
/// [[hooks.pre_tool_use]]
/// matcher = "Edit|Write"
/// [[hooks.pre_tool_use.hooks]]
/// type = "command"
/// command = "echo pre $ZEPH_TOOL_NAME"
/// timeout_secs = 5
/// fail_closed = false
///
/// [[hooks.turn_complete]]
/// type = "mcp_tool"
/// server = "notifier"
/// tool = "notify"
/// [hooks.turn_complete.args]
/// channel = "desktop"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
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
    /// Hooks fired after each agent turn completes (#3327).
    ///
    /// Runs regardless of the `[notifications]` config. When a `[notifications]` notifier is
    /// also configured, these hooks share its `should_fire` gate (respecting `min_turn_duration_ms`,
    /// `only_on_error`, and `enabled`). When no notifier is configured, hooks fire on every
    /// completed turn.
    ///
    /// Use `min_duration_ms` in a wrapper script or the `[notifications].min_turn_duration_ms`
    /// gate to avoid firing on trivial responses.
    ///
    /// Environment variables set for `Command` hooks:
    /// - `ZEPH_TURN_DURATION_MS`   — wall-clock duration of the turn in milliseconds.
    /// - `ZEPH_TURN_STATUS`        — `"success"` or `"error"`.
    /// - `ZEPH_TURN_PREVIEW`       — redacted first ≤ 160 chars of the assistant response.
    /// - `ZEPH_TURN_LLM_REQUESTS`  — number of completed LLM round-trips this turn.
    #[serde(default)]
    pub turn_complete: Vec<HookDef>,
    /// Maximum number of `PreToolUse` hook blocks allowed per turn before the turn is ended
    /// with a warning message. Counts individual tool blocks — if a tier has N blocked tools,
    /// the counter increments by N. Default: 8. Use `0` for no cap (unlimited blocks).
    #[serde(default = "default_hook_block_cap")]
    pub hook_block_cap: usize,
    /// Hooks fired before each tool execution, matched by tool name pattern.
    ///
    /// Uses pipe-separated pattern matching (same as subagent hooks). Hooks fire
    /// before the `RuntimeLayer::before_tool` permission check — they observe every
    /// attempted tool call, including calls that will be subsequently blocked.
    ///
    /// Hook serialization within a tier: hooks for tools in the same dependency tier
    /// are dispatched sequentially (one tool's hooks complete before the next tool's
    /// hooks start). Hooks for tools in different tiers may overlap.
    ///
    /// Hooks are fail-open: errors are logged but do not block tool execution.
    ///
    /// Environment variables set for `Command` hooks:
    /// - `ZEPH_TOOL_NAME`      — name of the tool being invoked.
    /// - `ZEPH_TOOL_ARGS_JSON` — JSON-serialized tool arguments (truncated at 64 KiB).
    /// - `ZEPH_SESSION_ID`     — current conversation identifier, omitted when unavailable.
    #[serde(default)]
    pub pre_tool_use: Vec<HookMatcher>,
    /// Hooks fired after each tool execution completes, matched by tool name pattern.
    ///
    /// Fires after the tool result is available. Same pattern matching and
    /// fail-open semantics as `pre_tool_use`.
    ///
    /// Environment variables set for `Command` hooks:
    /// - `ZEPH_TOOL_NAME`        — name of the tool that was invoked.
    /// - `ZEPH_TOOL_ARGS_JSON`   — JSON-serialized tool arguments (truncated at 64 KiB).
    /// - `ZEPH_SESSION_ID`       — current conversation identifier, omitted when unavailable.
    /// - `ZEPH_TOOL_DURATION_MS` — wall-clock execution time in milliseconds.
    #[serde(default)]
    pub post_tool_use: Vec<HookMatcher>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            cwd_changed: Vec::new(),
            file_changed: None,
            permission_denied: Vec::new(),
            turn_complete: Vec::new(),
            hook_block_cap: default_hook_block_cap(),
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
        }
    }
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
            && self.turn_complete.is_empty()
            && self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
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
            r#if: None,
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
            turn_complete: Vec::new(),
            hook_block_cap: 8,
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
        };
        assert!(!cfg.is_empty());
    }

    #[test]
    fn hooks_config_not_empty_with_permission_denied_hooks() {
        let cfg = HooksConfig {
            cwd_changed: Vec::new(),
            file_changed: None,
            permission_denied: vec![cmd_hook("echo denied")],
            turn_complete: Vec::new(),
            hook_block_cap: 8,
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
        };
        assert!(!cfg.is_empty());
    }

    #[test]
    fn hooks_config_not_empty_with_turn_complete_hooks() {
        let cfg = HooksConfig {
            cwd_changed: Vec::new(),
            file_changed: None,
            permission_denied: Vec::new(),
            turn_complete: vec![cmd_hook("notify-send Zeph done")],
            hook_block_cap: 8,
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
        };
        assert!(!cfg.is_empty());
    }

    #[test]
    fn hooks_config_is_empty_when_all_empty_including_turn_complete() {
        let cfg = HooksConfig {
            cwd_changed: Vec::new(),
            file_changed: None,
            permission_denied: Vec::new(),
            turn_complete: Vec::new(),
            hook_block_cap: 8,
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
        };
        assert!(cfg.is_empty());
    }

    #[test]
    fn hooks_config_parses_turn_complete_from_toml() {
        let toml = r#"
[[turn_complete]]
type = "command"
command = "osascript -e 'display notification \"$ZEPH_TURN_PREVIEW\" with title \"Zeph\"'"
timeout_secs = 3
fail_closed = false
"#;
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.turn_complete.len(), 1);
        assert!(cfg.cwd_changed.is_empty());
        assert!(cfg.permission_denied.is_empty());
    }

    #[test]
    fn hooks_config_not_empty_with_pre_tool_use() {
        use crate::subagent::HookMatcher;
        let cfg = HooksConfig {
            cwd_changed: Vec::new(),
            file_changed: None,
            permission_denied: Vec::new(),
            turn_complete: Vec::new(),
            hook_block_cap: 8,
            pre_tool_use: vec![HookMatcher {
                matcher: "Edit|Write".to_owned(),
                hooks: vec![cmd_hook("echo pre")],
            }],
            post_tool_use: Vec::new(),
        };
        assert!(!cfg.is_empty());
    }

    #[test]
    fn hooks_config_parses_pre_and_post_tool_use_from_toml() {
        let toml = r#"
[[pre_tool_use]]
matcher = "Edit|Write"
[[pre_tool_use.hooks]]
type = "command"
command = "echo pre $ZEPH_TOOL_NAME"
timeout_secs = 5
fail_closed = false

[[post_tool_use]]
matcher = "Shell"
[[post_tool_use.hooks]]
type = "command"
command = "echo post $ZEPH_TOOL_DURATION_MS"
timeout_secs = 5
fail_closed = false
"#;
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.pre_tool_use.len(), 1);
        assert_eq!(cfg.pre_tool_use[0].matcher, "Edit|Write");
        assert_eq!(cfg.pre_tool_use[0].hooks.len(), 1);
        assert_eq!(cfg.post_tool_use.len(), 1);
        assert_eq!(cfg.post_tool_use[0].matcher, "Shell");
        assert!(!cfg.is_empty());
    }

    /// Exercises the full testing.toml hooks pattern: `cwd_changed` + `file_changed` + `permission_denied`
    /// all in one TOML document, in the order they appear in testing.toml. Prevents regression of
    /// issue #3625 where hooks appeared empty despite correct TOML config.
    #[test]
    fn hooks_config_parses_all_sections_in_sequence() {
        let toml = r#"
[[cwd_changed]]
type = "command"
command = "echo 'CWD_CHANGED_HOOK_FIRED'"
timeout_secs = 10
fail_closed = false

[file_changed]
watch_paths = ["src/", "Cargo.toml"]
debounce_ms = 500
[[file_changed.hooks]]
type = "command"
command = "cargo check"
timeout_secs = 30
fail_closed = false

[[permission_denied]]
type = "command"
command = "echo 'PERMISSION_DENIED_HOOK_FIRED'"
timeout_secs = 5
fail_closed = false
"#;
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.cwd_changed.len(), 1, "expected 1 cwd_changed hook");
        assert!(
            matches!(&cfg.cwd_changed[0].action, HookAction::Command { command } if command == "echo 'CWD_CHANGED_HOOK_FIRED'")
        );
        let fc = cfg
            .file_changed
            .as_ref()
            .expect("file_changed must be Some");
        assert_eq!(fc.hooks.len(), 1, "expected 1 file_changed hook");
        assert_eq!(fc.debounce_ms, 500);
        assert_eq!(
            cfg.permission_denied.len(),
            1,
            "expected 1 permission_denied hook"
        );
        assert!(!cfg.is_empty(), "hooks config must not be empty");
    }

    #[test]
    fn hook_block_cap_default_is_8() {
        let cfg = HooksConfig::default();
        assert_eq!(cfg.hook_block_cap, 8);
    }

    #[test]
    fn hook_block_cap_parses_from_toml() {
        let toml = "hook_block_cap = 4\n";
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.hook_block_cap, 4);
    }

    #[test]
    fn hook_block_cap_zero_from_toml() {
        let toml = "hook_block_cap = 0\n";
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.hook_block_cap, 0);
    }

    #[test]
    fn hooks_config_parses_mcp_tool_in_pre_tool_use() {
        let toml = r#"
[[pre_tool_use]]
matcher = "Shell"
[[pre_tool_use.hooks]]
type = "mcp_tool"
server = "policy"
tool = "audit"
[pre_tool_use.hooks.args]
severity = "high"
"#;
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.pre_tool_use.len(), 1);
        assert_eq!(cfg.pre_tool_use[0].matcher, "Shell");
        assert_eq!(cfg.pre_tool_use[0].hooks.len(), 1);
        assert!(matches!(
            &cfg.pre_tool_use[0].hooks[0].action,
            HookAction::McpTool { server, tool, .. } if server == "policy" && tool == "audit"
        ));
        assert!(!cfg.is_empty());
    }

    // ── HookDef `if` field serde ──────────────────────────────────────────────

    #[test]
    fn hook_def_if_none_omits_field_in_toml() {
        let hook = cmd_hook("echo hi");
        // r#if: None must not serialize the `if` key.
        let serialized = toml::to_string(&hook).unwrap();
        assert!(
            !serialized.contains("if"),
            "unexpected `if` key: {serialized}"
        );
    }

    #[test]
    fn hook_def_if_some_roundtrips_via_toml() {
        use crate::subagent::HookAction;
        use crate::subagent::HookDef;
        let hook = HookDef {
            action: HookAction::Command {
                command: "echo hi".into(),
            },
            timeout_secs: 10,
            fail_closed: false,
            r#if: Some("tool:shell".to_owned()),
        };
        let serialized = toml::to_string(&hook).unwrap();
        assert!(
            serialized.contains("if = \"tool:shell\""),
            "missing `if` key: {serialized}"
        );
        let deserialized: HookDef = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.r#if.as_deref(), Some("tool:shell"));
    }

    #[test]
    fn hook_def_if_condition_parses_from_toml() {
        let toml = r#"
[[post_tool_use]]
matcher = "Shell"
[[post_tool_use.hooks]]
type = "command"
command = "echo shell"
timeout_secs = 5
fail_closed = false
if = "tool:shell"
"#;
        let cfg: HooksConfig = toml::from_str(toml).unwrap();
        let hook = &cfg.post_tool_use[0].hooks[0];
        assert_eq!(hook.r#if.as_deref(), Some("tool:shell"));
    }
}
