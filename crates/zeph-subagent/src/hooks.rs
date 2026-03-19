// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lifecycle hooks for sub-agents.
//!
//! Hooks are shell commands executed at specific points in a sub-agent's lifecycle.
//! Per-agent frontmatter supports `PreToolUse` and `PostToolUse` hooks via the
//! `hooks` section. `SubagentStart` and `SubagentStop` are config-level events.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

pub use zeph_config::{HookDef, HookMatcher, HookType, SubagentHooks};

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum HookError {
    #[error("hook command failed (exit code {code}): {command}")]
    NonZeroExit { command: String, code: i32 },

    #[error("hook command timed out after {timeout_secs}s: {command}")]
    Timeout { command: String, timeout_secs: u64 },

    #[error("hook I/O error for command '{command}': {source}")]
    Io {
        command: String,
        #[source]
        source: std::io::Error,
    },
}

// ── Matching ──────────────────────────────────────────────────────────────────

/// Return all hook definitions whose matchers match `tool_name`.
///
/// Matching rules:
/// - Each `HookMatcher.matcher` is a `|`-separated list of tokens.
/// - A token matches if `tool_name` contains the token (case-sensitive substring).
/// - Empty tokens are ignored.
#[must_use]
pub fn matching_hooks<'a>(matchers: &'a [HookMatcher], tool_name: &str) -> Vec<&'a HookDef> {
    let mut result = Vec::new();
    for m in matchers {
        let matched = m
            .matcher
            .split('|')
            .filter(|token| !token.is_empty())
            .any(|token| tool_name.contains(token));
        if matched {
            result.extend(m.hooks.iter());
        }
    }
    result
}

// ── Execution ─────────────────────────────────────────────────────────────────

/// Execute a list of hook definitions, setting the provided environment variables.
///
/// Hooks are run sequentially. If a hook has `fail_closed = true` and fails,
/// execution stops immediately and `Err` is returned. Otherwise errors are logged
/// and execution continues.
///
/// # Errors
///
/// Returns [`HookError`] if a fail-closed hook exits non-zero or times out.
pub async fn fire_hooks<S: BuildHasher>(
    hooks: &[HookDef],
    env: &HashMap<String, String, S>,
) -> Result<(), HookError> {
    for hook in hooks {
        let result = fire_single_hook(hook, env).await;
        match result {
            Ok(()) => {}
            Err(e) if hook.fail_closed => {
                tracing::error!(
                    command = %hook.command,
                    error = %e,
                    "fail-closed hook failed — aborting"
                );
                return Err(e);
            }
            Err(e) => {
                tracing::warn!(
                    command = %hook.command,
                    error = %e,
                    "hook failed (fail_open) — continuing"
                );
            }
        }
    }
    Ok(())
}

