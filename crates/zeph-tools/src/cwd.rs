// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;

use zeph_common::ToolName;

use crate::executor::{
    ClaimSource, ToolCall, ToolError, ToolExecutor, ToolOutput, deserialize_params,
};
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

/// Tool executor that changes the agent process working directory.
///
/// Implements the `set_working_directory` tool. The LLM calls this when it needs
/// to change context for a series of operations. Shell `cd` inside child processes
/// has no effect on the agent's cwd — this tool is the only persistent mechanism.
#[derive(Debug, Default)]
pub struct SetCwdExecutor;

impl ToolExecutor for SetCwdExecutor {
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if call.tool_id != TOOL_NAME {
            return Ok(None);
        }
        let params: SetCwdParams = deserialize_params(&call.params)?;
        let target = PathBuf::from(&params.path);

        // Resolve relative paths against current cwd before changing.
        let resolved = if target.is_absolute() {
            target
        } else {
            std::env::current_dir()
                .map_err(ToolError::Execution)?
                .join(target)
        };

        std::env::set_current_dir(&resolved).map_err(ToolError::Execution)?;

        let new_cwd = std::env::current_dir().map_err(ToolError::Execution)?;
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
        }))
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            id: TOOL_NAME.into(),
            description: TOOL_DESCRIPTION.into(),
            schema: schemars::schema_for!(SetCwdParams),
            invocation: InvocationHint::ToolCall,
            output_schema: None,
        }]
    }

    fn is_tool_retryable(&self, _tool_id: &str) -> bool {
        false
    }

    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }
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
        }
    }

    #[tokio::test]
    async fn set_cwd_changes_process_cwd() {
        let original_cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let executor = SetCwdExecutor;
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

    #[tokio::test]
    async fn set_cwd_returns_none_for_unknown_tool() {
        let executor = SetCwdExecutor;
        let call = ToolCall {
            tool_id: ToolName::new("other_tool"),
            params: serde_json::Map::new(),
            caller_id: None,
        };
        let result = executor.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn set_cwd_errors_on_nonexistent_path() {
        let executor = SetCwdExecutor;
        let call = make_call("/nonexistent/path/that/does/not/exist");
        let result = executor.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    #[test]
    fn tool_definitions_contains_set_working_directory() {
        let executor = SetCwdExecutor;
        let defs = executor.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id.as_ref(), TOOL_NAME);
    }
}
