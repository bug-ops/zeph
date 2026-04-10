// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `AcpLspProvider`: sends LSP requests to the IDE via ACP `ext_method`.
//!
//! LSP extension methods (`lsp/hover`, `lsp/definition`, etc.) are **agent→client**
//! requests — the agent sends them to the IDE, which proxies to its active LSP server.
//! This is the opposite direction from `_session/*` and `_agent/*` methods, which are
//! client→agent. `AcpLspProvider` uses `conn.ext_method()` (the `acp::Client` trait)
//! to send these outbound requests.
//!
use std::sync::Arc;
use std::time::Duration;

use acp::Client as _;
use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot};

use crate::error::AcpError;

use super::provider::LspProvider;
use super::types::{
    LspCodeAction, LspDiagnostic, LspDocumentSymbol, LspHoverResult, LspLocation, LspRange,
    LspSymbolInformation,
};

enum LspRequest {
    ExtMethod {
        method: &'static str,
        params: serde_json::Value,
        reply: oneshot::Sender<Result<serde_json::Value, AcpError>>,
    },
}

/// ACP-backed LSP provider that relays requests to the connected IDE.
///
/// Created in `build_acp_context()` when the client advertises `meta["lsp"]`
/// capability during `initialize()`. Falls back to `None` when the IDE does not
/// support LSP extension methods.
#[derive(Clone)]
pub struct AcpLspProvider {
    /// Whether the IDE advertised LSP support during `initialize()`.
    ide_supports_lsp: bool,
    request_tx: mpsc::UnboundedSender<LspRequest>,
    /// Timeout for each LSP `ext_method` call.
    request_timeout: Duration,
    /// Maximum number of reference locations to return.
    max_references: usize,
    /// Maximum number of workspace symbol search results to return.
    max_workspace_symbols: usize,
}

impl AcpLspProvider {
    /// Create a new ACP LSP provider and its `LocalSet`-side handler future.
    ///
    /// Spawn the returned future inside the same `LocalSet` that owns `conn`; it
    /// drives the internal request loop that calls `conn.ext_method()`.
    ///
    /// # Parameters
    ///
    /// - `conn` — ACP client connection (must live on the same `LocalSet`).
    /// - `ide_supports_lsp` — set from `client_caps.meta["lsp"]` during `initialize()`.
    /// - `request_timeout_secs` — per-request timeout for `ext_method` calls.
    /// - `max_references` — truncation limit for `lsp/references` results.
    /// - `max_workspace_symbols` — truncation limit for `lsp/workspaceSymbol` results.
    pub fn new<C>(
        conn: std::rc::Rc<C>,
        ide_supports_lsp: bool,
        request_timeout_secs: u64,
        max_references: usize,
        max_workspace_symbols: usize,
    ) -> (Self, impl std::future::Future<Output = ()>)
    where
        C: acp::Client + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let handler = async move { run_lsp_handler(conn, rx).await };
        (
            Self {
                ide_supports_lsp,
                request_tx: tx,
                request_timeout: Duration::from_secs(request_timeout_secs),
                max_references,
                max_workspace_symbols,
            },
            handler,
        )
    }

    fn call_ext_method(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> impl std::future::Future<Output = Result<serde_json::Value, AcpError>> + '_ {
        let timeout = self.request_timeout;
        async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.request_tx
                .send(LspRequest::ExtMethod {
                    method,
                    params,
                    reply: reply_tx,
                })
                .map_err(|_| AcpError::ChannelClosed)?;

            tokio::time::timeout(timeout, reply_rx)
                .await
                .map_err(|_| AcpError::ClientError("LSP request timed out".to_owned()))?
                .map_err(|_| AcpError::ChannelClosed)?
        }
    }
}

async fn run_lsp_handler<C>(conn: std::rc::Rc<C>, mut rx: mpsc::UnboundedReceiver<LspRequest>)
where
    C: acp::Client + 'static,
{
    while let Some(request) = rx.recv().await {
        match request {
            LspRequest::ExtMethod {
                method,
                params,
                reply,
            } => {
                let result = async {
                    let raw = serde_json::value::to_raw_value(&params)
                        .map_err(|e| AcpError::ClientError(e.to_string()))?;
                    let req = acp::ExtRequest::new(method, Arc::from(raw));
                    let result = conn
                        .ext_method(req)
                        .await
                        .map_err(|e| AcpError::ClientError(e.to_string()))?;
                    serde_json::from_str(result.0.get())
                        .map_err(|e| AcpError::ClientError(e.to_string()))
                }
                .await;
                let _ = reply.send(result);
            }
        }
    }
}

impl LspProvider for AcpLspProvider {
    fn name(&self) -> &'static str {
        "acp"
    }

    fn is_available(&self) -> bool {
        self.ide_supports_lsp && !self.request_tx.is_closed()
    }

    async fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<LspHoverResult, AcpError> {
        let params = serde_json::json!({ "uri": uri, "line": line, "character": character });
        let value = self.call_ext_method("lsp/hover", params).await?;
        serde_json::from_value(value).map_err(|e| AcpError::ClientError(e.to_string()))
    }

    async fn definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, AcpError> {
        let params = serde_json::json!({ "uri": uri, "line": line, "character": character });
        let value = self.call_ext_method("lsp/definition", params).await?;
        serde_json::from_value(value).map_err(|e| AcpError::ClientError(e.to_string()))
    }

    async fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<LspLocation>, AcpError> {
        let params = serde_json::json!({
            "uri": uri,
            "line": line,
            "character": character,
            "include_declaration": include_declaration,
        });
        let value = self.call_ext_method("lsp/references", params).await?;
        let mut result: Vec<LspLocation> =
            serde_json::from_value(value).map_err(|e| AcpError::ClientError(e.to_string()))?;
        result.truncate(self.max_references);
        Ok(result)
    }

    async fn diagnostics(&self, uri: &str) -> Result<Vec<LspDiagnostic>, AcpError> {
        let params = serde_json::json!({ "uri": uri });
        let value = self.call_ext_method("lsp/diagnostics", params).await?;
        serde_json::from_value(value).map_err(|e| AcpError::ClientError(e.to_string()))
    }

    async fn document_symbols(&self, uri: &str) -> Result<Vec<LspDocumentSymbol>, AcpError> {
        let params = serde_json::json!({ "uri": uri });
        let value = self.call_ext_method("lsp/documentSymbols", params).await?;
        serde_json::from_value(value).map_err(|e| AcpError::ClientError(e.to_string()))
    }

    async fn workspace_symbol(&self, query: &str) -> Result<Vec<LspSymbolInformation>, AcpError> {
        let params = serde_json::json!({ "query": query });
        let value = self.call_ext_method("lsp/workspaceSymbol", params).await?;
        let mut result: Vec<LspSymbolInformation> =
            serde_json::from_value(value).map_err(|e| AcpError::ClientError(e.to_string()))?;
        result.truncate(self.max_workspace_symbols);
        Ok(result)
    }

    async fn code_actions(
        &self,
        uri: &str,
        range: &LspRange,
        diagnostics: &[LspDiagnostic],
    ) -> Result<Vec<LspCodeAction>, AcpError> {
        let params = serde_json::json!({
            "uri": uri,
            "range": range,
            "diagnostics": diagnostics,
        });
        let value = self.call_ext_method("lsp/codeActions", params).await?;
        let actions: Vec<LspCodeAction> =
            serde_json::from_value(value).map_err(|e| AcpError::ClientError(e.to_string()))?;
        // Filter out actions without workspace edits (M5).
        Ok(actions.into_iter().filter(|a| a.edit.is_some()).collect())
    }
}
