// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A2A HTTP server — serves `/.well-known/agent.json`, `POST /a2a`, and `POST /a2a/stream`.
//!
//! The server accepts JSON-RPC 2.0 requests on `/a2a` and SSE streaming requests on
//! `/a2a/stream`. It delegates task processing to a user-supplied [`TaskProcessor`]
//! implementation and manages task lifecycle via [`TaskManager`].
//!
//! # Architecture
//!
//! ```text
//! A2aServer
//!   └─ axum Router
//!        ├─ GET  /.well-known/agent.json  →  agent_card_handler
//!        ├─ POST /a2a                     →  jsonrpc_handler  (+ auth + rate-limit)
//!        └─ POST /a2a/stream              →  stream_handler   (+ auth + rate-limit)
//! ```
//!
//! Each request goes through:
//! 1. Body size limit (`max_body_size`, default 1 MiB).
//! 2. Bearer token authentication (constant-time comparison via blake3).
//! 3. Per-IP rate limiting with sliding window and eviction.
//! 4. Handler dispatches to the appropriate operation.
//!
//! # Feature flag
//!
//! This module is only compiled when the `server` feature is enabled.

mod handlers;
mod router;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::error::A2aError;
use crate::types::AgentCard;
use router::build_router_with_full_config;
pub use state::{AppState, ProcessorEvent, TaskManager, TaskProcessor};

/// An A2A protocol HTTP server.
///
/// `A2aServer` wraps an axum router and binds to a TCP port. It serves:
/// - `GET /.well-known/agent.json` — agent capability card (unauthenticated).
/// - `POST /a2a` — JSON-RPC 2.0 endpoint for blocking task operations.
/// - `POST /a2a/stream` — SSE endpoint for real-time streaming task execution.
///
/// # Lifecycle
///
/// The server runs until a `true` is sent on `shutdown_rx`. Pass a `watch::channel`
/// so that the owning component (e.g., `zeph-core`) can stop the server cleanly.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_a2a::{A2aServer, TaskProcessor, ProcessorEvent, A2aError, AgentCardBuilder, Message};
/// use std::sync::Arc;
/// use tokio::sync::watch;
/// use std::pin::Pin;
///
/// struct MyProcessor;
///
/// impl TaskProcessor for MyProcessor {
///     fn process(
///         &self,
///         task_id: String,
///         message: Message,
///         event_tx: tokio::sync::mpsc::Sender<ProcessorEvent>,
///     ) -> Pin<Box<dyn std::future::Future<Output = Result<(), A2aError>> + Send>> {
///         Box::pin(async move {
///             let _ = event_tx.send(ProcessorEvent::StatusUpdate {
///                 state: zeph_a2a::TaskState::Completed,
///                 is_final: true,
///             }).await;
///             Ok(())
///         })
///     }
/// }
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let card = AgentCardBuilder::new("my-agent", "http://localhost:9090", "0.1.0").build();
/// let (_shutdown_tx, shutdown_rx) = watch::channel(false);
///
/// A2aServer::new(card, Arc::new(MyProcessor), "0.0.0.0", 9090, shutdown_rx)
///     .with_auth(Some("my-secret-token".into()))
///     .serve()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[cfg_attr(docsrs, doc(cfg(feature = "server")))]
pub struct A2aServer {
    state: AppState,
    addr: SocketAddr,
    shutdown_rx: watch::Receiver<bool>,
    auth_token: Option<String>,
    require_auth: bool,
    rate_limit: u32,
    max_body_size: usize,
}

impl A2aServer {
    /// Create a new `A2aServer` with default security settings.
    ///
    /// - Auth: disabled (all requests accepted).
    /// - Rate limit: disabled (`0` = unlimited).
    /// - Max body size: 1 MiB.
    ///
    /// If `host` cannot be parsed as a valid address, the server falls back to
    /// `0.0.0.0:{port}` and emits a `WARN` log.
    #[must_use]
    pub fn new(
        card: AgentCard,
        processor: Arc<dyn TaskProcessor>,
        host: &str,
        port: u16,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let addr: SocketAddr = format!("{host}:{port}").parse().unwrap_or_else(|e| {
            tracing::warn!("invalid host '{host}': {e}, falling back to 0.0.0.0:{port}");
            SocketAddr::from(([0, 0, 0, 0], port))
        });

        let state = AppState {
            card,
            task_manager: TaskManager::new(),
            processor,
            request_timeout: Duration::from_mins(5),
        };

        Self {
            state,
            addr,
            shutdown_rx,
            auth_token: None,
            require_auth: false,
            rate_limit: 0,
            max_body_size: 1_048_576,
        }
    }

    /// Set the bearer token used for request authentication.
    ///
    /// When `Some(token)` is provided, every request to `/a2a` and `/a2a/stream` must
    /// include an `Authorization: Bearer <token>` header. The comparison is constant-time
    /// (blake3 hash of both sides) to prevent timing attacks.
    ///
    /// Passing `None` disables bearer auth. A `WARN` log is emitted if no token is set,
    /// as a reminder that the server is open to unauthenticated requests.
    #[must_use]
    pub fn with_auth(mut self, token: Option<String>) -> Self {
        self.auth_token = token;
        self
    }

