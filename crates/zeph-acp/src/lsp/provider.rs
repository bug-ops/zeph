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

/// Abstract interface over IDE-proxied (ACP) and standalone (MCP/mcpls) LSP sources.
///
/// Implementations must be `Send + Sync` so the agent loop can run on a multithreaded
/// tokio scheduler while still forwarding LSP requests back to the ACP control-plane thread.
///
/// All positions use **1-based** line and character coordinates (ACP/MCP convention).
/// The IDE is responsible for converting to 0-based LSP coordinates on its side.
///
/// # Implementations
///
/// | Type | Source | Feature |
/// |------|--------|---------|
/// | [`AcpLspProvider`] | IDE via ACP `ext_method` | always |
/// | [`McpLspProvider`] | `mcpls` MCP server | always |
///
/// [`AcpLspProvider`]: super::AcpLspProvider
/// [`McpLspProvider`]: super::McpLspProvider
pub trait LspProvider: Send + Sync {
    /// Resolve hover information at the given 1-based position.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::ClientError`] if the IDE or
    /// MCP server returns an error, or [`AcpError::ChannelClosed`]
    /// if the connection was dropped.
    fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl std::future::Future<Output = Result<LspHoverResult, AcpError>>;

    /// Resolve definition location(s) at the given 1-based position.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::ClientError`] on protocol failure.
    fn definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> impl std::future::Future<Output = Result<Vec<LspLocation>, AcpError>>;

    /// Find all references at the given 1-based position.
    ///
    /// Results are truncated to the provider's configured `max_references` limit.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::ClientError`] on protocol failure.
    fn references(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> impl std::future::Future<Output = Result<Vec<LspLocation>, AcpError>>;

    /// Fetch current diagnostics for a file URI.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::ClientError`] on protocol failure.
    fn diagnostics(
        &self,
        uri: &str,
    ) -> impl std::future::Future<Output = Result<Vec<LspDiagnostic>, AcpError>>;

    /// Fetch the document symbol tree for a file URI.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::ClientError`] on protocol failure.
    fn document_symbols(
        &self,
        uri: &str,
    ) -> impl std::future::Future<Output = Result<Vec<LspDocumentSymbol>, AcpError>>;

    /// Search for symbols matching a query across the entire workspace.
    ///
    /// Results are truncated to the provider's configured `max_workspace_symbols` limit.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::ClientError`] on protocol failure.
    fn workspace_symbol(
        &self,
        query: &str,
    ) -> impl std::future::Future<Output = Result<Vec<LspSymbolInformation>, AcpError>>;

    /// Fetch available code actions for a range and optional diagnostics context.
    ///
    /// Actions without a workspace edit are filtered out on the agent side so that
    /// only actionable (apply-able) actions are returned.
    ///
    /// # Errors
    ///
    /// Returns [`AcpError::ClientError`] on protocol failure.
    fn code_actions(
        &self,
        uri: &str,
        range: &LspRange,
        diagnostics: &[LspDiagnostic],
    ) -> impl std::future::Future<Output = Result<Vec<LspCodeAction>, AcpError>>;

    /// Human-readable provider name for logging and diagnostics (e.g. `"acp"`, `"mcp/mcpls"`).
    fn name(&self) -> &'static str;

    /// Returns `true` when the provider can currently serve requests.
    ///
    /// `false` means the underlying channel is closed or the IDE does not support LSP.
    fn is_available(&self) -> bool;
}
