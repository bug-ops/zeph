// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`McpCaller`] trait — minimal async interface over `McpManager`.
//!
//! Used by `lsp_hooks` to abstract over the real manager and test stubs.

use std::future::Future;

use rmcp::model::CallToolResult;

use crate::error::McpError;

/// Minimal async interface over `McpManager` required by `lsp_hooks`.
///
/// Implemented by `McpManager` (real transport) and `MockMcpCaller`
/// (test stub, enabled via the `mock` feature).
pub trait McpCaller: Send + Sync {
    fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> impl Future<Output = Result<CallToolResult, McpError>> + Send;

    fn list_servers(&self) -> impl Future<Output = Vec<String>> + Send;
}

impl McpCaller for crate::manager::McpManager {
    async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        self.call_tool(server_id, tool_name, args).await
    }

    async fn list_servers(&self) -> Vec<String> {
        self.list_servers().await
    }
}
