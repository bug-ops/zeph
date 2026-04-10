// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`McpCaller`] trait — minimal async interface over `McpManager`.
//!
//! Used by `lsp_hooks` to abstract over the real manager and test stubs.

use std::future::Future;

use rmcp::model::CallToolResult;

use crate::error::McpError;

/// Minimal async interface over [`McpManager`](crate::manager::McpManager) for tool dispatch.
///
/// Implemented by `McpManager` (real transport) and `MockMcpCaller`
/// (test stub, enabled via the `mock` feature).
///
/// This trait exists to allow callers (`lsp_hooks` and similar integration points) to
/// accept either the real manager or a test double without a generic parameter bound on
/// the full `McpManager` type.
///
/// # Examples
///
/// ```ignore
/// async fn dispatch(caller: &dyn McpCaller) {
///     let result = caller
///         .call_tool("github", "list_issues", serde_json::json!({}))
///         .await;
/// }
/// ```
pub trait McpCaller: Send + Sync {
    /// Call a named tool on a specific server with JSON arguments.
    ///
    /// # Errors
    ///
    /// Returns [`McpError`] on connection failure, policy violation, timeout,
    /// or any server-side error.
    fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> impl Future<Output = Result<CallToolResult, McpError>> + Send;

    /// Return the IDs of all currently connected servers.
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
