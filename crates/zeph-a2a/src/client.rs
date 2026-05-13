// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A2A protocol HTTP client with optional TLS enforcement and SSRF protection.

use std::pin::Pin;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_core::Stream;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio_stream::StreamExt;
use zeph_common::net::is_private_ip;

use crate::error::A2aError;
use crate::jsonrpc::{
    JsonRpcRequest, JsonRpcResponse, METHOD_CANCEL_TASK, METHOD_GET_TASK, METHOD_SEND_MESSAGE,
    METHOD_SEND_STREAMING_MESSAGE, SendMessageParams, TaskIdParams,
};
use crate::types::{Task, TaskArtifactUpdateEvent, TaskStatusUpdateEvent};

/// A pinned, heap-allocated stream of [`TaskEvent`]s from a streaming A2A call.
///
/// Produced by [`A2aClient::stream_message`]. Each item is either a status update
/// or an artifact update; errors are surfaced inline as `Err(A2aError)`.
pub type TaskEventStream = Pin<Box<dyn Stream<Item = Result<TaskEvent, A2aError>> + Send>>;

/// A single event received on a streaming (`message/stream`) A2A connection.
///
/// The A2A spec multiplexes two event kinds over the same SSE channel. This enum
/// uses `#[serde(untagged)]` so that the deserializer inspects the `kind` field
/// inside the inner struct to determine the variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TaskEvent {
    /// A task lifecycle transition (e.g., `submitted` → `working` → `completed`).
    StatusUpdate(TaskStatusUpdateEvent),
    /// A new or updated output artifact from the agent.
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

/// HTTP client for the A2A protocol.
///
/// `A2aClient` wraps a `reqwest::Client` and provides typed methods for the four
/// A2A JSON-RPC operations: `message/send`, `message/stream`, `tasks/get`, and
/// `tasks/cancel`. Each call optionally accepts a bearer token for authentication.
///
/// # Security
///
/// Use [`with_security`](A2aClient::with_security) to harden the client for
/// production deployments:
/// - `require_tls = true` rejects any `http://` endpoint before connecting.
/// - `ssrf_protection = true` resolves the endpoint's hostname via DNS and rejects
///   addresses in private/loopback ranges (10/8, 172.16/12, 192.168/16, 127/8, etc.).
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_a2a::{A2aClient, SendMessageParams, Message};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = A2aClient::new(reqwest::Client::new())
///     .with_security(true, true); // require HTTPS, block SSRF
///
/// let params = SendMessageParams {
///     message: Message::user_text("Summarize this page."),
///     configuration: None,
/// };
/// let task = client.send_message("https://agent.example.com/a2a", params, Some("tok")).await?;
/// println!("Task state: {:?}", task.status.state);
/// # Ok(())
/// # }
/// ```
pub struct A2aClient {
    client: reqwest::Client,
    require_tls: bool,
    ssrf_protection: bool,
    /// Per-request timeout applied to `rpc_call` (send + JSON parse) and to the initial
    /// `send()` in `stream_message`. The SSE body stream itself is not bounded — that
    /// is the caller's responsibility.
    ///
    /// If the underlying `reqwest::Client` was also built with `.timeout()`, both limits
    /// race: whichever fires first wins. `request_timeout` takes semantic priority because
    /// it maps to `A2aError::Timeout`; the reqwest-level timeout maps to `A2aError::Http`.
    request_timeout: Duration,
}