    /// When `true`, the server rejects all requests if no auth token is configured.
    ///
    /// This is a safety guard: if `require_auth = true` but no token is provided via
    /// [`with_auth`](Self::with_auth), every request returns `401 Unauthorized`.
    #[must_use]
    pub fn with_require_auth(mut self, require: bool) -> Self {
        self.require_auth = require;
        self
    }

    /// Set the per-IP request rate limit (requests per 60-second sliding window).
    ///
    /// `0` disables rate limiting entirely. When the limit is exceeded, the server
    /// returns `429 Too Many Requests`. The rate limiter tracks up to 10,000 unique IPs;
    /// beyond that, stale entries are evicted and new IPs may be rejected.
    #[must_use]
    pub fn with_rate_limit(mut self, limit: u32) -> Self {
        self.rate_limit = limit;
        self
    }

    /// Set the maximum allowed request body size in bytes (default: 1 MiB).
    ///
    /// Requests exceeding this limit are rejected with `413 Payload Too Large` before
    /// reaching the handler, protecting against memory exhaustion from large payloads.
    #[must_use]
    pub fn with_max_body_size(mut self, size: usize) -> Self {
        self.max_body_size = size;
        self
    }

    /// Set the per-request timeout for task processing (default: 300 seconds).
    ///
    /// If a [`TaskProcessor`] does not complete within this duration, the server
    /// aborts the spawned future, marks the task as [`crate::TaskState::Failed`], and returns
    /// a JSON-RPC internal-error response to the caller.
    ///
    /// For streaming calls, a final SSE event with `failed` state is sent before the
    /// connection closes, so the client always receives a terminal event.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.state.request_timeout = timeout;
        self
    }

    /// Bind to the configured address and start serving A2A requests.
    ///
    /// Runs until a `true` value is received on the `shutdown_rx` channel provided to
    /// [`new`](Self::new). Shutdown is graceful: in-flight requests are allowed to complete.
    ///
    /// # Errors
    ///
    /// Returns [`A2aError::Server`] if the TCP listener fails to
    /// bind or if the axum server encounters a fatal I/O error during operation.
    pub async fn serve(self) -> Result<(), A2aError> {
        if self.auth_token.is_none() {
            tracing::warn!(
                "A2A server running without bearer auth — ensure this is a trusted-network-only deployment"
            );
        }

        let router = build_router_with_full_config(
            self.state,
            self.auth_token,
            self.require_auth,
            self.rate_limit,
            self.max_body_size,
        );

        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .map_err(|e| A2aError::Server(format!("failed to bind {}: {e}", self.addr)))?;
        tracing::info!("A2A server listening on {}", self.addr);

        let mut shutdown_rx = self.shutdown_rx;
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                while !*shutdown_rx.borrow_and_update() {
                    if shutdown_rx.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
                tracing::info!("A2A server shutting down");
            })
            .await
            .map_err(|e| A2aError::Server(format!("server error: {e}")))?;

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use crate::error::A2aError;
    use crate::types::{AgentCapabilities, AgentCard, Message};

    use super::state::{AppState, ProcessorEvent, TaskManager, TaskProcessor};

    pub struct EchoProcessor;

    impl TaskProcessor for EchoProcessor {
        fn process(
            &self,
            _task_id: String,
            message: Message,
            event_tx: tokio::sync::mpsc::Sender<ProcessorEvent>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), A2aError>> + Send>>
        {
            Box::pin(async move {
                let text = message.text_content().unwrap_or("").to_owned();
                let _ = event_tx
                    .send(ProcessorEvent::ArtifactChunk {
                        text: format!("echo: {text}"),
                        is_final: true,
                    })
                    .await;
                let _ = event_tx
                    .send(ProcessorEvent::StatusUpdate {
                        state: crate::types::TaskState::Completed,
                        is_final: true,
                    })
                    .await;
                Ok(())
            })
        }
    }

    pub struct FailingProcessor;

    impl TaskProcessor for FailingProcessor {
        fn process(
            &self,
            _task_id: String,
            _message: Message,
            _event_tx: tokio::sync::mpsc::Sender<ProcessorEvent>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), A2aError>> + Send>>
        {
            Box::pin(async { Err(A2aError::Server("boom".into())) })
        }
    }

    pub fn test_card() -> AgentCard {
        AgentCard {
            name: "test-agent".into(),
            description: "test".into(),
            url: "http://localhost:8080".into(),
            version: "0.1.0".into(),
            protocol_version: crate::A2A_PROTOCOL_VERSION.to_owned(),
            provider: None,
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![],
        }
    }

    pub fn test_state() -> AppState {
        AppState {
            card: test_card(),
            task_manager: TaskManager::new(),
            processor: Arc::new(EchoProcessor),
            request_timeout: std::time::Duration::from_mins(5),
        }
    }

    pub fn failing_state() -> AppState {
        AppState {
            card: test_card(),
            task_manager: TaskManager::new(),
            processor: Arc::new(FailingProcessor),
            request_timeout: std::time::Duration::from_mins(5),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::testing::{failing_state, test_state};
    use super::*;

    #[tokio::test]
    async fn agent_card_endpoint() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let req = axum::http::Request::builder()
            .uri("/.well-known/agent.json")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let card: AgentCard = serde_json::from_slice(&body).unwrap();
        assert_eq!(card.name, "test-agent");
    }

    #[tokio::test]
    async fn send_message_success() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}]
                }
            }
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert!(rpc["result"].is_object());
        assert_eq!(rpc["result"]["status"]["state"], "completed");
        assert!(!rpc["result"]["history"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_task_not_found() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "tasks/get",
            "params": {"id": "nonexistent"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(rpc["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn unknown_method() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "3",
            "method": "unknown/method",
            "params": {}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(rpc["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn cancel_nonexistent_task() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "4",
            "method": "tasks/cancel",
            "params": {"id": "nope"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(rpc["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn send_message_processor_failure_sets_failed() {
        let app = router::build_router_with_config(failing_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}]
                }
            }
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(rpc["result"]["status"]["state"], "failed");
    }

    #[tokio::test]
    async fn send_message_invalid_params() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {"wrong_field": true}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(rpc["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn get_task_invalid_params() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "tasks/get",
            "params": {"not_an_id": 123}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(rpc["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn cancel_task_invalid_params() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "tasks/cancel",
            "params": {}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(rpc["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn streaming_method_via_jsonrpc_returns_method_not_found() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/stream",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}]
                }
            }
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(rpc["error"]["code"], -32601);
        let msg = rpc["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("stream"),
            "error message should mention streaming"
        );
    }

    #[tokio::test]
    async fn send_then_get_with_history_length() {
        use tower::Service;

        let state = test_state();
        let mut app = router::build_router_with_config(state, None, 0);

        // Send a message
        let send_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}]
                }
            }
        });
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&send_body).unwrap()))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let task_id = rpc["result"]["id"].as_str().unwrap().to_owned();

        // Get task with historyLength=1 — should return only the last message
        let get_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "tasks/get",
            "params": {"id": task_id, "historyLength": 1}
        });
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&get_body).unwrap()))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let history = rpc["result"]["history"].as_array().unwrap();
        assert_eq!(history.len(), 1);
    }

    #[tokio::test]
    async fn cancel_completed_task_returns_not_cancelable() {
        use tower::Service;

        let state = test_state();
        let mut app = router::build_router_with_config(state, None, 0);

        // Create a task via send
        let send_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}]
                }
            }
        });
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&send_body).unwrap()))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let task_id = rpc["result"]["id"].as_str().unwrap().to_owned();

        // Task is already completed — cancel should fail with -32002
        let cancel_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "2",
            "method": "tasks/cancel",
            "params": {"id": task_id}
        });
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&cancel_body).unwrap()))
            .unwrap();
        let resp = app.call(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(rpc["error"]["code"], -32002);
    }

    #[tokio::test]
    async fn sse_stream_success() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}]
                }
            }
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a/stream")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("text/event-stream"),
            "expected SSE content-type, got: {content_type}"
        );

        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8_lossy(&body_bytes);
        assert!(
            body_str.contains("working"),
            "should contain working status event"
        );
        assert!(
            body_str.contains("completed"),
            "should contain completed status event"
        );
    }

    #[tokio::test]
    async fn sse_stream_processor_failure() {
        let app = router::build_router_with_config(failing_state(), None, 0);

        let body = serde_json::json!({
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{"kind": "text", "text": "hello"}]
                }
            }
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a/stream")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8_lossy(&body_bytes);
        assert!(
            body_str.contains("failed"),
            "should contain failed status event"
        );
    }

    #[tokio::test]
    async fn sse_stream_missing_message_sends_error() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({"params": {}});

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a/stream")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8_lossy(&body_bytes);
        assert!(
            body_str.contains("missing message param"),
            "should contain error about missing message"
        );
    }

    #[tokio::test]
    async fn jsonrpc_response_format_correctness() {
        let app = router::build_router_with_config(test_state(), None, 0);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "test-id-42",
            "method": "tasks/get",
            "params": {"id": "nonexistent"}
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/a2a")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();

        assert_eq!(rpc["jsonrpc"], "2.0", "must always include jsonrpc version");
        assert_eq!(rpc["id"], "test-id-42", "must echo back the request id");
        assert!(
            rpc["result"].is_null(),
            "error response must not have result"
        );
        assert!(
            rpc["error"].is_object(),
            "error response must have error object"
        );
        assert!(
            rpc["error"]["code"].is_number(),
            "error must have numeric code"
        );
        assert!(
            rpc["error"]["message"].is_string(),
            "error must have string message"
        );
    }
}
