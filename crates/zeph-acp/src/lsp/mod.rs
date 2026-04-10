// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LSP extension for ACP — IDE code intelligence via `ext_method`.
//!
//! Provides `LspProvider` trait and implementations for IDE-proxied (ACP) and
//! standalone (MCP/mcpls) LSP sources, plus a bounded `DiagnosticsCache` for
//! diagnostics pushed by the IDE.

pub mod acp_provider;
pub mod cache;
pub mod mcp_provider;
pub mod provider;
pub mod types;

pub use acp_provider::AcpLspProvider;
pub use cache::DiagnosticsCache;
pub use mcp_provider::McpLspProvider;
pub use provider::LspProvider;
pub use types::{
    LspCodeAction, LspDiagnostic, LspDiagnosticSeverity, LspDocumentSymbol, LspHoverResult,
    LspLocation, LspPosition, LspRange, LspSymbolInformation, LspSymbolKind, LspTextEdit,
    LspWorkspaceEdit,
};

/// ACP `ext_method` names advertised in `InitializeResponse` capability meta
/// when the agent supports IDE-proxied LSP.
///
/// The IDE checks this list to know which extension methods to enable on its LSP bridge.
pub const LSP_METHODS: &[&str] = &[
    "lsp/hover",
    "lsp/definition",
    "lsp/references",
    "lsp/diagnostics",
    "lsp/documentSymbols",
    "lsp/workspaceSymbol",
    "lsp/codeActions",
];

/// ACP notification method names that the agent registers handlers for.
///
/// The IDE sends these as unsolicited notifications; the agent updates its
/// [`DiagnosticsCache`] and refreshes its session context accordingly.
pub const LSP_NOTIFICATIONS: &[&str] = &["lsp/publishDiagnostics", "lsp/didSave"];
