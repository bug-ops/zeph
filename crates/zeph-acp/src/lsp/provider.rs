// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `LspProvider` trait — abstract interface over ACP and MCP LSP sources.
//!
//! # Thread safety
//!
//! `LspProvider` implementations are `Send + Sync` so ACP sessions can run the
//! agent loop on the multithreaded tokio scheduler while proxying IDE requests
//! back through a dedicated ACP control-plane runtime.

use crate::error::AcpError;

use super::types::{
    LspCodeAction, LspDiagnostic, LspDocumentSymbol, LspHoverResult, LspLocation, LspRange,
    LspSymbolInformation,
};

/// Abstract interface over IDE-proxied (ACP) and standalone (MCP) LSP sources.
///
pub trait LspProvider: Send + Sync {
    /// Resolve hover information at the given 1-based position.
    fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl std::future::Future<Output = Result<LspHoverResult, AcpError>>;

    /// Resolve definition location(s) at the given 1-based position.
    fn definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl std::future::Future<Output = Result<Vec<LspLocation>, AcpError>>;

    /// Find all references at the given 1-based position.
    fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> impl std::future::Future<Output = Result<Vec<LspLocation>, AcpError>>;

    /// Fetch current diagnostics for a file.
    fn diagnostics(
        &self,
        uri: &str,
    ) -> impl std::future::Future<Output = Result<Vec<LspDiagnostic>, AcpError>>;

    /// Fetch the document symbol tree for a file.
    fn document_symbols(
        &self,
        uri: &str,
    ) -> impl std::future::Future<Output = Result<Vec<LspDocumentSymbol>, AcpError>>;

    /// Search for symbols matching a query across the workspace.
    fn workspace_symbol(
        &self,
        query: &str,
    ) -> impl std::future::Future<Output = Result<Vec<LspSymbolInformation>, AcpError>>;

    /// Fetch available code actions for a range and optional diagnostics context.
    ///
    /// Actions without a workspace edit are filtered out on the agent side (M5).
    fn code_actions(
        &self,
        uri: &str,
        range: &LspRange,
        diagnostics: &[LspDiagnostic],
    ) -> impl std::future::Future<Output = Result<Vec<LspCodeAction>, AcpError>>;

    /// Human-readable provider name (e.g. `"acp"`, `"mcp/mcpls"`).
    fn name(&self) -> &'static str;

    /// Returns `true` when the provider can currently serve requests.
    fn is_available(&self) -> bool;
}
