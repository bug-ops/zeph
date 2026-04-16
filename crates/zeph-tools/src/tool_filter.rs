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
                },
                ToolDef {
                    id: "glob".into(),
                    description: "find files".into(),
                    schema: schemars::schema_for!(String),
                    invocation: crate::registry::InvocationHint::ToolCall,
                    output_schema: None,
                },
                ToolDef {
                    id: "edit".into(),
                    description: "edit a file".into(),
                    schema: schemars::schema_for!(String),
                    invocation: crate::registry::InvocationHint::ToolCall,
                    output_schema: None,
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
        };
        let result = filter.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());
    }
}