impl A2aClient {
    /// Create a new `A2aClient` with no security restrictions.
    ///
    /// Security features are disabled by default for local/dev usage. Enable them
    /// with [`with_security`](Self::with_security) for production deployments.
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            require_tls: false,
            ssrf_protection: false,
            request_timeout: Duration::from_secs(30),
        }
    }

    /// Configure TLS enforcement and SSRF protection for this client.
    ///
    /// Both flags default to `false`. This method uses the builder pattern and
    /// can be chained directly after [`new`](Self::new).
    ///
    /// - `require_tls`: reject any endpoint that does not start with `https://`.
    /// - `ssrf_protection`: resolve the endpoint hostname via DNS and reject private IP ranges.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::A2aClient;
    ///
    /// let client = A2aClient::new(reqwest::Client::new())
    ///     .with_security(true, true);
    /// ```
    #[must_use]
    pub fn with_security(mut self, require_tls: bool, ssrf_protection: bool) -> Self {
        self.require_tls = require_tls;
        self.ssrf_protection = ssrf_protection;
        self
    }

    /// Set the per-request timeout for RPC and streaming connection calls (default: 30 seconds).
    ///
    /// Applied to the full send + JSON response parse in `rpc_call`, and to the initial
    /// HTTP `send()` in `stream_message`. The SSE body stream after connection is intentionally
    /// unbounded — streams can legitimately run for a long time.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// # Errors
    /// Returns `A2aError` on network, JSON, or JSON-RPC errors, or `A2aError::Timeout`
    /// if the request exceeds the configured `request_timeout`.
    pub async fn send_message(
        &self,
        endpoint: &str,
        params: SendMessageParams,
        token: Option<&str>,
    ) -> Result<Task, A2aError> {
        self.rpc_call(endpoint, METHOD_SEND_MESSAGE, params, token)
            .await
    }

    /// # Errors
    /// Returns `A2aError` on network failure or if the SSE connection cannot be established.
    pub async fn stream_message(
        &self,
        endpoint: &str,
        params: SendMessageParams,
        token: Option<&str>,
    ) -> Result<TaskEventStream, A2aError> {
        self.validate_endpoint(endpoint).await?;
        let request = JsonRpcRequest::new(METHOD_SEND_STREAMING_MESSAGE, params);
        let mut req = self.client.post(endpoint).json(&request);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = tokio::time::timeout(self.request_timeout, req.send())
            .await
            .map_err(|_| A2aError::Timeout(self.request_timeout))?
            .map_err(A2aError::Http)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = tokio::time::timeout(Duration::from_secs(5), resp.text())
                .await
                .unwrap_or(Ok(String::new()))
                .unwrap_or_default();
            // Truncate body to avoid leaking large upstream error responses.
            let truncated = if body.len() > 256 {
                format!("{}…", &body[..256])
            } else {
                body
            };
            return Err(A2aError::Stream(format!("HTTP {status}: {truncated}")));
        }

        let event_stream = resp.bytes_stream().eventsource();
        let mapped = event_stream.filter_map(|event| match event {
            Ok(event) => {
                if event.data.is_empty() || event.data == "[DONE]" {
                    return None;
                }
                match serde_json::from_str::<JsonRpcResponse<TaskEvent>>(&event.data) {
                    Ok(rpc_resp) => match rpc_resp.into_result() {
                        Ok(task_event) => Some(Ok(task_event)),
                        Err(rpc_err) => Some(Err(A2aError::from(rpc_err))),
                    },
                    Err(e) => Some(Err(A2aError::Stream(format!(
                        "failed to parse SSE event: {e}"
                    )))),
                }
            }
            Err(e) => Some(Err(A2aError::Stream(format!("SSE stream error: {e}")))),
        });

        Ok(Box::pin(mapped))
    }

    /// # Errors
    /// Returns `A2aError` on network, JSON, or JSON-RPC errors, or `A2aError::Timeout`
    /// if the request exceeds the configured `request_timeout`.
    pub async fn get_task(
        &self,
        endpoint: &str,
        params: TaskIdParams,
        token: Option<&str>,
    ) -> Result<Task, A2aError> {
        self.rpc_call(endpoint, METHOD_GET_TASK, params, token)
            .await
    }

    /// # Errors
    /// Returns `A2aError` on network, JSON, or JSON-RPC errors, or `A2aError::Timeout`
    /// if the request exceeds the configured `request_timeout`.
    pub async fn cancel_task(
        &self,
        endpoint: &str,
        params: TaskIdParams,
        token: Option<&str>,
    ) -> Result<Task, A2aError> {
        self.rpc_call(endpoint, METHOD_CANCEL_TASK, params, token)
            .await
    }

    async fn validate_endpoint(&self, endpoint: &str) -> Result<(), A2aError> {
        if self.require_tls && !endpoint.starts_with("https://") {
            return Err(A2aError::Security(format!(
                "TLS required but endpoint uses HTTP: {endpoint}"
            )));
        }

        if self.ssrf_protection {
            let url: url::Url = endpoint
                .parse()
                .map_err(|e| A2aError::Security(format!("invalid URL: {e}")))?;

            if let Some(host) = url.host_str() {
                let addrs = tokio::net::lookup_host(format!(
                    "{}:{}",
                    host,
                    url.port_or_known_default().unwrap_or(443)
                ))
                .await
                .map_err(|e| A2aError::Security(format!("DNS resolution failed: {e}")))?;

                for addr in addrs {
                    if is_private_ip(addr.ip()) {
                        return Err(A2aError::Security(format!(
                            "SSRF protection: private IP {} for host {host}",
                            addr.ip()
                        )));
                    }
                }
            }
        }

        Ok(())
    }

    async fn rpc_call<P: Serialize, R: DeserializeOwned>(
        &self,
        endpoint: &str,
        method: &str,
        params: P,
        token: Option<&str>,
    ) -> Result<R, A2aError> {
        self.validate_endpoint(endpoint).await?;
        let request = JsonRpcRequest::new(method, params);
        let mut req = self.client.post(endpoint).json(&request);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let rpc_response: JsonRpcResponse<R> = tokio::time::timeout(self.request_timeout, async {
            let resp = req.send().await?;
            resp.json().await
        })
        .await
        .map_err(|_| A2aError::Timeout(self.request_timeout))?
        .map_err(A2aError::Http)?;
        rpc_response.into_result().map_err(A2aError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;
    use crate::jsonrpc::{JsonRpcError, JsonRpcResponse};
    use crate::types::{
        Artifact, Message, Part, Task, TaskArtifactUpdateEvent, TaskState, TaskStatus,
        TaskStatusUpdateEvent,
    };

    #[test]
    fn task_event_deserialize_status_update() {
        let event = TaskStatusUpdateEvent {
            kind: "status-update".into(),
            task_id: "t-1".into(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Working,
                timestamp: "ts".into(),
                message: Some(Message::user_text("thinking...")),
            },
            is_final: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: TaskEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, TaskEvent::StatusUpdate(_)));
    }

    #[test]
    fn task_event_deserialize_artifact_update() {
        let event = TaskArtifactUpdateEvent {
            kind: "artifact-update".into(),
            task_id: "t-1".into(),
            context_id: None,
            artifact: Artifact {
                artifact_id: "a-1".into(),
                name: None,
                parts: vec![Part::text("result")],
                metadata: None,
            },
            is_final: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: TaskEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, TaskEvent::ArtifactUpdate(_)));
    }

    #[test]
    fn rpc_response_with_task_result() {
        let task = Task {
            id: "t-1".into(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: "ts".into(),
                message: None,
            },
            artifacts: vec![],
            history: vec![],
            metadata: None,
        };
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::Value::String("req-1".into()),
            result: Some(task),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: JsonRpcResponse<Task> = serde_json::from_str(&json).unwrap();
        let task = back.into_result().unwrap();
        assert_eq!(task.id, "t-1");
        assert_eq!(task.status.state, TaskState::Completed);
    }

    #[test]
    fn rpc_response_with_error() {
        let resp: JsonRpcResponse<Task> = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::Value::String("req-1".into()),
            result: None,
            error: Some(JsonRpcError {
                code: -32001,
                message: "task not found".into(),
                data: None,
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: JsonRpcResponse<Task> = serde_json::from_str(&json).unwrap();
        let err = back.into_result().unwrap_err();
        assert_eq!(err.code, -32001);
    }

    #[test]
    fn a2a_client_construction() {
        let client = A2aClient::new(reqwest::Client::new());
        drop(client);
    }

    #[test]
    fn is_private_ip_loopback() {
        assert!(is_private_ip(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)));
        assert!(is_private_ip(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn is_private_ip_private_ranges() {
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_link_local() {
        assert!(is_private_ip("169.254.0.1".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_unspecified() {
        assert!(is_private_ip("0.0.0.0".parse().unwrap()));
        assert!(is_private_ip("::".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_public() {
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn tls_enforcement_rejects_http() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(true, false);
        let result = client.validate_endpoint("http://example.com/rpc").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, A2aError::Security(_)));
        assert!(err.to_string().contains("TLS required"));
    }

    #[tokio::test]
    async fn tls_enforcement_allows_https() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(true, false);
        let result = client.validate_endpoint("https://example.com/rpc").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn ssrf_protection_rejects_localhost() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(false, true);
        let result = client.validate_endpoint("http://127.0.0.1:8080/rpc").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("SSRF"));
    }

    #[tokio::test]
    async fn no_security_allows_http_localhost() {
        let client = A2aClient::new(reqwest::Client::new());
        let result = client.validate_endpoint("http://127.0.0.1:8080/rpc").await;
        assert!(result.is_ok());
    }

    #[test]
    fn jsonrpc_request_serialization_for_send_message() {
        let params = SendMessageParams {
            message: Message::user_text("hello"),
            configuration: None,
        };
        let req = JsonRpcRequest::new(METHOD_SEND_MESSAGE, params);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"method\":\"message/send\""));
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"hello\""));
    }

    #[test]
    fn jsonrpc_request_serialization_for_get_task() {
        let params = TaskIdParams {
            id: "task-123".into(),
            history_length: Some(5),
        };
        let req = JsonRpcRequest::new(METHOD_GET_TASK, params);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"method\":\"tasks/get\""));
        assert!(json.contains("\"task-123\""));
        assert!(json.contains("\"historyLength\":5"));
    }

    #[test]
    fn jsonrpc_request_serialization_for_cancel_task() {
        let params = TaskIdParams {
            id: "task-456".into(),
            history_length: None,
        };
        let req = JsonRpcRequest::new(METHOD_CANCEL_TASK, params);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"method\":\"tasks/cancel\""));
        assert!(!json.contains("historyLength"));
    }

    #[test]
    fn jsonrpc_request_serialization_for_stream() {
        let params = SendMessageParams {
            message: Message::user_text("stream me"),
            configuration: None,
        };
        let req = JsonRpcRequest::new(METHOD_SEND_STREAMING_MESSAGE, params);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"method\":\"message/stream\""));
    }

    #[tokio::test]
    async fn send_message_connection_error() {
        let client = A2aClient::new(reqwest::Client::new());
        let params = SendMessageParams {
            message: Message::user_text("hello"),
            configuration: None,
        };
        let result = client
            .send_message("http://127.0.0.1:1/rpc", params, None)
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), A2aError::Http(_)));
    }

    #[tokio::test]
    async fn get_task_connection_error() {
        let client = A2aClient::new(reqwest::Client::new());
        let params = TaskIdParams {
            id: "t-1".into(),
            history_length: None,
        };
        let result = client
            .get_task("http://127.0.0.1:1/rpc", params, None)
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), A2aError::Http(_)));
    }

    #[tokio::test]
    async fn cancel_task_connection_error() {
        let client = A2aClient::new(reqwest::Client::new());
        let params = TaskIdParams {
            id: "t-1".into(),
            history_length: None,
        };
        let result = client
            .cancel_task("http://127.0.0.1:1/rpc", params, None)
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), A2aError::Http(_)));
    }

    #[tokio::test]
    async fn stream_message_connection_error() {
        let client = A2aClient::new(reqwest::Client::new());
        let params = SendMessageParams {
            message: Message::user_text("stream me"),
            configuration: None,
        };
        let result = client
            .stream_message("http://127.0.0.1:1/rpc", params, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn stream_message_tls_required_rejects_http() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(true, false);
        let params = SendMessageParams {
            message: Message::user_text("hello"),
            configuration: None,
        };
        let result = client
            .stream_message("http://example.com/rpc", params, None)
            .await;
        match result {
            Err(A2aError::Security(msg)) => assert!(msg.contains("TLS required")),
            _ => panic!("expected Security error"),
        }
    }

    #[tokio::test]
    async fn send_message_tls_required_rejects_http() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(true, false);
        let params = SendMessageParams {
            message: Message::user_text("hello"),
            configuration: None,
        };
        let result = client
            .send_message("http://example.com/rpc", params, None)
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), A2aError::Security(_)));
    }

    #[tokio::test]
    async fn get_task_tls_required_rejects_http() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(true, false);
        let params = TaskIdParams {
            id: "t-1".into(),
            history_length: None,
        };
        let result = client
            .get_task("http://example.com/rpc", params, None)
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), A2aError::Security(_)));
    }

    #[tokio::test]
    async fn cancel_task_tls_required_rejects_http() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(true, false);
        let params = TaskIdParams {
            id: "t-1".into(),
            history_length: None,
        };
        let result = client
            .cancel_task("http://example.com/rpc", params, None)
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), A2aError::Security(_)));
    }

    #[tokio::test]
    async fn validate_endpoint_invalid_url_with_ssrf() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(false, true);
        let result = client.validate_endpoint("not-a-url").await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), A2aError::Security(_)));
    }

    #[test]
    fn with_security_returns_configured_client() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(true, true);
        assert!(client.require_tls);
        assert!(client.ssrf_protection);
    }

    #[test]
    fn default_client_no_security() {
        let client = A2aClient::new(reqwest::Client::new());
        assert!(!client.require_tls);
        assert!(!client.ssrf_protection);
    }

    #[test]
    fn task_event_clone() {
        let event = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
            kind: "status-update".into(),
            task_id: "t-1".into(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Working,
                timestamp: "ts".into(),
                message: None,
            },
            is_final: false,
        });
        let cloned = event.clone();
        let json1 = serde_json::to_string(&event).unwrap();
        let json2 = serde_json::to_string(&cloned).unwrap();
        assert_eq!(json1, json2);
    }

    #[test]
    fn task_event_debug() {
        let event = TaskEvent::ArtifactUpdate(TaskArtifactUpdateEvent {
            kind: "artifact-update".into(),
            task_id: "t-1".into(),
            context_id: None,
            artifact: Artifact {
                artifact_id: "a-1".into(),
                name: None,
                parts: vec![Part::text("data")],
                metadata: None,
            },
            is_final: true,
        });
        let dbg = format!("{event:?}");
        assert!(dbg.contains("ArtifactUpdate"));
    }

    #[test]
    fn is_private_ip_ipv4_non_private() {
        assert!(!is_private_ip("93.184.216.34".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_ipv6_non_private() {
        assert!(!is_private_ip("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn rpc_response_error_takes_priority_over_result() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::Value::String("1".into()),
            result: Some(Task {
                id: "t-1".into(),
                context_id: None,
                status: TaskStatus {
                    state: TaskState::Completed,
                    timestamp: "ts".into(),
                    message: None,
                },
                artifacts: vec![],
                history: vec![],
                metadata: None,
            }),
            error: Some(JsonRpcError {
                code: -32001,
                message: "error".into(),
                data: None,
            }),
        };
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.code, -32001);
    }

    #[test]
    fn rpc_response_neither_result_nor_error() {
        let resp: JsonRpcResponse<Task> = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::Value::String("1".into()),
            result: None,
            error: None,
        };
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.code, -32603);
    }

    #[test]
    fn task_event_serialize_round_trip() {
        let event = TaskEvent::StatusUpdate(TaskStatusUpdateEvent {
            kind: "status-update".into(),
            task_id: "t-1".into(),
            context_id: Some("ctx-1".into()),
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: "2025-01-01T00:00:00Z".into(),
                message: Some(Message::user_text("done")),
            },
            is_final: true,
        });
        let json = serde_json::to_string(&event).unwrap();
        let back: TaskEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskEvent::StatusUpdate(_)));
    }
}

