// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool invocation: the [`McpManager::call_tool`] entry point and result extraction.

use std::time::Duration;

use rmcp::model::CallToolResult;
use zeph_config::McpTrustLevel;

use crate::error::McpError;

use super::McpManager;

impl McpManager {
    /// Returns `true` when `server_id` is configured with `media_passthrough = true` and its
    /// resolved trust level is not [`McpTrustLevel::Sandboxed`] (spec-072 C2: media
    /// passthrough is hard-blocked for Sandboxed servers regardless of the flag).
    ///
    /// Returns `false` for an unconfigured server ID (e.g. dynamically added without a
    /// static config entry).
    pub async fn media_passthrough_allowed(&self, server_id: &str) -> bool {
        let Some(entry) = self.configs.iter().find(|c| c.id == server_id) else {
            return false;
        };
        if !entry.media_passthrough {
            return false;
        }
        let trust_level = self
            .server_trust
            .read()
            .await
            .get(server_id)
            .map_or(entry.trust_level, |(t, _, _)| *t);
        trust_level != McpTrustLevel::Sandboxed
    }

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
