// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::net::SocketAddr;
use std::time::Instant;

use tokio::sync::{mpsc, watch};

use crate::error::GatewayError;
use crate::router::build_router;

/// Shared state threaded through every axum handler.
///
/// Cloned cheaply for each request because all fields are either `Clone` or
/// wrapped in `Arc`-backed primitives.
#[derive(Clone)]
pub(crate) struct AppState {
    /// Channel used to forward sanitised webhook messages to the agent.
    pub webhook_tx: mpsc::Sender<String>,
    /// Monotonic timestamp recorded when the server started, used by `/health`.
    pub started_at: Instant,
}

/// HTTP gateway server with bearer-auth, rate limiting, and body-size enforcement.
///
/// Build the server with [`GatewayServer::new`], apply optional configuration via
/// the builder methods, then drive it with [`GatewayServer::serve`].
///
/// # Defaults
///
/// | Setting | Default |
/// |---|---|
/// | Bearer auth | disabled (open) |
/// | Rate limit | 120 requests / 60 s per IP |
/// | Max body size | 1 MiB (1 048 576 bytes) |
///
/// # Example
///
/// ```no_run
/// use tokio::sync::{mpsc, watch};
/// use zeph_gateway::GatewayServer;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let (tx, _rx) = mpsc::channel::<String>(64);
///     let (_stx, srx) = watch::channel(false);
///
///     GatewayServer::new("127.0.0.1", 9000, tx, srx)
///         .with_auth(Some("hunter2".into()))
///         .with_rate_limit(30)
///         .with_max_body_size(512 * 1024)
///         .serve()
///         .await?;
///
///     Ok(())
/// }
/// ```
pub struct GatewayServer {
    addr: SocketAddr,
    auth_token: Option<String>,
    rate_limit: u32,
    max_body_size: usize,
    webhook_tx: mpsc::Sender<String>,
    shutdown_rx: watch::Receiver<bool>,
    /// Prometheus metrics registry and endpoint path (feature-gated).
    #[cfg(feature = "prometheus")]
    metrics_registry: Option<(
        std::sync::Arc<prometheus_client::registry::Registry>,
        String,
    )>,
}