#[cfg(test)]
mod wiremock_tests {
    use tokio_stream::StreamExt;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::client::A2aClient;
    use crate::jsonrpc::{SendMessageParams, TaskIdParams};
    use crate::testing::*;
    use crate::types::Message;

    #[tokio::test]
    async fn send_message_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(task_rpc_response("task-1", "submitted"))
            .mount(&server)
            .await;

        let client = A2aClient::new(reqwest::Client::new());
        let params = SendMessageParams {
            message: Message::user_text("hello"),
            configuration: None,
        };
        let task = client
            .send_message(&format!("{}/rpc", server.uri()), params, None)
            .await
            .unwrap();
        assert_eq!(task.id, "task-1");
    }

    #[tokio::test]
    async fn send_message_rpc_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(task_rpc_error_response(-32001, "task not found"))
            .mount(&server)
            .await;

        let client = A2aClient::new(reqwest::Client::new());
        let params = SendMessageParams {
            message: Message::user_text("hi"),
            configuration: None,
        };
        let result = client
            .send_message(&format!("{}/rpc", server.uri()), params, None)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(
            err,
            crate::error::A2aError::JsonRpc { code: -32001, .. }
        ));
    }

    #[tokio::test]
    async fn send_message_with_bearer_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(task_rpc_response("task-auth", "submitted"))
            .mount(&server)
            .await;

        let client = A2aClient::new(reqwest::Client::new());
        let params = SendMessageParams {
            message: Message::user_text("secure"),
            configuration: None,
        };
        let task = client
            .send_message(
                &format!("{}/rpc", server.uri()),
                params,
                Some("secret-token"),
            )
            .await
            .unwrap();
        assert_eq!(task.id, "task-auth");
    }

    #[tokio::test]
    async fn get_task_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(task_rpc_response("task-get", "completed"))
            .mount(&server)
            .await;

        let client = A2aClient::new(reqwest::Client::new());
        let params = TaskIdParams {
            id: "task-get".into(),
            history_length: None,
        };
        let task = client
            .get_task(&format!("{}/rpc", server.uri()), params, None)
            .await
            .unwrap();
        assert_eq!(task.id, "task-get");
    }

    #[tokio::test]
    async fn cancel_task_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(task_rpc_response("task-cancel", "canceled"))
            .mount(&server)
            .await;

        let client = A2aClient::new(reqwest::Client::new());
        let params = TaskIdParams {
            id: "task-cancel".into(),
            history_length: None,
        };
        let task = client
            .cancel_task(&format!("{}/rpc", server.uri()), params, None)
            .await
            .unwrap();
        assert_eq!(task.id, "task-cancel");
    }

    #[tokio::test]
    async fn stream_message_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(sse_task_events_response("task-stream", "result content"))
            .mount(&server)
            .await;

        let client = A2aClient::new(reqwest::Client::new());
        let params = SendMessageParams {
            message: Message::user_text("stream"),
            configuration: None,
        };
        let stream = client
            .stream_message(&format!("{}/rpc", server.uri()), params, None)
            .await
            .unwrap();
        let events: Vec<_> = stream.collect().await;
        assert!(!events.is_empty());
    }

    #[tokio::test]
    async fn stream_message_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .mount(&server)
            .await;

        let client = A2aClient::new(reqwest::Client::new());
        let params = SendMessageParams {
            message: Message::user_text("fail"),
            configuration: None,
        };
        let result = client
            .stream_message(&format!("{}/rpc", server.uri()), params, None)
            .await;
        let err = result.err().expect("expected error");
        assert!(matches!(err, crate::error::A2aError::Stream(_)));
    }

    #[tokio::test]
    async fn rpc_call_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rpc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(5))
                    .set_body_json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": "req-1",
                        "result": {
                            "id": "t-1",
                            "status": {"state": "completed", "timestamp": "2026-01-01T00:00:00Z"}
                        }
                    })),
            )
            .mount(&server)
            .await;

        let client = A2aClient::new(reqwest::Client::new())
            .with_request_timeout(std::time::Duration::from_millis(100));
        let params = SendMessageParams {
            message: Message::user_text("hello"),
            configuration: None,
        };
        let result = client
            .send_message(&format!("{}/rpc", server.uri()), params, None)
            .await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), crate::error::A2aError::Timeout(_)),
            "expected Timeout error"
        );
    }
}
