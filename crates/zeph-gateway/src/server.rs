use std::net::SocketAddr;
use std::time::Instant;

use tokio::sync::{mpsc, watch};

use crate::error::GatewayError;
use crate::router::build_router;

#[derive(Clone)]
pub(crate) struct AppState {
    pub webhook_tx: mpsc::Sender<String>,
    pub started_at: Instant,
}

pub struct GatewayServer {
    addr: SocketAddr,
    auth_token: Option<String>,
    rate_limit: u32,
    max_body_size: usize,
    webhook_tx: mpsc::Sender<String>,
    shutdown_rx: watch::Receiver<bool>,
}

impl GatewayServer {
    #[must_use]
    pub fn new(
        bind: &str,
        port: u16,
        webhook_tx: mpsc::Sender<String>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let addr: SocketAddr = format!("{bind}:{port}").parse().unwrap_or_else(|e| {
            tracing::warn!("invalid bind '{bind}': {e}, falling back to 127.0.0.1:{port}");
            SocketAddr::from(([127, 0, 0, 1], port))
        });

        if bind == "0.0.0.0" {
            tracing::warn!("gateway binding to 0.0.0.0 — ensure this is intended for production");
        }

        Self {
            addr,
            auth_token: None,
            rate_limit: 120,
            max_body_size: 1_048_576,
            webhook_tx,
            shutdown_rx,
        }
    }

    #[must_use]
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.auth_token = token;
        self
    }

    #[must_use]
    pub fn with_rate_limit(mut self, limit: u32) -> Self {
        self.rate_limit = limit;
        self
    }

    #[must_use]
    pub fn with_max_body_size(mut self, size: usize) -> Self {
        self.max_body_size = size;
        self
    }

    /// Start the HTTP gateway server.
    ///
    /// # Errors
    ///
    /// Returns an error if the server fails to bind or encounters a fatal I/O error.
    pub async fn serve(self) -> Result<(), GatewayError> {
        let state = AppState {
            webhook_tx: self.webhook_tx,
            started_at: Instant::now(),
        };

        let router = build_router(state, self.auth_token, self.rate_limit, self.max_body_size);

        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .map_err(|e| GatewayError::Bind(self.addr.to_string(), e))?;
        tracing::info!("gateway listening on {}", self.addr);

        let mut shutdown_rx = self.shutdown_rx;
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            while !*shutdown_rx.borrow_and_update() {
                if shutdown_rx.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
            tracing::info!("gateway shutting down");
        })
        .await
        .map_err(|e| GatewayError::Server(format!("{e}")))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_builder_chain() {
        let (tx, _rx) = mpsc::channel(1);
        let (_stx, srx) = watch::channel(false);
        let server = GatewayServer::new("127.0.0.1", 8090, tx, srx)
            .with_auth(Some("token".into()))
            .with_rate_limit(60)
            .with_max_body_size(512);

        assert_eq!(server.rate_limit, 60);
        assert_eq!(server.max_body_size, 512);
        assert!(server.auth_token.is_some());
    }

    #[test]
    fn server_invalid_bind_fallback() {
        let (tx, _rx) = mpsc::channel(1);
        let (_stx, srx) = watch::channel(false);
        let server = GatewayServer::new("not_an_ip", 9999, tx, srx);
        assert_eq!(server.addr.port(), 9999);
    }
}
