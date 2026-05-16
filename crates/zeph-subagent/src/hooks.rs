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
//! # `PostToolUse` stdout replacement
//!
//! Shell hooks for `PostToolUse` events may emit a JSON object to stdout to replace the
//! tool output seen by the agent. The JSON must contain:
//!
//! ```json
//! { "hookSpecificOutput": { "updatedToolOutput": "replacement text" } }
//! ```
//!
//! If `updatedToolOutput` is `null` or absent, or stdout is empty / not valid JSON, the
//! original tool output is preserved (backward compatible). Hook stdout is capped at 1 MiB
//! to prevent memory exhaustion; output exceeding the cap is silently truncated and treated
//! as if no substitution was requested.
//!
//! # Hook stdin (`PostToolUse`)
//!
//! For `PostToolUse` and `PostToolUseFailure` events, a JSON context object is written to
//! the hook process's stdin:
//!
//! ```json
//! {
//!   "tool_name": "Shell",
//!   "tool_args": { ... },
//!   "session_id": "abc123",
//!   "duration_ms": 142,
//!   "tool_output": "command output here"
//! }
//! ```
//!
//! `tool_error` replaces `tool_output` for failure events. Hooks that do not read stdin
//! are unaffected — the pipe is closed when the child ignores it.
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
//!     fire_hooks(&hooks, &HashMap::new(), None, None).await.unwrap();
//! }
//! ```

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::time::timeout;

pub use zeph_config::{HookAction, HookDef, HookMatcher, SubagentHooks};

// ── Hook output types ─────────────────────────────────────────────────────────

/// Structured output captured from a hook's stdout.
///
/// Only populated for shell `PostToolUse` hooks; MCP hooks always produce
/// `updated_tool_output: None`.
#[derive(Debug, Default)]
pub struct HookOutput {
    /// Replacement text for the tool output, when the hook requests a substitution.
    ///
    /// `None` means the original tool output is preserved.
    pub updated_tool_output: Option<String>,
}

/// Aggregate result of executing one or more hooks in sequence.
///
/// Callers that do not need the output can ignore this and check only for `Err`.
#[derive(Debug, Default)]
pub struct HookRunResult {
    /// Merged output from the hook sequence. When multiple hooks emit
    /// `updatedToolOutput`, the last non-`None` value wins.
    pub output: HookOutput,
}

// ── Hook stdin payload ────────────────────────────────────────────────────────

/// Context serialized to hook stdin for `PostToolUse` and `PostToolUseFailure` events.
///
/// The `tool_output` field is present for success events; `tool_error` is present
/// for failure events. Both are `Option` with `skip_serializing_if` so only the
/// relevant field appears in the JSON written to stdin.
#[derive(Debug, Serialize)]
pub struct PostToolUseHookInput<'a> {
    /// Name of the tool that was invoked.
    pub tool_name: &'a str,
    /// Arguments passed to the tool (the parsed JSON value).
    pub tool_args: &'a serde_json::Value,
    /// Conversation / session identifier, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<&'a str>,
    /// Wall-clock time the tool took to execute, in milliseconds.
    pub duration_ms: u64,
    /// Tool output text (success path). Absent for failure events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<&'a str>,
    /// Tool error text (failure path). Absent for success events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_error: Option<&'a str>,
}

