// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lifecycle hooks for sub-agents.
//!
//! Hooks are shell commands or MCP tool calls executed at specific points in a
//! sub-agent's or main agent's lifecycle. Per-agent frontmatter supports `PreToolUse`
//! and `PostToolUse` hooks via the `hooks` section. Config-level events include
//! `CwdChanged`, `FileChanged`, and `PermissionDenied`.
//!
//! # Hook actions
//!
//! - `type = "command"` — runs a shell command via `sh -c`.
//! - `type = "mcp_tool"` — dispatches to an MCP server tool via [`McpDispatch`].
//!
//! # Security
//!
//! All shell hook commands are run via `sh -c` with a **cleared** environment. Only `PATH`
//! from the parent process is preserved, and the hook-specific `ZEPH_*` variables are
//! added explicitly. This prevents accidental secret leakage from the parent environment.
//!
//! # Execution order
//!
//! Hooks within a matcher are run sequentially. `fail_closed = true` hooks abort on the
//! first error; `fail_closed = false` (default) log the error and continue.
//!
//! # Examples
//!
//! ```rust,no_run
//! use std::collections::HashMap;
//! use zeph_subagent::{HookDef, HookAction, fire_hooks};
//!
//! async fn run() {
//!     let hooks = vec![HookDef {
//!         action: HookAction::Command { command: "true".to_owned() },
//!         timeout_secs: 5,
//!         fail_closed: false,
//!     }];
//!     fire_hooks(&hooks, &HashMap::new(), None).await.unwrap();
//! }
//! ```

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

pub use zeph_config::{HookAction, HookDef, HookMatcher, SubagentHooks};

// ── McpDispatch ───────────────────────────────────────────────────────────────

/// Abstraction over MCP tool dispatch used by hooks.
///
/// This trait decouples `zeph-subagent` from `zeph-mcp`, allowing the hook
/// executor to call MCP tools without a direct crate dependency. Implementors
/// are provided by `zeph-core` at the call site.
///
/// # Errors
///
/// Returns an error string if the tool call fails for any reason (server not
/// found, policy violation, timeout, etc.).
pub trait McpDispatch: Send + Sync {
    /// Call a tool on the named MCP server with the given JSON arguments.
    fn call_tool<'a>(
        &'a self,
        server: &'a str,
        tool: &'a str,
        args: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    >;
}

// ── Error ─────────────────────────────────────────────────────────────────────

/// Errors that can occur when executing a lifecycle hook.
#[derive(Debug, Error)]
pub enum HookError {
    /// The shell command exited with a non-zero status code.
    #[error("hook command failed (exit code {code}): {command}")]
    NonZeroExit { command: String, code: i32 },

    /// The shell command did not complete within its configured `timeout_secs`.
    #[error("hook command timed out after {timeout_secs}s: {command}")]
    Timeout { command: String, timeout_secs: u64 },

    /// The shell could not be spawned or an I/O error occurred while waiting.
    #[error("hook I/O error for command '{command}': {source}")]
    Io {
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// An `mcp_tool` hook was configured but no MCP manager is available.
    #[error(
        "mcp_tool hook requires an MCP manager but none was provided (server={server}, tool={tool})"
    )]
    McpUnavailable { server: String, tool: String },

    /// The MCP tool call returned an error.
    #[error("mcp_tool hook failed (server={server}, tool={tool}): {reason}")]
    McpToolFailed {
        server: String,
        tool: String,
        reason: String,
    },
}

// ── Matching ──────────────────────────────────────────────────────────────────

