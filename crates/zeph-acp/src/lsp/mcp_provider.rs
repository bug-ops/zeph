// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `McpLspProvider`: delegates LSP requests to the `mcpls` MCP server.
//!
//! Maps `LspProvider` methods to `mcpls` tool calls via `McpManager::call_tool`.
//! Detected at startup when `McpManager` has a server with a `"get_hover"` tool.
//!
//! # !Send constraint
//!
//! `McpManager` uses `Arc<RwLock<...>>` internally and is `Send + Sync`, but
//! `McpLspProvider` is co-located in the `!Send` LSP module for API consistency.
//! It may be called from a `LocalSet` context.

use std::sync::Arc;

use rmcp::model::RawContent;
use zeph_mcp::McpManager;

use crate::error::AcpError;

use super::provider::LspProvider;
use super::types::{
    LspCodeAction, LspDiagnostic, LspDocumentSymbol, LspHoverResult, LspLocation, LspRange,
    LspSymbolInformation,
};

/// MCP-backed LSP provider that delegates to the `mcpls` MCP server.
pub struct McpLspProvider {
    manager: Arc<McpManager>,
    /// Server ID of the mcpls instance in the manager.
    server_id: String,
    /// Maximum number of reference locations to return.
    max_references: usize,
    /// Maximum number of workspace symbol search results to return.
    max_workspace_symbols: usize,
}

impl McpLspProvider {
    /// Create a new provider for the given MCP manager and server ID.
    #[must_use]
    pub fn new(
        manager: Arc<McpManager>,
        server_id: impl Into<String>,
        max_references: usize,
        max_workspace_symbols: usize,
    ) -> Self {
        Self {
            manager,
            server_id: server_id.into(),
            max_references,
            max_workspace_symbols,
        }
    }

    async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, AcpError> {
        let result = self
            .manager
            .call_tool(&self.server_id, tool_name, args)
            .await
            .map_err(|e| AcpError::ClientError(e.to_string()))?;

        // Extract text content from the first content block.
        let text: String = result
            .content
            .iter()
            .find_map(|c| {
                if let RawContent::Text(t) = &c.raw {
                    Some(t.text.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| AcpError::ClientError("mcpls returned no text content".to_owned()))?;

        // Check is_error before JSON parsing to surface the actual mcpls error message
        // instead of an opaque serde parse failure.
        if result.is_error == Some(true) {
            return Err(AcpError::ClientError(format!("mcpls error: {text}")));
        }

        serde_json::from_str(&text).map_err(|e| AcpError::ClientError(e.to_string()))
    }
}

impl LspProvider for McpLspProvider {
    fn name(&self) -> &'static str {
        "mcp/mcpls"
    }

    fn is_available(&self) -> bool {
        // TODO: check McpManager for server liveness (e.g. manager.is_server_connected).
        // Currently `McpManager` does not expose a liveness check, so we return `true`
        // when constructed ("configured") rather than "connected". If the mcpls server
        // disconnects, call_tool() will surface an error on first use. Priority fallback
        // logic should account for this limitation when it is implemented.
        true
    }

    async fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<LspHoverResult, AcpError> {
        let args = serde_json::json!({ "file_path": uri, "line": line, "character": character });
        self.call_tool("get_hover", args).await.and_then(|v| {
            serde_json::from_value(v).map_err(|e| AcpError::ClientError(e.to_string()))
        })
    }

    async fn definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, AcpError> {
        let args = serde_json::json!({ "file_path": uri, "line": line, "character": character });
        self.call_tool("get_definition", args).await.and_then(|v| {
            serde_json::from_value(v).map_err(|e| AcpError::ClientError(e.to_string()))
        })
    }

    async fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<LspLocation>, AcpError> {
        let args = serde_json::json!({
            "file_path": uri,
            "line": line,
            "character": character,
            "include_declaration": include_declaration,
        });
        let mut result: Vec<LspLocation> =
            self.call_tool("get_references", args).await.and_then(|v| {
                serde_json::from_value(v).map_err(|e| AcpError::ClientError(e.to_string()))
            })?;
        result.truncate(self.max_references);
        Ok(result)
    }

    async fn diagnostics(&self, uri: &str) -> Result<Vec<LspDiagnostic>, AcpError> {
        let args = serde_json::json!({ "file_path": uri });
        self.call_tool("get_diagnostics", args).await.and_then(|v| {
            serde_json::from_value(v).map_err(|e| AcpError::ClientError(e.to_string()))
        })
    }

    async fn document_symbols(&self, uri: &str) -> Result<Vec<LspDocumentSymbol>, AcpError> {
        let args = serde_json::json!({ "file_path": uri });
        self.call_tool("get_document_symbols", args)
            .await
            .and_then(|v| {
                serde_json::from_value(v).map_err(|e| AcpError::ClientError(e.to_string()))
            })
    }

    async fn workspace_symbol(&self, query: &str) -> Result<Vec<LspSymbolInformation>, AcpError> {
        let args = serde_json::json!({ "query": query });
        let mut result: Vec<LspSymbolInformation> = self
            .call_tool("workspace_symbol_search", args)
            .await
            .and_then(|v| {
                serde_json::from_value(v).map_err(|e| AcpError::ClientError(e.to_string()))
            })?;
        result.truncate(self.max_workspace_symbols);
        Ok(result)
    }

    async fn code_actions(
        &self,
        uri: &str,
        range: &LspRange,
        _diagnostics: &[LspDiagnostic],
    ) -> Result<Vec<LspCodeAction>, AcpError> {
        let args = serde_json::json!({
            "file_path": uri,
            "start_line": range.start.line,
            "start_character": range.start.character,
            "end_line": range.end.line,
            "end_character": range.end.character,
        });
        let value = self.call_tool("get_code_actions", args).await?;
        let actions: Vec<LspCodeAction> =
            serde_json::from_value(value).map_err(|e| AcpError::ClientError(e.to_string()))?;
        // Filter out actions without workspace edits (M5).
        Ok(actions.into_iter().filter(|a| a.edit.is_some()).collect())
    }
}
