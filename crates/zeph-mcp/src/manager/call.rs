// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool invocation: the [`McpManager::call_tool`] entry point and result extraction.

use std::time::Duration;

use rmcp::model::CallToolResult;

use crate::error::McpError;

use super::McpManager;

impl McpManager {
    /// Route tool call to the correct server's client.
    ///
    /// # Errors
    ///
    /// Returns `McpError::PolicyViolation` if the enforcer rejects the call,
    /// or `McpError::ServerNotFound` if the server is not connected.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.manager.call_tool", skip_all, fields(server_id = %server_id, tool_name = %tool_name))
    )]
    pub async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        self.enforcer
            .check(server_id, tool_name)
            .map_err(|v| McpError::PolicyViolation(v.to_string()))?;

        let clients = self.clients.read().await;
        let client = clients
            .get(server_id)
            .ok_or_else(|| McpError::ServerNotFound {
                server_id: server_id.into(),
            })?;
        let result = if let Some(tool_timeout_secs) = self.tool_timeout_secs {
            client
                .call_tool_with_timeout(tool_name, args, Duration::from_secs(tool_timeout_secs))
                .await?
        } else {
            client.call_tool(tool_name, args).await?
        };

        if let Some(ref guard) = self.embedding_guard {
            let text = extract_text_content(&result);
            if !text.is_empty() {
                guard.check_async(server_id, tool_name, &text);
            }
        }

        Ok(result)
    }
}

/// Render a tool result's content blocks to text for the embedding anomaly guard.
fn extract_text_content(result: &CallToolResult) -> String {
    crate::content::render_content_blocks(&result.content)
}
