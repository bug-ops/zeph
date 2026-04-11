// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_common::ToolName;

/// Typed error code for MCP tool call retry and recovery classification.
///
/// Used by [`McpError::code`] and callers such as the agent retry loop to decide
/// whether an operation should be retried, backed off, or abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpErrorCode {
    /// Transient error: retry is likely to succeed.
    Transient,
    /// Rate limited: back off and retry.
    RateLimited,
    /// Invalid input: do not retry without changing parameters.
    InvalidInput,
    /// Auth failure: re-authenticate or escalate.
    AuthFailure,
    /// Server error: may be transient, retry with backoff.
    ServerError,
    /// Not found: resource or tool does not exist.
    NotFound,
    /// Blocked by policy rules.
    PolicyBlocked,
}

impl McpErrorCode {
    /// Whether this error code suggests the operation can be retried.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Transient | Self::RateLimited | Self::ServerError
        )
    }
}

/// Crate-wide error type for all MCP operations.
///
/// Variants cover connection failures, tool call errors, policy blocks, OAuth flows,
/// and infrastructure errors (Qdrant, JSON serialization). Use [`McpError::code`] to
/// obtain a typed [`McpErrorCode`] for retry/recovery decisions.
///
/// # Examples
///
/// ```
/// use zeph_mcp::error::{McpError, McpErrorCode};
///
/// let err = McpError::Timeout {
///     server_id: "github".to_owned(),
///     tool_name: "create_issue".into(),
///     timeout_secs: 30,
/// };
/// assert_eq!(err.code(), Some(McpErrorCode::Transient));
/// assert!(err.code().unwrap().is_retryable());
/// ```
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("connection failed for server '{server_id}': {message}")]
    Connection { server_id: String, message: String },

    #[error("tool call failed: {server_id}/{tool_name}: {message}")]
    ToolCall {
        server_id: String,
        tool_name: ToolName,
        message: String,
        /// Typed error code for retry classification.
        code: McpErrorCode,
    },

    #[error("server '{server_id}' not found")]
    ServerNotFound { server_id: String },

    #[error("server '{server_id}' is already connected")]
    ServerAlreadyConnected { server_id: String },

    #[error("tool '{tool_name}' not found on server '{server_id}'")]
    ToolNotFound {
        server_id: String,
        tool_name: ToolName,
    },

    #[error("tool call timed out after {timeout_secs}s: {server_id}/{tool_name}")]
    Timeout {
        server_id: String,
        tool_name: ToolName,
        timeout_secs: u64,
    },

    #[error("Qdrant error: {0}")]
    Qdrant(#[from] Box<qdrant_client::QdrantError>),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("integer conversion: {0}")]
    IntConversion(#[from] std::num::TryFromIntError),

    #[error("SSRF blocked: URL '{url}' resolves to private/reserved IP {addr}")]
    SsrfBlocked { url: String, addr: String },

    #[error("invalid URL '{url}': {message}")]
    InvalidUrl { url: String, message: String },

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("MCP command '{command}' not allowed")]
    CommandNotAllowed { command: String },

    #[error("env var '{var_name}' is blocked for MCP server processes")]
    EnvVarBlocked { var_name: String },

    #[error("policy violation: {0}")]
    PolicyViolation(String),

    #[error("OAuth error for server '{server_id}': {message}")]
    OAuthError { server_id: String, message: String },

    #[error("OAuth callback timed out for server '{server_id}' after {timeout_secs}s")]
    OAuthCallbackTimeout {
        server_id: String,
        timeout_secs: u64,
    },

    #[error("tool list refresh rejected for '{server_id}': list is locked after initial connect")]
    ToolListLocked { server_id: String },
}

