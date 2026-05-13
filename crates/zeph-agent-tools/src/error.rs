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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::ChannelSinkError;

    #[test]
    fn cancelled_display() {
        assert_eq!(ToolDispatchError::Cancelled.to_string(), "turn cancelled");
    }

    #[test]
    fn context_overflow_display() {
        assert_eq!(
            ToolDispatchError::ContextOverflow.to_string(),
            "context length exceeded after compaction"
        );
    }

    #[test]
    fn timeout_display() {
        let err = ToolDispatchError::Timeout("30s".into());
        assert_eq!(err.to_string(), "timeout: 30s");
    }

    #[test]
    fn mcp_display() {
        let err = ToolDispatchError::Mcp("server down".into());
        assert_eq!(err.to_string(), "MCP error: server down");
    }

    #[test]
    fn channel_variant_display() {
        let sink_err = ChannelSinkError::new("broken pipe");
        let err = ToolDispatchError::Channel(sink_err);
        assert!(err.to_string().contains("channel error:"));
        assert!(err.to_string().contains("broken pipe"));
    }

    #[test]
    fn from_channel_sink_error() {
        let sink_err = ChannelSinkError::new("test");
        let err: ToolDispatchError = sink_err.into();
        assert!(matches!(err, ToolDispatchError::Channel(_)));
    }

    #[test]
    fn from_llm_error() {
        let llm_err = zeph_llm::LlmError::RateLimited;
        let err: ToolDispatchError = llm_err.into();
        assert!(matches!(err, ToolDispatchError::Llm(_)));
    }

    #[test]
    fn from_tool_error() {
        let tool_err = zeph_tools::ToolError::Cancelled;
        let err: ToolDispatchError = tool_err.into();
        assert!(matches!(err, ToolDispatchError::Tool(_)));
    }
}
