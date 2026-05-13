// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for A2A client, server, and discovery operations.

use crate::jsonrpc::JsonRpcError;

/// All errors that can occur in A2A client and server operations.
///
/// The variants map to distinct failure modes so callers can recover appropriately:
/// - Retry on [`Http`](A2aError::Http) (transient network issues).
/// - Inspect the code on [`JsonRpc`](A2aError::JsonRpc) — `-32001` is task-not-found,
///   `-32002` is not-cancelable.
/// - Abort on [`Security`](A2aError::Security) — endpoint rejected by TLS or SSRF policy.
#[derive(Debug, thiserror::Error)]
pub enum A2aError {
    /// A `reqwest` HTTP transport error (connection refused, timeout, TLS, etc.).
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization or deserialization failure.
    #[error("JSON serialization/deserialization failed: {0}")]
    Json(#[from] serde_json::Error),

    /// The remote agent returned a JSON-RPC error object.
    ///
    /// Well-known codes defined by the A2A spec:
    /// - `-32001`: task not found
    /// - `-32002`: task not in a cancelable state
    #[error("JSON-RPC error {code}: {message}")]
    JsonRpc { code: i32, message: String },

    /// `AgentRegistry` could not retrieve a valid [`AgentCard`](crate::types::AgentCard)
    /// from the remote agent's well-known URL.
    #[error("agent discovery failed for {url}: {reason}")]
    Discovery { url: String, reason: String },

    /// An error occurred while reading the SSE event stream from a streaming call.
    #[error("SSE stream error: {0}")]
    Stream(String),

    /// An internal server-side error (binding failure, task processing panic, etc.).
    #[error("server error: {0}")]
    Server(String),

    /// A request was rejected by the client's security policy.
    ///
    /// Triggered when [`A2aClient`](crate::A2aClient) is configured with
    /// `require_tls = true` and an `http://` endpoint is used, or when
    /// `ssrf_protection = true` and DNS resolves to a private/loopback address.
    #[error("security policy violation: {0}")]
    Security(String),

    /// A request or task processing operation exceeded its deadline.
    #[error("operation timed out after {0:?}")]
    Timeout(std::time::Duration),
}

impl From<JsonRpcError> for A2aError {
    fn from(e: JsonRpcError) -> Self {
        Self::JsonRpc {
            code: e.code,
            message: e.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_jsonrpc_error() {
        let rpc_err = JsonRpcError {
            code: -32001,
            message: "task not found".into(),
            data: None,
        };
        let err: A2aError = rpc_err.into();
        match err {
            A2aError::JsonRpc { code, message } => {
                assert_eq!(code, -32001);
                assert_eq!(message, "task not found");
            }
            _ => panic!("expected JsonRpc variant"),
        }
    }

    #[test]
    fn error_display() {
        let err = A2aError::Discovery {
            url: "http://example.com".into(),
            reason: "connection refused".into(),
        };
        assert_eq!(
            err.to_string(),
            "agent discovery failed for http://example.com: connection refused"
        );

        let err = A2aError::Stream("unexpected EOF".into());
        assert_eq!(err.to_string(), "SSE stream error: unexpected EOF");
    }

    #[test]
    fn security_error_display() {
        let err = A2aError::Security("TLS required but endpoint uses HTTP".into());
        assert_eq!(
            err.to_string(),
            "security policy violation: TLS required but endpoint uses HTTP"
        );
    }

    #[test]
    fn from_serde_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err: A2aError = json_err.into();
        assert!(matches!(err, A2aError::Json(_)));
    }
}
