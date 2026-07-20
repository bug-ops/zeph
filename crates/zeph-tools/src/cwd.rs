// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;

use zeph_common::ToolName;

use crate::executor::{
    ClaimSource, ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params,
};
use crate::file::expand_tilde;
use crate::registry::{InvocationHint, ToolDef};

const TOOL_NAME: &str = "set_working_directory";

const TOOL_DESCRIPTION: &str = "Change the agent's working directory. \
Shell commands (`bash`) run in child processes — a `cd` inside them does NOT persist. \
Use this tool when you need to change the working context for subsequent operations. \
Returns the new absolute working directory path on success.";

#[derive(Deserialize, JsonSchema)]
struct SetCwdParams {
    /// Target directory path (absolute or relative to current working directory).
    path: String,
}

/// Resolve `path` (expanding `~`, and resolving relative paths against the current cwd),
/// verify it falls within `allowed_paths`, and change the process working directory to it.
/// Returns the new absolute (canonicalized) cwd on success.
///
/// Shared by [`SetCwdExecutor`] (the LLM-invoked `set_working_directory` tool) and the
/// user-invoked `/cd` slash command (`zeph-core`'s `WorktreeAccess::change_working_directory`) —
/// both entry points into the same underlying mechanism, per #6032 FR-001/FR-011: `/cd` must
/// not duplicate this resolution logic.
///
/// Per spec 063 FR-001/"Never" (SEC-2): `/cd` "must not become a bypass for the per-path file
/// read sandbox" — the sandbox check runs *before* `set_current_dir`, so a rejected path never
/// mutates the process cwd. `allowed_paths` uses the same `zeph_common::security` containment
/// check as `FileExecutor`/`DiagnosticsExecutor`; an empty slice matches those callers'
/// convention (see [`SetCwdExecutor::new`]) rather than allowing every path.
///
/// # Errors
///
/// Returns [`std::io::Error`] if the target path does not exist, is not a directory, is not
/// readable (mirrors `std::env::set_current_dir`'s error contract — see
/// `set_cwd_errors_on_nonexistent_path`), or falls outside `allowed_paths`
/// (`io::ErrorKind::PermissionDenied`).
pub fn resolve_and_set_cwd(path: &str, allowed_paths: &[PathBuf]) -> std::io::Result<PathBuf> {
    let target = expand_tilde(PathBuf::from(path));

    // Resolve relative paths against current cwd before changing.
    let resolved = if target.is_absolute() {
        target
    } else {
        std::env::current_dir()?.join(target)
    };

    let canonical = zeph_common::security::validate_path_within(&resolved, allowed_paths)?;
    std::env::set_current_dir(&canonical)?;
    Ok(canonical)
}

/// Tool executor that changes the agent process working directory.
///
/// Implements the `set_working_directory` tool. The LLM calls this when it needs
/// to change context for a series of operations. Shell `cd` inside child processes
/// has no effect on the agent's cwd — this tool is the only persistent mechanism.
///
/// Sandboxed to `allowed_paths` (#6032 SEC-2), mirroring [`crate::file::FileExecutor`] and
/// [`crate::diagnostics::DiagnosticsExecutor`] — a `cd` outside the configured sandbox is
/// rejected rather than silently permitted, closing a gap the LLM-invoked tool previously
/// shared with `/cd` before this fix.
#[derive(Debug)]
pub struct SetCwdExecutor {
    allowed_paths: Vec<PathBuf>,
}

impl SetCwdExecutor {
    /// Create a new executor sandboxed to `allowed_paths`.
    ///
    /// An empty `allowed_paths` defaults to `[current_dir]`, matching
    /// [`crate::file::FileExecutor::new`]/[`crate::diagnostics::DiagnosticsExecutor::new`]'s
    /// convention — not "allow every path".
    #[must_use]
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        let paths = if allowed_paths.is_empty() {
            vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
        } else {
            allowed_paths.into_iter().map(expand_tilde).collect()
        };
        Self {
            allowed_paths: paths
                .into_iter()
                .map(|p| p.canonicalize().unwrap_or(p))
                .collect(),
        }
    }
}

impl ToolExecutor for SetCwdExecutor {
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != TOOL_NAME {
            return Ok(None);
        }
        let params: SetCwdParams = deserialize_params(&call.params)?;
        let new_cwd =
            resolve_and_set_cwd(&params.path, &self.allowed_paths).map_err(ToolError::Execution)?;
        let summary = new_cwd.display().to_string();

