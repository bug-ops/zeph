// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use thiserror::Error;

/// Errors that can be returned by the HTTP gateway.
///
/// All variants implement [`std::error::Error`] via [`thiserror`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GatewayError {
    /// The server could not bind to the requested address.
    ///
    /// The first field is the address string (e.g. `"127.0.0.1:8080"`) and the
    /// second is the underlying I/O error.
    #[error("failed to bind {0}: {1}")]
    Bind(String, #[source] std::io::Error),

    /// The `axum` server returned a fatal error after binding succeeded.
    ///
    /// This typically indicates a listener failure or an OS-level socket error.
    #[error("server error: {0}")]
    Server(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_error_exposes_io_error_as_source() {
        let io_err = std::io::Error::new(std::io::ErrorKind::AddrInUse, "address in use");
        let err = GatewayError::Bind("127.0.0.1:8080".to_string(), io_err);

        let source = std::error::Error::source(&err).expect("Bind must expose a source");
        let downcast = source
            .downcast_ref::<std::io::Error>()
            .expect("source must downcast to std::io::Error");
        assert_eq!(downcast.kind(), std::io::ErrorKind::AddrInUse);

        assert_eq!(
            err.to_string(),
            "failed to bind 127.0.0.1:8080: address in use"
        );
    }

    #[test]
    fn server_error_exposes_io_error_as_source() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let err = GatewayError::Server(io_err);

        let source = std::error::Error::source(&err).expect("Server must expose a source");
        let downcast = source
            .downcast_ref::<std::io::Error>()
            .expect("source must downcast to std::io::Error");
        assert_eq!(downcast.kind(), std::io::ErrorKind::BrokenPipe);

        assert_eq!(err.to_string(), "server error: broken pipe");
    }
}
