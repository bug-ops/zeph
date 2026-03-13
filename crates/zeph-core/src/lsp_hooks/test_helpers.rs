// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared test helpers for `lsp_hooks` unit tests.

use std::sync::{Arc, Mutex};

use rmcp::model::{CallToolResult, Content};
use zeph_mcp::McpCaller;
use zeph_mcp::error::McpError;

/// Minimal [`McpCaller`] stub that records every `call_tool` invocation and
/// returns pre-configured text responses from a FIFO queue.
///
/// Used by `diagnostics` and `hover` test modules to verify that the correct
/// argument keys are passed to `call_tool` without a live MCP server.
pub(super) struct RecordingCaller {
    /// `(server_id, tool_name, args)` tuples recorded in call order.
    pub(super) calls: Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
    responses: Arc<Mutex<Vec<Result<CallToolResult, McpError>>>>,
}

impl RecordingCaller {
    pub(super) fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(super) fn with_text(self, text: &str) -> Self {
        self.responses
            .lock()
            .unwrap()
            .push(Ok(CallToolResult::success(vec![Content::text(text)])));
        self
    }
}

impl McpCaller for RecordingCaller {
    async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        self.calls
            .lock()
            .unwrap()
            .push((server_id.to_owned(), tool_name.to_owned(), args));
        let mut queue = self.responses.lock().unwrap();
        if queue.is_empty() {
            return Err(McpError::ServerNotFound {
                server_id: server_id.to_owned(),
            });
        }
        queue.remove(0)
    }

    async fn list_servers(&self) -> Vec<String> {
        vec!["mcpls".to_owned()]
    }
}
