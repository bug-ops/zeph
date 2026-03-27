// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test helpers for `zeph-mcp`.
//!
//! Provides `MockMcpServer` — an in-process MCP server stub that returns
//! pre-configured tool lists and call results without spawning a real process
//! or opening any network connections.
//!
//! The mock operates at the [`McpToolExecutor`] / [`McpTool`] level, bypassing
//! the rmcp transport layer.  This is sufficient for testing the agent
//! integration path (tool discovery → prompt formatting → tool call dispatch).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use zeph_tools::registry::{InvocationHint, ToolDef};

use crate::tool::McpTool;

type MockResponses = Arc<Mutex<HashMap<String, Vec<Result<String, String>>>>>;
type RecordedCalls = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

/// Configurable MCP tool executor for unit tests.
///
/// Pre-loads a set of [`McpTool`] definitions and maps tool IDs to canned
/// string responses.  Each [`execute_tool_call`] pops the next response for
/// the requested tool, or returns an error if the tool is unknown.
pub struct MockMcpServer {
    tools: Vec<McpTool>,
    /// Map: `"server_id:tool_name"` → queue of responses to return in order.
    responses: MockResponses,
    /// Records every tool call received.
    pub recorded_calls: RecordedCalls,
}

impl MockMcpServer {
    /// Create an empty mock server.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            responses: Arc::new(Mutex::new(HashMap::new())),
            recorded_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a tool definition.
    #[must_use]
    pub fn with_tool(
        mut self,
        server_id: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        self.tools.push(McpTool {
            server_id: server_id.into(),
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        });
        self
    }

    /// Add a successful response for a tool.  Responses are consumed in order;
    /// if exhausted, subsequent calls return an error.
    ///
    /// `tool_id` must be in `"server_id:tool_name"` format.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock response mutex is poisoned.
    #[must_use]
    pub fn with_response(self, tool_id: impl Into<String>, response: impl Into<String>) -> Self {
        self.responses
            .lock()
            .unwrap()
            .entry(tool_id.into())
            .or_default()
            .push(Ok(response.into()));
        self
    }

    /// Add an error response for a tool.
    ///
    /// # Panics
    ///
    /// Panics if the internal mock response mutex is poisoned.
    #[must_use]
    pub fn with_error(self, tool_id: impl Into<String>, message: impl Into<String>) -> Self {
        self.responses
            .lock()
            .unwrap()
            .entry(tool_id.into())
            .or_default()
            .push(Err(message.into()));
        self
    }

    /// Return the list of registered [`McpTool`] definitions.
    #[must_use]
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }
}

impl Default for MockMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolExecutor for MockMcpServer {
    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| ToolDef {
                id: t.qualified_name().into(),
                description: t.description.clone().into(),
                schema: serde_json::from_value(t.input_schema.clone())
                    .unwrap_or_else(|_| schemars::Schema::default()),
                invocation: InvocationHint::ToolCall,
            })
            .collect()
    }

    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        // Intentionally no-op: this mock operates at the ToolExecutor level
        // and bypasses the rmcp transport layer entirely.  Tool dispatch always
        // goes through execute_tool_call; execute is never called in tests.
        Ok(None)
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        // Record the call for test assertions.
        self.recorded_calls.lock().unwrap().push((
            call.tool_id.clone(),
            serde_json::Value::Object(call.params.clone()),
        ));

        // Check the tool exists.
        let known = self
            .tools
            .iter()
            .any(|t| t.qualified_name() == call.tool_id);
        if !known {
            return Ok(None);
        }

        // Pop the next pre-configured response.
        let outcome = self
            .responses
            .lock()
            .unwrap()
            .get_mut(&call.tool_id)
            .and_then(|queue| {
                if queue.is_empty() {
                    None
                } else {
                    Some(queue.remove(0))
                }
            });

        match outcome {
            Some(Ok(text)) => Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: text,
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            })),
            Some(Err(msg)) => Err(ToolError::Blocked { command: msg }),
            None => Err(ToolError::Blocked {
                command: format!("MockMcpServer: no response queued for {}", call.tool_id),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_returns_registered_tools() {
        let mock = MockMcpServer::new()
            .with_tool("server1", "bash", "Run bash")
            .with_tool("server1", "read_file", "Read a file");

        let defs = mock.tool_definitions();
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().any(|d| d.id.as_ref() == "server1:bash"));
        assert!(defs.iter().any(|d| d.id.as_ref() == "server1:read_file"));
    }

    #[tokio::test]
    async fn execute_tool_call_returns_canned_response() {
        let mock = MockMcpServer::new()
            .with_tool("srv", "echo", "Echo tool")
            .with_response("srv:echo", "hello from mock");

        let call = ToolCall {
            tool_id: "srv:echo".into(),
            params: serde_json::Map::new(),
        };
        let result = mock.execute_tool_call(&call).await.unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().summary, "hello from mock");
    }

    #[tokio::test]
    async fn execute_tool_call_unknown_tool_returns_none() {
        let mock = MockMcpServer::new();
        let call = ToolCall {
            tool_id: "srv:unknown".into(),
            params: serde_json::Map::new(),
        };
        let result = mock.execute_tool_call(&call).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn execute_tool_call_error_response_propagates() {
        let mock = MockMcpServer::new()
            .with_tool("srv", "fail", "Failing tool")
            .with_error("srv:fail", "intentional test error");

        let call = ToolCall {
            tool_id: "srv:fail".into(),
            params: serde_json::Map::new(),
        };
        let result = mock.execute_tool_call(&call).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_tool_call_records_calls() {
        let mock = MockMcpServer::new()
            .with_tool("srv", "ping", "Ping")
            .with_response("srv:ping", "pong");

        let call = ToolCall {
            tool_id: "srv:ping".into(),
            params: serde_json::Map::new(),
        };
        mock.execute_tool_call(&call).await.unwrap();

        let recorded = mock.recorded_calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "srv:ping");
    }

    #[tokio::test]
    async fn execute_tool_call_exhausted_queue_errors() {
        let mock = MockMcpServer::new()
            .with_tool("srv", "once", "One-shot tool")
            .with_response("srv:once", "first");

        let call = ToolCall {
            tool_id: "srv:once".into(),
            params: serde_json::Map::new(),
        };

        // First call succeeds.
        mock.execute_tool_call(&call).await.unwrap().unwrap();

        // Second call errors — queue exhausted.
        let result = mock.execute_tool_call(&call).await;
        assert!(result.is_err());
    }
}
