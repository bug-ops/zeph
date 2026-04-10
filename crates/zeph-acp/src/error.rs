// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

/// Errors produced by the ACP server and its subsystems.
///
/// Each variant corresponds to a distinct failure domain so callers can handle
/// or propagate them appropriately.
///
/// # Examples
///
/// ```
/// use zeph_acp::AcpError;
///
/// let err = AcpError::Transport("connection reset".to_owned());
/// assert!(err.to_string().contains("transport error"));
///
/// let err = AcpError::TerminalTimeout { output: "partial".to_owned() };
/// assert!(err.to_string().contains("timed out"));
/// ```
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// The underlying JSON-RPC transport (stdio, HTTP, WebSocket) encountered an I/O error.
    #[error("transport error: {0}")]
    Transport(String),

    /// The connected IDE returned a protocol-level error response.
    #[error("IDE returned error: {0}")]
    ClientError(String),

    /// The IDE did not advertise the required ACP capability.
    #[error("capability not available: {0}")]
    CapabilityUnavailable(String),

    /// The internal async channel between the agent loop and the ACP handler was dropped.
    ///
    /// This typically means the session has already terminated.
    #[error("channel closed")]
    ChannelClosed,

    /// A terminal command did not complete within the configured timeout.
    ///
    /// `output` contains whatever the terminal produced before the timeout.
    #[error("terminal command timed out; partial output: {output}")]
    TerminalTimeout { output: String },

    /// The stdin data payload exceeds the 64 KiB limit.
    #[error("stdin payload too large: {size} bytes (max 65536)")]
    StdinTooLarge { size: usize },

    /// The terminal's stdin channel was closed by the IDE before the write completed.
    #[error("broken pipe: terminal stdin closed")]
    BrokenPipe,

    /// A `ResourceLink` URI could not be resolved (bad scheme, path traversal, SSRF, etc.).
    #[error("resource link error: {0}")]
    ResourceLink(String),
}