/// Maximum number of bytes read from hook stdout before truncation.
const HOOK_STDOUT_CAP: usize = 1024 * 1024; // 1 MiB

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
/// The optional `stdin_json` bytes are written to the hook process's stdin before
/// it runs (shell hooks only). Pass `None` for hooks that do not require context
/// on stdin (e.g., `PreToolUse`). MCP hooks never receive stdin data.
///
/// When multiple shell hooks run in sequence and emit `updatedToolOutput`, the last
/// non-`None` value wins. If a fail-closed hook aborts after a previous hook already
/// produced a replacement, the prior replacement is preserved in the returned error
/// path — callers should discard `HookRunResult` on `Err` if appropriate for their
/// use-case, but the struct always reflects what was captured before the abort.
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
    stdin_json: Option<&[u8]>,
) -> Result<HookRunResult, HookError> {
    let mut run_result = HookRunResult::default();
    for hook in hooks {
        // For chaining: pass the already-replaced output as the new stdin so each
        // subsequent hook sees the current (potentially substituted) output.
        let effective_stdin = run_result
            .output
            .updated_tool_output
            .as_deref()
            .map(str::as_bytes)
            .or(stdin_json);
        let result = fire_single_hook(hook, env, mcp, effective_stdin).await;
        match result {
            Ok(hook_output) => {
                if hook_output.updated_tool_output.is_some() {
                    run_result.output.updated_tool_output = hook_output.updated_tool_output;
                }
            }
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
    Ok(run_result)
}

async fn fire_single_hook<S: BuildHasher>(
    hook: &HookDef,
    env: &HashMap<String, String, S>,
    mcp: Option<&dyn McpDispatch>,
    stdin_json: Option<&[u8]>,
) -> Result<HookOutput, HookError> {
    match &hook.action {
        HookAction::Command { command } => {
            fire_shell_hook(command, hook.timeout_secs, env, stdin_json).await
        }
        HookAction::McpTool { server, tool, args } => {
            let dispatcher = mcp.ok_or_else(|| HookError::McpUnavailable {
                server: server.clone(),
                tool: tool.clone(),
            })?;
            let call_fut = dispatcher.call_tool(server, tool, args.clone());
            match timeout(Duration::from_secs(hook.timeout_secs), call_fut).await {
                Ok(Ok(_)) => {
                    // MCP hooks produce no stdout — output substitution is not supported.
                    Ok(HookOutput::default())
                }
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
    stdin_json: Option<&[u8]>,
) -> Result<HookOutput, HookError> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt as _;

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
    cmd.stdin(if stdin_json.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    // Capture stdout to parse potential updatedToolOutput JSON.
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());

    let mut child = cmd.spawn().map_err(|e| HookError::Io {
        command: command.to_owned(),
        source: e,
    })?;

    // Write stdin before awaiting child exit to avoid deadlock on full pipes.
    // Drop the handle to close the pipe so the child gets EOF when it stops reading.
    if let Some(bytes) = stdin_json
        && let Some(mut stdin_handle) = child.stdin.take()
        && let Err(e) = stdin_handle.write_all(bytes).await
    {
        tracing::warn!(
            command,
            error = %e,
            "failed to write stdin to hook — continuing without stdin data"
        );
    }

    // Wait for process exit with a timeout, then read stdout. Sequential order avoids the
    // deadlock where read_fut blocks on EOF while kill() is gated behind join! completion.
    let stdout_handle = child.stdout.take();
    match timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(Ok(status)) => {
            let mut stdout_bytes = Vec::new();
            if let Some(handle) = stdout_handle {
                let mut limited = handle.take(HOOK_STDOUT_CAP as u64 + 1);
                let _ = limited.read_to_end(&mut stdout_bytes).await;
            }
            if status.success() {
                Ok(parse_hook_stdout(command, &stdout_bytes))
            } else {
                Err(HookError::NonZeroExit {
                    command: command.to_owned(),
                    code: status.code().unwrap_or(-1),
                })
            }
        }
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

/// Parse hook stdout bytes into a [`HookOutput`].
///
/// Returns a default (no substitution) value on any parse error to preserve
/// backward compatibility with hooks that write non-JSON to stdout.
fn parse_hook_stdout(command: &str, bytes: &[u8]) -> HookOutput {
    if bytes.is_empty() {
        return HookOutput::default();
    }
    if bytes.len() > HOOK_STDOUT_CAP {
        tracing::warn!(
            command,
            bytes = bytes.len(),
            cap = HOOK_STDOUT_CAP,
            "hook stdout exceeds 1 MiB cap — treating as no substitution"
        );
        return HookOutput::default();
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        tracing::warn!(command, "hook stdout is not valid UTF-8 — no substitution");
        return HookOutput::default();
    };
    // Silent on JSON parse failure: backward compat — hooks may write human-readable output.
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return HookOutput::default();
    };
    let updated = json
        .get("hookSpecificOutput")
        .and_then(|h| h.get("updatedToolOutput"));

    match updated {
        None | Some(serde_json::Value::Null) => HookOutput::default(),
        Some(serde_json::Value::String(s)) => HookOutput {
            updated_tool_output: Some(s.clone()),
        },
        Some(other) => {
            tracing::warn!(
                command,
                kind = other
                    .is_object()
                    .then_some("object")
                    .or_else(|| other.is_array().then_some("array"))
                    .or_else(|| other.is_number().then_some("number"))
                    .or_else(|| other.is_boolean().then_some("boolean"))
                    .unwrap_or("unknown"),
                "hookSpecificOutput.updatedToolOutput has unexpected type — no substitution"
            );
            HookOutput::default()
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
        assert!(fire_hooks(&hooks, &env, None, None).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_fail_open_continues() {
        let hooks = vec![
            cmd_hook("false", false, 5), // fail open
            cmd_hook("true", false, 5),  // should still run
        ];
        let env = HashMap::new();
        assert!(fire_hooks(&hooks, &env, None, None).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_fail_closed_returns_err() {
        let hooks = vec![cmd_hook("false", true, 5)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HookError::NonZeroExit { .. }));
    }

    #[tokio::test]
    async fn fire_hooks_timeout() {
        let hooks = vec![cmd_hook("sleep 10", true, 1)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, HookError::Timeout { .. }));
    }

    #[tokio::test]
    async fn fire_hooks_env_passed() {
        let hooks = vec![cmd_hook(r#"test "$ZEPH_TEST_VAR" = "hello""#, true, 5)];
        let mut env = HashMap::new();
        env.insert("ZEPH_TEST_VAR".to_owned(), "hello".to_owned());
        assert!(fire_hooks(&hooks, &env, None, None).await.is_ok());
    }

    #[tokio::test]
    async fn fire_hooks_empty_list_ok() {
        let env = HashMap::new();
        assert!(fire_hooks(&[], &env, None, None).await.is_ok());
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
        assert!(fire_hooks(&hooks, &env, None, None).await.is_ok());
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
        let result = fire_hooks(&hooks, &env, None, None).await;
        assert!(matches!(result, Err(HookError::McpUnavailable { .. })));
    }

    // ── MCP dispatch tests (#3773) ────────────────────────────────────────────

    /// Stub MCP dispatch that records how many times it was called.
    struct CountingDispatch(std::sync::Arc<std::sync::atomic::AtomicU32>);

    impl McpDispatch for CountingDispatch {
        fn call_tool<'a>(
            &'a self,
            _server: &'a str,
            _tool: &'a str,
            _args: serde_json::Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
        > {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(std::future::ready(Ok(serde_json::Value::Null)))
        }
    }

    #[tokio::test]
    async fn fire_hooks_mcp_dispatch_called_when_provided() {
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let dispatch = CountingDispatch(std::sync::Arc::clone(&call_count));

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
        let result = fire_hooks(&hooks, &env, Some(&dispatch), None).await;
        assert!(
            result.is_ok(),
            "fire_hooks should succeed with mcp dispatch"
        );
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "MCP dispatch should have been called exactly once"
        );
    }

    // ── stdout replacement tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn fire_hooks_stdout_replacement_json() {
        let cmd = r#"printf '{"hookSpecificOutput":{"updatedToolOutput":"replaced"}}'"#;
        let hooks = vec![cmd_hook(cmd, true, 5)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None, None).await.unwrap();
        assert_eq!(
            result.output.updated_tool_output.as_deref(),
            Some("replaced")
        );
    }

    #[tokio::test]
    async fn fire_hooks_stdout_empty_no_replacement() {
        let hooks = vec![cmd_hook("true", true, 5)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None, None).await.unwrap();
        assert!(result.output.updated_tool_output.is_none());
    }

    #[tokio::test]
    async fn fire_hooks_stdout_non_json_no_replacement() {
        let hooks = vec![cmd_hook("echo hello", true, 5)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None, None).await.unwrap();
        assert!(result.output.updated_tool_output.is_none());
    }

    #[tokio::test]
    async fn fire_hooks_stdout_null_updatedtooloutput_no_replacement() {
        let cmd = r#"printf '{"hookSpecificOutput":{"updatedToolOutput":null}}'"#;
        let hooks = vec![cmd_hook(cmd, true, 5)];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None, None).await.unwrap();
        assert!(result.output.updated_tool_output.is_none());
    }

    #[tokio::test]
    async fn fire_hooks_stdin_passed_to_hook() {
        // Hook reads stdin and checks that "duration_ms" key is present in the JSON.
        let cmd = r#"python3 -c "import sys,json; d=json.load(sys.stdin); exit(0 if 'duration_ms' in d else 1)""#;
        let hooks = vec![cmd_hook(cmd, true, 10)];
        let env = HashMap::new();
        let stdin = br#"{"tool_name":"Shell","tool_args":{},"duration_ms":42}"#;
        let result = fire_hooks(&hooks, &env, None, Some(stdin)).await;
        assert!(
            result.is_ok(),
            "hook should succeed when stdin has duration_ms"
        );
    }

    #[tokio::test]
    async fn fire_hooks_chaining_last_replacement_wins() {
        // First hook produces replacement "first", second produces "second" — second wins.
        let h1 = cmd_hook(
            r#"printf '{"hookSpecificOutput":{"updatedToolOutput":"first"}}'"#,
            false,
            5,
        );
        let h2 = cmd_hook(
            r#"printf '{"hookSpecificOutput":{"updatedToolOutput":"second"}}'"#,
            false,
            5,
        );
        let hooks = vec![h1, h2];
        let env = HashMap::new();
        let result = fire_hooks(&hooks, &env, None, None).await.unwrap();
        assert_eq!(result.output.updated_tool_output.as_deref(), Some("second"));
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

    // ── regression: #4011 ────────────────────────────────────────────────────

    /// Regression for #4011: a hook that writes to stdout and then hangs must be killed
    /// within `timeout_secs` and return `HookError::Timeout`.  The old `tokio::join!`
    /// implementation deadlocked here because the stdout reader blocked on EOF while
    /// `child.kill()` was gated behind the join completing.
    #[tokio::test]
    async fn fire_shell_hook_timeout_with_stdout_does_not_deadlock() {
        // Write a line to stdout, then block forever — this is the exact pattern that
        // triggered the deadlock in the original implementation.
        let cmd = r#"echo "some output"; sleep 60"#;
        let hooks = vec![cmd_hook(cmd, true, 1)];
        let env = HashMap::new();

        // Must complete in bounded time (the timeout is 1 s; allow 5 s total for CI variance).
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fire_hooks(&hooks, &env, None, None),
        )
        .await
        .expect("fire_hooks must return within 5 s — deadlock regression #4011");

        assert!(
            matches!(result, Err(HookError::Timeout { .. })),
            "expected HookError::Timeout, got: {result:?}"
        );
    }
}
