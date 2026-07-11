// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::registry::ToolDef;

/// Wraps a `ToolExecutor` and suppresses specified tool ids from both
/// `tool_definitions` and `execute_tool_call`.
///
/// Used to hide `FileExecutor` tools (e.g. `read`, `glob`) when
/// `AcpFileExecutor` provides equivalent IDE-proxied alternatives.
#[derive(Debug)]
pub struct ToolFilter<E: ToolExecutor> {
    inner: E,
    suppressed: &'static [&'static str],
}

impl<E: ToolExecutor> ToolFilter<E> {
    #[must_use]
    pub fn new(inner: E, suppressed: &'static [&'static str]) -> Self {
        Self { inner, suppressed }
    }
}

impl<E: ToolExecutor> ToolExecutor for ToolFilter<E> {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute(response).await
    }

    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute_confirmed(response).await
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.inner
            .tool_definitions()
            .into_iter()
            .filter(|d| !self.suppressed.contains(&d.id.as_ref()))
            .collect()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        if self.suppressed.contains(&call.tool_id.as_str()) {
            return Ok(None);
        }
        self.inner.execute_tool_call(call).await
    }

    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        if self.suppressed.contains(&call.tool_id.as_str()) {
            return Ok(None);
        }
        self.inner.execute_tool_call_confirmed(call).await
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        self.inner.set_effective_trust(level);
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_retryable(tool_id)
    }

    fn is_tool_speculatable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_speculatable(tool_id)
    }

    fn requires_confirmation(&self, call: &ToolCall) -> bool {
        self.inner.requires_confirmation(call)
    }

    fn checkpoint_undo(&self, n: usize) -> crate::executor::CheckpointActionResult {
        self.inner.checkpoint_undo(n)
    }

    fn checkpoint_redo(&self) -> crate::executor::CheckpointActionResult {
        self.inner.checkpoint_redo()
    }

    fn checkpoint_list(&self) -> crate::executor::CheckpointListResult {
        self.inner.checkpoint_list()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolName;

    #[derive(Debug)]
    struct StubExecutor;
    impl ToolExecutor for StubExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        fn tool_definitions(&self) -> Vec<ToolDef> {
            vec![
                ToolDef {
                    id: "read".into(),
                    description: "read a file".into(),
                    schema: schemars::schema_for!(String),
                    invocation: crate::registry::InvocationHint::ToolCall,
                    output_schema: None,
                    server_id: None,
                },
                ToolDef {
                    id: "glob".into(),
                    description: "find files".into(),
                    schema: schemars::schema_for!(String),
                    invocation: crate::registry::InvocationHint::ToolCall,
                    output_schema: None,
                    server_id: None,
                },
                ToolDef {
                    id: "edit".into(),
                    description: "edit a file".into(),
                    schema: schemars::schema_for!(String),
                    invocation: crate::registry::InvocationHint::ToolCall,
                    output_schema: None,
                    server_id: None,
                },
            ]
        }
        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "stub".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }
    }

    #[test]
    fn suppressed_tools_hidden_from_definitions() {
        let filter = ToolFilter::new(StubExecutor, &["read", "glob"]);
        let defs = filter.tool_definitions();
        let ids: Vec<&str> = defs.iter().map(|d| d.id.as_ref()).collect();
        assert!(!ids.contains(&"read"));
        assert!(!ids.contains(&"glob"));
        assert!(ids.contains(&"edit"));
    }

    #[tokio::test]
    async fn suppressed_tool_call_returns_none() {
        let filter = ToolFilter::new(StubExecutor, &["read", "glob"]);
        let call = ToolCall {
            tool_id: ToolName::new("read"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = filter.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn allowed_tool_call_passes_through() {
        let filter = ToolFilter::new(StubExecutor, &["read", "glob"]);
        let call = ToolCall {
            tool_id: ToolName::new("edit"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,

            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = filter.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());
    }

    /// Inner executor whose cross-cutting methods return distinguishable non-default
    /// values, used to prove `ToolFilter` forwards rather than falling through to the
    /// base `ToolExecutor` defaults.
    #[derive(Debug)]
    struct CrossCuttingStubExecutor;

    impl ToolExecutor for CrossCuttingStubExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "stub".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }
        async fn execute_tool_call_confirmed(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "stub-confirmed".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }
        fn is_tool_retryable(&self, _tool_id: &str) -> bool {
            true
        }
        fn is_tool_speculatable(&self, _tool_id: &str) -> bool {
            true
        }
        fn requires_confirmation(&self, _call: &ToolCall) -> bool {
            true
        }
        fn checkpoint_undo(&self, _n: usize) -> crate::executor::CheckpointActionResult {
            crate::executor::CheckpointActionResult {
                reverted_commands: 1,
                restored: 0,
                deleted: 0,
                supported: true,
                message: "stub-undo".to_owned(),
            }
        }
        fn checkpoint_redo(&self) -> crate::executor::CheckpointActionResult {
            crate::executor::CheckpointActionResult {
                reverted_commands: 0,
                restored: 1,
                deleted: 0,
                supported: true,
                message: "stub-redo".to_owned(),
            }
        }
        fn checkpoint_list(&self) -> crate::executor::CheckpointListResult {
            crate::executor::CheckpointListResult {
                entries: vec![],
                redo_depth: 3,
                supported: true,
            }
        }
    }

    fn make_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: ToolName::new(tool_id),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    /// Regression test for #6012: cross-cutting methods must be forwarded to `self.inner`.
    /// Before the fix every one of these fell through to the base `ToolExecutor` default
    /// (`false` / `unsupported()`) regardless of the inner executor's actual policy.
    #[test]
    fn cross_cutting_methods_delegated_to_inner() {
        let filter = ToolFilter::new(CrossCuttingStubExecutor, &["read", "glob"]);

        assert!(filter.is_tool_retryable("edit"));
        assert!(filter.is_tool_speculatable("edit"));
        assert!(filter.requires_confirmation(&make_call("edit")));

        let undo = filter.checkpoint_undo(1);
        assert!(undo.supported);
        assert_eq!(undo.message, "stub-undo");

        let redo = filter.checkpoint_redo();
        assert!(redo.supported);
        assert_eq!(redo.message, "stub-redo");

        let list = filter.checkpoint_list();
        assert!(list.supported);
        assert_eq!(list.redo_depth, 3);
    }

    /// Suppression must also apply to the confirmed-call dispatch path, not just the
    /// initial (unconfirmed) `execute_tool_call`.
    #[tokio::test]
    async fn suppressed_tool_call_confirmed_returns_none() {
        let filter = ToolFilter::new(CrossCuttingStubExecutor, &["read", "glob"]);
        let result = filter
            .execute_tool_call_confirmed(&make_call("read"))
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn allowed_tool_call_confirmed_passes_through() {
        let filter = ToolFilter::new(CrossCuttingStubExecutor, &["read", "glob"]);
        let result = filter
            .execute_tool_call_confirmed(&make_call("edit"))
            .await
            .unwrap()
            .unwrap();
        // Distinguishes forwarding to execute_tool_call_confirmed from an (incorrect)
        // fallback to execute_tool_call — the two return different summaries.
        assert_eq!(result.summary, "stub-confirmed");
    }
}
