mod handlers;
mod router;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::watch;

use crate::error::A2aError;
use crate::types::AgentCard;
use router::build_router_with_full_config;
pub use state::{AppState, ProcessorEvent, TaskManager, TaskProcessor};

pub struct A2aServer {
    state: AppState,
    addr: SocketAddr,
    shutdown_rx: watch::Receiver<bool>,
    auth_token: Option<String>,
    rate_limit: u32,
    max_body_size: usize,
}

impl A2aServer {
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
        };

        Self {
            state,
            addr,
            shutdown_rx,
            auth_token: None,
            rate_limit: 0,
            max_body_size: 1_048_576,
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

    /// Start the HTTP server. Returns when the shutdown signal is received.
    ///
    /// # Errors
    ///
    /// Returns an error if the server fails to bind or encounters a fatal I/O error.
    pub async fn serve(self) -> Result<(), A2aError> {
        let router = build_router_with_full_config(
            self.state,
            self.auth_token,
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
        }
    }

    pub fn failing_state() -> AppState {
        AppState {
            card: test_card(),
            task_manager: TaskManager::new(),
            processor: Arc::new(FailingProcessor),
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
            .uri("/.well-known/agent-card.json")
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