impl GatewayServer {
    /// Create a new gateway server.
    ///
    /// `bind` is parsed as an IP address string (e.g. `"127.0.0.1"` or `"0.0.0.0"`).
    /// If parsing fails, the server falls back to `127.0.0.1:<port>` and emits a warning.
    ///
    /// `webhook_tx` receives every valid, sanitised webhook message as a formatted
    /// `"[sender@channel] body"` string.
    ///
    /// `shutdown_rx` is a [`watch::Receiver<bool>`] that signals graceful shutdown
    /// when its value transitions to `true`.  Sending `true` causes the server to
    /// stop accepting new connections and drain in-flight requests.
    ///
    /// # Panics
    ///
    /// Does not panic. Invalid `bind` values fall back to `127.0.0.1` with a log warning.
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
            #[cfg(feature = "prometheus")]
            metrics_registry: None,
        }
    }

    /// Set the bearer token required on `POST /webhook` requests.
    ///
    /// When `token` is `Some`, every request to `/webhook` must carry an
    /// `Authorization: Bearer <token>` header.  The comparison is performed
    /// in constant time (BLAKE3 + `subtle::ConstantTimeEq`) to prevent
    /// timing-oracle attacks.
    ///
    /// When `token` is `None`, bearer authentication is disabled and a warning
    /// is logged at startup.
    ///
    /// # Example
    ///
    /// ```
    /// use tokio::sync::{mpsc, watch};
    /// use zeph_gateway::GatewayServer;
    ///
    /// let (tx, _rx) = mpsc::channel::<String>(1);
    /// let (_stx, srx) = watch::channel(false);
    ///
    /// let server = GatewayServer::new("127.0.0.1", 8080, tx, srx)
    ///     .with_auth(Some("super-secret".into()));
    /// ```
    #[must_use]
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.auth_token = token;
        self
    }

    /// Set the per-IP rate limit for `POST /webhook`.
    ///
    /// `limit` is the maximum number of requests allowed per remote IP in a
    /// 60-second fixed window.  Setting `limit` to `0` disables rate limiting.
    ///
    /// # Example
    ///
    /// ```
    /// use tokio::sync::{mpsc, watch};
    /// use zeph_gateway::GatewayServer;
    ///
    /// let (tx, _rx) = mpsc::channel::<String>(1);
    /// let (_stx, srx) = watch::channel(false);
    ///
    /// // Allow at most 30 webhook posts per minute per IP.
    /// let server = GatewayServer::new("127.0.0.1", 8080, tx, srx)
    ///     .with_rate_limit(30);
    /// ```
    #[must_use]
    pub fn with_rate_limit(mut self, limit: u32) -> Self {
        self.rate_limit = limit;
        self
    }

    /// Set the maximum allowed request body size in bytes.
    ///
    /// Requests whose body exceeds this size are rejected with `413 Content Too Large`
    /// before any handler is invoked. The default is 1 MiB (1 048 576 bytes).
    ///
    /// # Example
    ///
    /// ```
    /// use tokio::sync::{mpsc, watch};
    /// use zeph_gateway::GatewayServer;
    ///
    /// let (tx, _rx) = mpsc::channel::<String>(1);
    /// let (_stx, srx) = watch::channel(false);
    ///
    /// // Restrict bodies to 64 KiB.
    /// let server = GatewayServer::new("127.0.0.1", 8080, tx, srx)
    ///     .with_max_body_size(64 * 1024);
    /// ```
    #[must_use]
    pub fn with_max_body_size(mut self, size: usize) -> Self {
        self.max_body_size = size;
        self
    }

    /// Attach a Prometheus metrics registry to the gateway.
    ///
    /// When set, the server mounts an additional route at `path` that returns the registry
    /// contents encoded as `OpenMetrics` 1.0.0 text.  The endpoint is unauthenticated and
    /// bypasses rate limiting.
    ///
    /// Requires the `prometheus` feature.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # #[cfg(feature = "prometheus")]
    /// # {
    /// use std::sync::Arc;
    /// use prometheus_client::registry::Registry;
    /// use tokio::sync::{mpsc, watch};
    /// use zeph_gateway::GatewayServer;
    ///
    /// let (tx, _rx) = mpsc::channel::<String>(1);
    /// let (_stx, srx) = watch::channel(false);
    /// let registry = Arc::new(Registry::default());
    ///
    /// let server = GatewayServer::new("127.0.0.1", 8080, tx, srx)
    ///     .with_metrics_registry(registry, "/metrics");
    /// # }
    /// ```
    #[cfg(feature = "prometheus")]
    #[must_use]
    pub fn with_metrics_registry(
        mut self,
        registry: std::sync::Arc<prometheus_client::registry::Registry>,
        path: impl Into<String>,
    ) -> Self {
        self.metrics_registry = Some((registry, path.into()));
        self
    }

    /// Start the HTTP gateway server and block until shutdown is signalled.
    ///
    /// Binds a TCP listener on the configured address, installs middleware
    /// (body-size limit → auth → rate limiting), and serves requests until
    /// the [`watch::Receiver`] supplied to [`GatewayServer::new`] transitions
    /// to `true`.
    ///
    /// # Errors
    ///
    /// - [`GatewayError::Bind`] — the listener could not be bound (port in use,
    ///   permission denied, etc.).
    /// - [`GatewayError::Server`] — the server encountered a fatal I/O error
    ///   after binding.
    pub async fn serve(self) -> Result<(), GatewayError> {
        let state = AppState {
            webhook_tx: self.webhook_tx,
            started_at: Instant::now(),
        };

        if self.auth_token.is_none() {
            tracing::warn!(
                "gateway running without bearer auth — ensure firewall or upstream proxy enforces access control"
            );
        }

        let router = build_router(
            state,
            self.auth_token.as_deref(),
            self.rate_limit,
            self.max_body_size,
        );

        #[cfg(feature = "prometheus")]
        let router = if let Some((registry, path)) = self.metrics_registry {
            let metrics_route = axum::Router::new()
                .route(&path, axum::routing::get(crate::handlers::metrics_handler))
                .with_state(registry);
            router.merge(metrics_route)
        } else {
            router
        };

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

    #[cfg(feature = "prometheus")]
    #[tokio::test]
    async fn test_metrics_endpoint_returns_openmetrics() {
        use axum::body::Body;
        use http_body_util::BodyExt;
        use prometheus_client::registry::Registry;
        use tower::ServiceExt;

        let registry = std::sync::Arc::new(Registry::default());

        let (tx, _rx) = mpsc::channel(1);
        let (_stx, srx) = watch::channel(false);
        let server = GatewayServer::new("127.0.0.1", 19999, tx, srx)
            .with_metrics_registry(std::sync::Arc::clone(&registry), "/metrics");

        // Build the router directly without binding a port
        let state = AppState {
            webhook_tx: server.webhook_tx,
            started_at: Instant::now(),
        };
        let router = crate::router::build_router(
            state,
            server.auth_token.as_deref(),
            server.rate_limit,
            server.max_body_size,
        );
        let metrics_route = axum::Router::new()
            .route(
                "/metrics",
                axum::routing::get(crate::handlers::metrics_handler),
            )
            .with_state(registry);
        let router = router.merge(metrics_route);

        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(req).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/openmetrics-text"),
            "unexpected content-type: {ct}"
        );

        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body_bytes.to_vec()).unwrap();
        assert!(body.ends_with("# EOF\n"), "missing EOF marker in:\n{body}");
    }

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
