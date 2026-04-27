// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for the tool dispatcher.

use thiserror::Error;

/// Errors that can occur during tool dispatch.
///
/// The caller in `zeph-core` maps these to `AgentError` via `From<ToolDispatchError>`.
#[derive(Debug, Error)]
pub enum ToolDispatchError {
    /// LLM provider returned an error during tool-loop inference.
    #[error("LLM provider error: {0}")]
    Llm(#[from] zeph_llm::LlmError),

    /// Tool executor returned an error.
    #[error("tool execution error: {0}")]
    Tool(#[from] zeph_tools::ToolError),

    /// MCP server returned an error during tool dispatch.
    #[error("MCP error: {0}")]
    Mcp(String),

    /// The turn was cancelled by the user or a cancellation token.
    #[error("turn cancelled")]
    Cancelled,

    /// Context length exceeded even after compaction.
    #[error("context length exceeded after compaction")]
    ContextOverflow,

    /// An operation timed out.
    #[error("timeout: {0}")]
    Timeout(String),

    /// Channel sink returned an error while emitting events to the user surface.
    #[error("channel error: {0}")]
    Channel(#[from] crate::channel::ChannelSinkError),
}