        Ok(Some(ToolOutput {
            tool_name: ToolName::new(TOOL_NAME),
            summary,
            blocks_executed: 1,
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
            claim_source: Some(ClaimSource::FileSystem),
            ..Default::default()
        }))
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: TOOL_NAME.into(),
            description: TOOL_DESCRIPTION.into(),
            schema: schemars::schema_for!(SetCwdParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
            server_id: None,
        }]
    }

    fn is_tool_retryable(&self, _tool_id: &str) -> bool {
        false
    }

    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }

    crate::tool_executor_no_inner_defaults!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_call(path: &str) -> ToolCall {
        let mut params = serde_json::Map::new();
        params.insert(
            "path".to_owned(),
            serde_json::Value::String(path.to_owned()),
        );
        ToolCall {
            tool_id: ToolName::new(TOOL_NAME),
            params,
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    #[tokio::test]
    async fn set_cwd_changes_process_cwd() {
        let original_cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let executor = SetCwdExecutor::new(vec![dir.path().to_path_buf()]);
        let call = make_call(dir.path().to_str().unwrap());
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());
        let out = result.unwrap();
        // The returned summary is the new cwd.
        let new_cwd = std::env::current_dir().unwrap();
        assert_eq!(out.summary, new_cwd.display().to_string());
        // Restore cwd so parallel tests are not affected.
        let _ = std::env::set_current_dir(&original_cwd);
    }

    // --- tilde expansion regression (#5415) ---

    #[tokio::test]
    async fn set_cwd_expands_tilde_in_runtime_argument() {
        // Regression for #5415: a `~`-prefixed path coming from an LLM tool
        // call must resolve to the real home directory, mirroring the fix
        // for `DiagnosticsExecutor::validate_path`.
        let original_cwd = std::env::current_dir().unwrap();
        let home = dirs::home_dir().expect("home dir must be resolvable in test env");
        let subdir = tempfile::Builder::new()
            .prefix("zeph_test_cwd_tilde_")
            .tempdir_in(&home)
            .expect("failed to create temp dir under home");
        let dir_name = subdir.path().file_name().unwrap().to_str().unwrap();

        let executor = SetCwdExecutor::new(vec![subdir.path().to_path_buf()]);
        let call = make_call(&format!("~/{dir_name}"));
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());

        let new_cwd = std::env::current_dir().unwrap();
        assert_eq!(new_cwd, subdir.path().canonicalize().unwrap());
        assert!(
            !new_cwd.to_string_lossy().contains('~'),
            "tilde must not appear in resolved cwd: {new_cwd:?}"
        );

        // Restore cwd so parallel tests are not affected.
        let _ = std::env::set_current_dir(&original_cwd);
    }

    #[tokio::test]
    async fn set_cwd_returns_none_for_unknown_tool() {
        let executor = SetCwdExecutor::new(vec![]);
        let call = ToolCall {
            tool_id: ToolName::new("other_tool"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_cwd_errors_on_nonexistent_path() {
        let executor = SetCwdExecutor::new(vec![]);
        let call = make_call("/nonexistent/path/that/does/not/exist");
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    // --- sandbox enforcement regression (#6032 SEC-2) ---

    #[tokio::test]
    async fn set_cwd_rejects_path_outside_allowed_paths() {
        let original_cwd = std::env::current_dir().unwrap();
        let allowed_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let executor = SetCwdExecutor::new(vec![allowed_root.path().to_path_buf()]);

        let call = make_call(outside.path().to_str().unwrap());
        let result = executor.execute_tool_call(&call).await;

        assert!(
            result.is_err(),
            "cd to a directory outside allowed_paths must be rejected"
        );
        assert_eq!(
            std::env::current_dir().unwrap(),
            original_cwd,
            "process cwd must be unchanged after a rejected cd"
        );
    }

    #[test]
    fn resolve_and_set_cwd_rejects_path_outside_allowed_paths() {
        let original_cwd = std::env::current_dir().unwrap();
        let allowed_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let allowed = vec![allowed_root.path().canonicalize().unwrap()];

        let result = resolve_and_set_cwd(outside.path().to_str().unwrap(), &allowed);

        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::env::current_dir().unwrap(),
            original_cwd,
            "process cwd must be unchanged after a rejected cd"
        );
    }

    #[test]
    fn resolve_and_set_cwd_allows_path_inside_allowed_paths() {
        let original_cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![dir.path().canonicalize().unwrap()];

        let result = resolve_and_set_cwd(dir.path().to_str().unwrap(), &allowed);

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path().canonicalize().unwrap());
        let _ = std::env::set_current_dir(&original_cwd);
    }

    #[test]
    fn tool_definitions_contains_set_working_directory() {
        let executor = SetCwdExecutor::new(vec![]);
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), TOOL_NAME);
    }
}