impl McpError {
    /// Return the typed error code for this error variant.
    #[must_use]
    pub fn code(&self) -> Option<McpErrorCode> {
        match self {
            Self::ToolCall { code, .. } => Some(*code),
            Self::Timeout { .. } | Self::Connection { .. } => Some(McpErrorCode::Transient),
            Self::ServerNotFound { .. } | Self::ToolNotFound { .. } => Some(McpErrorCode::NotFound),
            Self::PolicyViolation(_)
            | Self::SsrfBlocked { .. }
            | Self::CommandNotAllowed { .. }
            | Self::EnvVarBlocked { .. } => Some(McpErrorCode::PolicyBlocked),
            Self::OAuthError { .. } | Self::OAuthCallbackTimeout { .. } => {
                Some(McpErrorCode::AuthFailure)
            }
            Self::InvalidUrl { .. } | Self::ToolListLocked { .. } => {
                Some(McpErrorCode::InvalidInput)
            }
            Self::Embedding(_) => Some(McpErrorCode::ServerError),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_error_display() {
        let err = McpError::Connection {
            server_id: "github".into(),
            message: "refused".into(),
        };
        assert_eq!(
            err.to_string(),
            "connection failed for server 'github': refused"
        );
    }

    #[test]
    fn tool_call_error_display() {
        let err = McpError::ToolCall {
            server_id: "fs".into(),
            tool_name: "read_file".into(),
            message: "not found".into(),
            code: McpErrorCode::ServerError,
        };
        assert_eq!(err.to_string(), "tool call failed: fs/read_file: not found");
    }

    #[test]
    fn error_code_is_retryable() {
        assert!(McpErrorCode::Transient.is_retryable());
        assert!(McpErrorCode::RateLimited.is_retryable());
        assert!(McpErrorCode::ServerError.is_retryable());
        assert!(!McpErrorCode::InvalidInput.is_retryable());
        assert!(!McpErrorCode::AuthFailure.is_retryable());
        assert!(!McpErrorCode::NotFound.is_retryable());
        assert!(!McpErrorCode::PolicyBlocked.is_retryable());
    }

    #[test]
    fn mcp_error_code_method() {
        let err = McpError::ToolCall {
            server_id: "s".into(),
            tool_name: "t".into(),
            message: "e".into(),
            code: McpErrorCode::RateLimited,
        };
        assert_eq!(err.code(), Some(McpErrorCode::RateLimited));

        let timeout = McpError::Timeout {
            server_id: "s".into(),
            tool_name: "t".into(),
            timeout_secs: 30,
        };
        assert_eq!(timeout.code(), Some(McpErrorCode::Transient));

        let policy = McpError::PolicyViolation("denied".into());
        assert_eq!(policy.code(), Some(McpErrorCode::PolicyBlocked));
    }

    #[test]
    fn server_not_found_display() {
        let err = McpError::ServerNotFound {
            server_id: "missing".into(),
        };
        assert_eq!(err.to_string(), "server 'missing' not found");
    }

    #[test]
    fn tool_not_found_display() {
        let err = McpError::ToolNotFound {
            server_id: "fs".into(),
            tool_name: "delete".into(),
        };
        assert_eq!(err.to_string(), "tool 'delete' not found on server 'fs'");
    }

    #[test]
    fn server_already_connected_display() {
        let err = McpError::ServerAlreadyConnected {
            server_id: "github".into(),
        };
        assert_eq!(err.to_string(), "server 'github' is already connected");
    }

    #[test]
    fn timeout_error_display() {
        let err = McpError::Timeout {
            server_id: "slow".into(),
            tool_name: "query".into(),
            timeout_secs: 30,
        };
        assert_eq!(err.to_string(), "tool call timed out after 30s: slow/query");
    }

    #[test]
    fn handshake_timeout_has_initialize_tool_name() {
        let err = McpError::Timeout {
            server_id: "my-server".into(),
            tool_name: "initialize".into(),
            timeout_secs: 10,
        };
        assert_eq!(
            err.to_string(),
            "tool call timed out after 10s: my-server/initialize"
        );
        assert_eq!(err.code(), Some(McpErrorCode::Transient));
    }

    #[test]
    fn list_tools_timeout_has_tools_list_tool_name() {
        let err = McpError::Timeout {
            server_id: "my-server".into(),
            tool_name: "tools/list".into(),
            timeout_secs: 30,
        };
        assert_eq!(
            err.to_string(),
            "tool call timed out after 30s: my-server/tools/list"
        );
        assert_eq!(err.code(), Some(McpErrorCode::Transient));
    }
}
