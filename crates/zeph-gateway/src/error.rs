// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use thiserror::Error;

/// Errors that can be returned by the HTTP gateway.
///
/// All variants implement [`std::error::Error`] via [`thiserror`].
#[derive(Debug, Error)]
pub enum GatewayError {
    /// The server could not bind to the requested address.
    ///
    /// The first field is the address string (e.g. `"127.0.0.1:8080"`) and the
    /// second is the underlying I/O error.
    #[error("failed to bind {0}: {1}")]
    Bind(String, std::io::Error),

    /// The `axum` server returned a fatal error after binding succeeded.
    ///
    /// This typically indicates a listener failure or an OS-level socket error.
    #[error("server error: {0}")]
    Server(String),
}