async fn fire_single_hook<S: BuildHasher>(
    hook: &HookDef,
    env: &HashMap<String, String, S>,
) -> Result<(), HookError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&hook.command);
    // SEC-H-002: clear inherited env to prevent secret leakage, then set only hook vars.
    cmd.env_clear();
    // Preserve minimal PATH so the shell can find standard tools.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Suppress stdout/stderr to prevent hook output flooding the agent.
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    let mut child = cmd.spawn().map_err(|e| HookError::Io {
        command: hook.command.clone(),
        source: e,
    })?;

    let result = timeout(Duration::from_secs(hook.timeout_secs), child.wait()).await;

    match result {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(HookError::NonZeroExit {
            command: hook.command.clone(),
            code: status.code().unwrap_or(-1),
        }),
        Ok(Err(e)) => Err(HookError::Io {
            command: hook.command.clone(),
            source: e,
        }),
        Err(_) => {
            // SEC-H-004: explicitly kill child on timeout to prevent orphan processes.
            let _ = child.kill().await;
            Err(HookError::Timeout {
                command: hook.command.clone(),
                timeout_secs: hook.timeout_secs,
            })
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook(command: &str, fail_closed: bool, timeout_secs: u64) -> HookDef {
        HookDef {
            hook_type: HookType::Command,
            command: command.to_owned(),
            timeout_secs,
            fail_closed,
        }
    }

    fn make_matcher(matcher: &str, hooks: Vec<HookDef>) -> HookMatcher {
        HookMatcher {
            matcher: matcher.to_owned(),
            hooks,
        }
    }

    // ── matching_hooks ────────────────────────────────────────────────────────

    #[test]
    fn matching_hooks_exact_name() {
        let hook = make_hook("echo hi", false, 30);
        let matchers = vec![make_matcher("Edit", vec![hook.clone()])];
        let result = matching_hooks(&matchers, "Edit");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].command, "echo hi");
    }

    #[test]
    fn matching_hooks_substring() {
        let hook = make_hook("echo sub", false, 30);
        let matchers = vec![make_matcher("Edit", vec![hook.clone()])];
        let result = matching_hooks(&matchers, "EditFile");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn matching_hooks_pipe_separated() {
        let h1 = make_hook("echo e", false, 30);
        let h2 = make_hook("echo w", false, 30);
        let matchers = vec![
            make_matcher("Edit|Write", vec![h1.clone()]),
            make_matcher("Shell", vec![h2.clone()]),
        ];
        let result_edit = matching_hooks(&matchers, "Edit");
        assert_eq!(result_edit.len(), 1);
        assert_eq!(result_edit[0].command, "echo e");

        let result_shell = matching_hooks(&matchers, "Shell");
        assert_eq!(result_shell.len(), 1);
        assert_eq!(result_shell[0].command, "echo w");

        let result_none = matching_hooks(&matchers, "Read");
        assert!(result_none.is_empty());
    }

    #[test]
    fn matching_hooks_no_match() {
        let hook = make_hook("echo nope", false, 30);
        let matchers = vec![make_matcher("Edit", vec![hook])];
        let result = matching_hooks(&matchers, "Shell");
        assert!(result.is_empty());
    }

    #[test]
    fn matching_hooks_empty_token_ignored() {
        let hook = make_hook("echo empty", false, 30);
        let matchers = vec![make_matcher("|Edit|", vec![hook])];
        let result = matching_hooks(&matchers, "Edit");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn matching_hooks_multiple_matchers_both_match() {
        let h1 = make_hook("echo 1", false, 30);
        let h2 = make_hook("echo 2", false, 30);
        let matchers = vec![
            make_matcher("Shell", vec![h1]),
            make_matcher("Shell", vec![h2]),
        ];
        let result = matching_hooks(&matchers, "Shell");
        assert_eq!(result.len(), 2);
    }

    // ── fire_hooks ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fire_hooks_success() {
        let hooks = vec![make_hook("true", false, 5)];
        let env = HashMap::new();
        assert!(fire_hooks(&hooks, &env).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_fail_open_continues() {
        let hooks = vec![
            make_hook("false", false, 5), // fail open
            make_hook("true", false, 5),  // should still run
        ];
        let env = HashMap::new();
        assert!(fire_hooks(&hooks, &env).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_fail_closed_returns_err() {
        let hooks = vec![make_hook("false", true, 5)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HookError::NonZeroExit { .. }));
    }

    #[tokio::test]
    async fn fire_hooks_timeout() {
        let hooks = vec![make_hook("sleep 10", true, 1)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HookError::Timeout { .. }));
    }

    #[tokio::test]
    async fn fire_hooks_env_passed() {
        let hooks = vec![make_hook(r#"test "$ZEPH_TEST_VAR" = "hello""#, true, 5)];
        let mut env = HashMap::new();
        env.insert("ZEPH_TEST_VAR".to_owned(), "hello".to_owned());
        assert!(fire_hooks(&hooks, &env).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_empty_list_ok() {
        let env = HashMap::new();
        assert!(fire_hooks(&[], &env).await.is_ok());
    }

    // ── YAML parsing ──────────────────────────────────────────────────────────

    #[test]
    fn subagent_hooks_parses_from_yaml() {
        let yaml = r#"
PreToolUse:
  - matcher: "Edit|Write"
    hooks:
      - type: command
        command: "echo pre"
        timeout_secs: 10
        fail_closed: false
PostToolUse:
  - matcher: "Shell"
    hooks:
      - type: command
        command: "echo post"
"#;
        let hooks: SubagentHooks = serde_norway::from_str(yaml).unwrap();
        assert_eq!(hooks.pre_tool_use.len(), 1);
        assert_eq!(hooks.pre_tool_use[0].matcher, "Edit|Write");
        assert_eq!(hooks.pre_tool_use[0].hooks.len(), 1);
        assert_eq!(hooks.pre_tool_use[0].hooks[0].command, "echo pre");
        assert_eq!(hooks.post_tool_use.len(), 1);
    }

    #[test]
    fn subagent_hooks_defaults_timeout() {
        let yaml = r#"
PreToolUse:
  - matcher: "Edit"
    hooks:
      - type: command
        command: "echo hi"
"#;
        let hooks: SubagentHooks = serde_norway::from_str(yaml).unwrap();
        assert_eq!(hooks.pre_tool_use[0].hooks[0].timeout_secs, 30);
        assert!(!hooks.pre_tool_use[0].hooks[0].fail_closed);
    }

    #[test]
    fn subagent_hooks_empty_default() {
        let hooks = SubagentHooks::default();
        assert!(hooks.pre_tool_use.is_empty());
        assert!(hooks.post_tool_use.is_empty());
    }
}