/// Return all hook definitions from `matchers` whose patterns match `tool_name`.
///
/// Matching rules:
/// - Each [`HookMatcher`]`.matcher` is a `|`-separated list of tokens.
/// - A token matches if `tool_name` **contains** the token (case-sensitive substring).
/// - Empty tokens are ignored.
///
/// # Examples
///
/// ```rust
/// use zeph_subagent::{HookDef, HookAction, HookMatcher, matching_hooks};
///
/// let hook = HookDef { action: HookAction::Command { command: "echo hi".to_owned() }, timeout_secs: 30, fail_closed: false };
/// let matchers = vec![HookMatcher { matcher: "Edit|Write".to_owned(), hooks: vec![hook] }];
///
/// assert_eq!(matching_hooks(&matchers, "Edit").len(), 1);
/// assert!(matching_hooks(&matchers, "Shell").is_empty());
/// ```
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
/// The `mcp` parameter provides MCP tool dispatch for `type = "mcp_tool"` hooks.
/// Pass `None` when no MCP manager is available; `mcp_tool` hooks will fail with
/// [`HookError::McpUnavailable`] (respecting `fail_closed`).
///
/// # Errors
///
/// Returns [`HookError`] if a fail-closed hook exits non-zero, times out, or the
/// MCP call fails.
pub async fn fire_hooks<S: BuildHasher>(
    hooks: &[HookDef],
    env: &HashMap<String, String, S>,
    mcp: Option<&dyn McpDispatch>,
) -> Result<(), HookError> {
    for hook in hooks {
        let result = fire_single_hook(hook, env, mcp).await;
        match result {
            Ok(()) => {}
            Err(e) if hook.fail_closed => {
                tracing::error!(
                    error = %e,
                    "fail-closed hook failed — aborting"
                );
                return Err(e);
            }
            Err(e) => {
                tracing::warn!(
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
    mcp: Option<&dyn McpDispatch>,
) -> Result<(), HookError> {
    match &hook.action {
        HookAction::Command { command } => fire_shell_hook(command, hook.timeout_secs, env).await,
        HookAction::McpTool { server, tool, args } => {
            let dispatcher = mcp.ok_or_else(|| HookError::McpUnavailable {
                server: server.clone(),
                tool: tool.clone(),
            })?;
            let call_fut = dispatcher.call_tool(server, tool, args.clone());
            match timeout(Duration::from_secs(hook.timeout_secs), call_fut).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(reason)) => Err(HookError::McpToolFailed {
                    server: server.clone(),
                    tool: tool.clone(),
                    reason,
                }),
                Err(_) => Err(HookError::Timeout {
                    command: format!("mcp_tool:{server}/{tool}"),
                    timeout_secs: hook.timeout_secs,
                }),
            }
        }
    }
}

async fn fire_shell_hook<S: BuildHasher>(
    command: &str,
    timeout_secs: u64,
    env: &HashMap<String, String, S>,
) -> Result<(), HookError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
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
        command: command.to_owned(),
        source: e,
    })?;

    let result = timeout(Duration::from_secs(timeout_secs), child.wait()).await;

    match result {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(HookError::NonZeroExit {
            command: command.to_owned(),
            code: status.code().unwrap_or(-1),
        }),
        Ok(Err(e)) => Err(HookError::Io {
            command: command.to_owned(),
            source: e,
        }),
        Err(_) => {
            // SEC-H-004: explicitly kill child on timeout to prevent orphan processes.
            let _ = child.kill().await;
            Err(HookError::Timeout {
                command: command.to_owned(),
                timeout_secs,
            })
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd_hook(command: &str, fail_closed: bool, timeout_secs: u64) -> HookDef {
        HookDef {
            action: HookAction::Command {
                command: command.to_owned(),
            },
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
        let hook = cmd_hook("echo hi", false, 30);
        let matchers = vec![make_matcher("Edit", vec![hook.clone()])];
        let result = matching_hooks(&matchers, "Edit");
        assert_eq!(result.len(), 1);
        assert!(
            matches!(&result[0].action, HookAction::Command { command } if command == "echo hi")
        );
    }

    #[test]
    fn matching_hooks_substring() {
        let hook = cmd_hook("echo sub", false, 30);
        let matchers = vec![make_matcher("Edit", vec![hook.clone()])];
        let result = matching_hooks(&matchers, "EditFile");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn matching_hooks_pipe_separated() {
        let h1 = cmd_hook("echo e", false, 30);
        let h2 = cmd_hook("echo w", false, 30);
        let matchers = vec![
            make_matcher("Edit|Write", vec![h1.clone()]),
            make_matcher("Shell", vec![h2.clone()]),
        ];
        let result_edit = matching_hooks(&matchers, "Edit");
        assert_eq!(result_edit.len(), 1);

        let result_shell = matching_hooks(&matchers, "Shell");
        assert_eq!(result_shell.len(), 1);

        let result_none = matching_hooks(&matchers, "Read");
        assert!(result_none.is_empty());
    }

    #[test]
    fn matching_hooks_no_match() {
        let hook = cmd_hook("echo nope", false, 30);
        let matchers = vec![make_matcher("Edit", vec![hook])];
        let result = matching_hooks(&matchers, "Shell");
        assert!(result.is_empty());
    }

    #[test]
    fn matching_hooks_empty_token_ignored() {
        let hook = cmd_hook("echo empty", false, 30);
        let matchers = vec![make_matcher("|Edit|", vec![hook])];
        let result = matching_hooks(&matchers, "Edit");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn matching_hooks_multiple_matchers_both_match() {
        let h1 = cmd_hook("echo 1", false, 30);
        let h2 = cmd_hook("echo 2", false, 30);
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
        let hooks = vec![cmd_hook("true", false, 5)];
        let env = HashMap::new();
        assert!(fire_hooks(&hooks, &env, None).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_fail_open_continues() {
        let hooks = vec![
            cmd_hook("false", false, 5), // fail open
            cmd_hook("true", false, 5),  // should still run
        ];
        let env = HashMap::new();
        assert!(fire_hooks(&hooks, &env, None).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_fail_closed_returns_err() {
        let hooks = vec![cmd_hook("false", true, 5)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HookError::NonZeroExit { .. }));
    }

    #[tokio::test]
    async fn fire_hooks_timeout() {
        let hooks = vec![cmd_hook("sleep 10", true, 1)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HookError::Timeout { .. }));
    }

    #[tokio::test]
    async fn fire_hooks_env_passed() {
        let hooks = vec![cmd_hook(r#"test "$ZEPH_TEST_VAR" = "hello""#, true, 5)];
        let mut env = HashMap::new();
        env.insert("ZEPH_TEST_VAR".to_owned(), "hello".to_owned());
        assert!(fire_hooks(&hooks, &env, None).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_empty_list_ok() {
        let env = HashMap::new();
        assert!(fire_hooks(&[], &env, None).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_mcp_unavailable_fail_open() {
        let hooks = vec![HookDef {
            action: HookAction::McpTool {
                server: "srv".into(),
                tool: "t".into(),
                args: serde_json::Value::Null,
            },
            timeout_secs: 5,
            fail_closed: false,
        }];
        let env = HashMap::new();
        // fail_open: should succeed even though MCP is unavailable
        assert!(fire_hooks(&hooks, &env, None).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_mcp_unavailable_fail_closed() {
        let hooks = vec![HookDef {
            action: HookAction::McpTool {
                server: "srv".into(),
                tool: "t".into(),
                args: serde_json::Value::Null,
            },
            timeout_secs: 5,
            fail_closed: true,
        }];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None).await;
        assert!(matches!(result, Err(HookError::McpUnavailable { .. })));
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
        assert!(
            matches!(&hooks.pre_tool_use[0].hooks[0].action, HookAction::Command { command } if command == "echo pre")
        );
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
