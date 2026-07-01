// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A2A protocol HTTP client with optional TLS enforcement and SSRF protection.

use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_core::Stream;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio_stream::StreamExt;
use zeph_common::net::resolve_and_validate;

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
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TaskEvent {
    /// A task lifecycle transition (e.g., `submitted` → `working` → `completed`).
    StatusUpdate(TaskStatusUpdateEvent),
    /// A new or updated output artifact from the agent.
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

/// Security posture applied to outbound [`A2aClient`] requests.
///
/// Named fields eliminate the transposition hazard of a two-bool builder method
/// (`with_security(true, false)` vs. `with_security(false, true)` are easy to swap
/// by accident) and group the security boundary as one reviewable unit.
///
/// # Examples
///
/// ```rust
/// use zeph_a2a::{A2aClient, SecurityPolicy};
///
/// // Recommended for production: reject HTTP and private/loopback targets.
/// let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy::hardened());
///
/// // Partial policy via named fields — no ambiguity about which flag is which.
/// let tls_only = SecurityPolicy {
///     require_tls: true,
///     ssrf_protection: false,
/// };
/// let _ = client.with_security(tls_only);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecurityPolicy {
    /// Reject any endpoint that does not start with `https://`, and build requests with
    /// `https_only(true)` so a redirect cannot silently downgrade the connection to `http://`.
    pub require_tls: bool,
    /// Resolve the endpoint hostname via DNS, reject private/loopback/link-local ranges,
    /// and pin the validated address for the actual connection so it cannot be re-resolved
    /// to a different (attacker-controlled) address between the check and the connect.
    pub ssrf_protection: bool,
}

impl SecurityPolicy {
    /// Both protections enabled. The recommended posture for production deployments
    /// that talk to untrusted or third-party A2A endpoints.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::SecurityPolicy;
    ///
    /// let policy = SecurityPolicy::hardened();
    /// assert!(policy.require_tls);
    /// assert!(policy.ssrf_protection);
    /// ```
    #[must_use]
    pub const fn hardened() -> Self {
        Self {
            require_tls: true,
            ssrf_protection: true,
        }
    }

    /// Both protections disabled. Suitable only for local development against
    /// trusted, non-adversarial endpoints (e.g. `http://localhost`).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::SecurityPolicy;
    ///
    /// let policy = SecurityPolicy::permissive();
    /// assert!(!policy.require_tls);
    /// assert!(!policy.ssrf_protection);
    /// ```
    #[must_use]
    pub const fn permissive() -> Self {
        Self {
            require_tls: false,
            ssrf_protection: false,
        }
    }
}

/// A DNS-validated hostname and its resolved addresses, used to pin the actual HTTP
/// connection to the exact addresses that passed SSRF validation (see [`SecurityPolicy`]).
#[derive(Debug)]
struct PinnedTarget {
    host: String,
    addrs: Vec<SocketAddr>,
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
/// production deployments — see [`SecurityPolicy`]. When either flag is enabled,
/// each request is sent through a dedicated per-request `reqwest::Client` with
/// redirects disabled and, when `ssrf_protection` is on, the connection pinned to
/// the exact addresses that were validated (no re-resolution at connect time).
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_a2a::{A2aClient, SecurityPolicy, SendMessageParams, Message};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let client = A2aClient::new(reqwest::Client::new())
///     .with_security(SecurityPolicy::hardened());
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
    security: SecurityPolicy,
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
            security: SecurityPolicy::permissive(),
            request_timeout: Duration::from_secs(30),
        }
    }

    /// Configure the [`SecurityPolicy`] for this client.
    ///
    /// Defaults to [`SecurityPolicy::permissive()`] (no restrictions). This method
    /// uses the builder pattern and can be chained directly after [`new`](Self::new).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::{A2aClient, SecurityPolicy};
    ///
    /// let client = A2aClient::new(reqwest::Client::new())
    ///     .with_security(SecurityPolicy::hardened());
    /// ```
    #[must_use]
    pub fn with_security(mut self, policy: SecurityPolicy) -> Self {
        self.security = policy;
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
    #[tracing::instrument(name = "a2a.client.send_message", skip_all, err)]
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
    #[tracing::instrument(name = "a2a.client.stream_message", skip_all, err)]
    pub async fn stream_message(
        &self,
        endpoint: &str,
        params: SendMessageParams,
        token: Option<&str>,
    ) -> Result<TaskEventStream, A2aError> {
        let pinned = self.validate_endpoint(endpoint).await?;
        let request_client = self.request_client(pinned.as_ref())?;
        let request = JsonRpcRequest::new(METHOD_SEND_STREAMING_MESSAGE, params);
        let mut req = request_client.post(endpoint).json(&request);
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
    #[tracing::instrument(name = "a2a.client.get_task", skip_all, err)]
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
    #[tracing::instrument(name = "a2a.client.cancel_task", skip_all, err)]
    pub async fn cancel_task(
        &self,
        endpoint: &str,
        params: TaskIdParams,
        token: Option<&str>,
    ) -> Result<Task, A2aError> {
        self.rpc_call(endpoint, METHOD_CANCEL_TASK, params, token)
            .await
    }

    /// Validates `endpoint` against the configured [`SecurityPolicy`] and, when
    /// `ssrf_protection` is enabled, resolves its hostname once and returns the
    /// validated addresses to be pinned for the actual connection.
    ///
    /// Returning `Ok(None)` means either security is off for that check, or the
    /// endpoint has no host (validation is skipped, matching prior behavior).
    #[tracing::instrument(name = "a2a.client.validate_endpoint", skip_all, err)]
    async fn validate_endpoint(&self, endpoint: &str) -> Result<Option<PinnedTarget>, A2aError> {
        if self.security.require_tls && !endpoint.starts_with("https://") {
            return Err(A2aError::Security(format!(
                "TLS required but endpoint uses HTTP: {endpoint}"
            )));
        }

        if !self.security.ssrf_protection {
            return Ok(None);
        }

        let url: url::Url = endpoint
            .parse()
            .map_err(|e| A2aError::Security(format!("invalid URL: {e}")))?;

        let Some(host) = url.host_str() else {
            return Ok(None);
        };
        let port = url.port_or_known_default().unwrap_or(443);
        let addrs = resolve_and_validate(host, port)
            .await
            .map_err(|e| A2aError::Security(e.to_string()))?;

        Ok(Some(PinnedTarget {
            host: host.to_owned(),
            addrs,
        }))
    }

    /// Returns `true` when either half of the [`SecurityPolicy`] requires requests
    /// to be sent through a dedicated per-request client instead of `self.client`.
    fn needs_hardened_client(&self) -> bool {
        self.security.require_tls || self.security.ssrf_protection
    }

    /// Selects the `reqwest::Client` to use for a single request: the shared
    /// injected client when no security is configured, or a fresh hardened client
    /// (redirects disabled, optionally TLS-enforced and address-pinned) otherwise.
    fn request_client(&self, pinned: Option<&PinnedTarget>) -> Result<reqwest::Client, A2aError> {
        if self.needs_hardened_client() {
            self.build_hardened_client(pinned)
        } else {
            Ok(self.client.clone())
        }
    }

    /// Builds a per-request client hardened per the configured [`SecurityPolicy`].
    ///
    /// Redirects are always disabled (`Policy::none()`) so a malicious `3xx` response
    /// cannot silently redirect the connection to a private address or downgrade to
    /// `http://` — the caller (`rpc_call`/`stream_message`) treats any non-2xx or
    /// unparseable response as an error instead of following it. When `pinned` is
    /// `Some`, the client is additionally locked to the exact addresses that passed
    /// SSRF validation via `resolve_to_addrs`, so reqwest cannot re-resolve the
    /// hostname to a different address at connect time (closing the DNS-rebinding
    /// TOCTOU window).
    fn build_hardened_client(
        &self,
        pinned: Option<&PinnedTarget>,
    ) -> Result<reqwest::Client, A2aError> {
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("zeph-a2a/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none());

        if self.security.require_tls {
            builder = builder.https_only(true);
        }
        if let Some(target) = pinned {
            builder = builder.resolve_to_addrs(&target.host, &target.addrs);
        }

        builder
            .build()
            .map_err(|e| A2aError::Security(format!("failed to build hardened client: {e}")))
    }

    #[tracing::instrument(name = "a2a.client.rpc_call", skip_all, err)]
    async fn rpc_call<P: Serialize, R: DeserializeOwned>(
        &self,
        endpoint: &str,
        method: &str,
        params: P,
        token: Option<&str>,
    ) -> Result<R, A2aError> {
        let pinned = self.validate_endpoint(endpoint).await?;
        let request_client = self.request_client(pinned.as_ref())?;
        let request = JsonRpcRequest::new(method, params);
        let mut req = request_client.post(endpoint).json(&request);
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
    use std::assert_matches;
    use std::net::IpAddr;

    use super::*;
    use zeph_common::net::is_private_ip;

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
        assert_matches!(parsed, TaskEvent::StatusUpdate(_));
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
        assert_matches!(parsed, TaskEvent::ArtifactUpdate(_));
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
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: true,
            ssrf_protection: false,
        });
        let result = client.validate_endpoint("http://example.com/rpc").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_matches!(err, A2aError::Security(_));
        assert!(err.to_string().contains("TLS required"));
    }

    #[tokio::test]
    async fn tls_enforcement_allows_https() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: true,
            ssrf_protection: false,
        });
        let result = client.validate_endpoint("https://example.com/rpc").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn ssrf_protection_rejects_localhost() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: false,
            ssrf_protection: true,
        });
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
        assert_matches!(result.unwrap_err(), A2aError::Http(_));
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
        assert_matches!(result.unwrap_err(), A2aError::Http(_));
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
        assert_matches!(result.unwrap_err(), A2aError::Http(_));
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
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: true,
            ssrf_protection: false,
        });
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
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: true,
            ssrf_protection: false,
        });
        let params = SendMessageParams {
            message: Message::user_text("hello"),
            configuration: None,
        };
        let result = client
            .send_message("http://example.com/rpc", params, None)
            .await;
        assert!(result.is_err());
        assert_matches!(result.unwrap_err(), A2aError::Security(_));
    }

    #[tokio::test]
    async fn get_task_tls_required_rejects_http() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: true,
            ssrf_protection: false,
        });
        let params = TaskIdParams {
            id: "t-1".into(),
            history_length: None,
        };
        let result = client
            .get_task("http://example.com/rpc", params, None)
            .await;
        assert!(result.is_err());
        assert_matches!(result.unwrap_err(), A2aError::Security(_));
    }

    #[tokio::test]
    async fn cancel_task_tls_required_rejects_http() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: true,
            ssrf_protection: false,
        });
        let params = TaskIdParams {
            id: "t-1".into(),
            history_length: None,
        };
        let result = client
            .cancel_task("http://example.com/rpc", params, None)
            .await;
        assert!(result.is_err());
        assert_matches!(result.unwrap_err(), A2aError::Security(_));
    }

    #[tokio::test]
    async fn validate_endpoint_invalid_url_with_ssrf() {
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: false,
            ssrf_protection: true,
        });
        let result = client.validate_endpoint("not-a-url").await;
        assert!(result.is_err());
        assert_matches!(result.unwrap_err(), A2aError::Security(_));
    }

    #[test]
    fn with_security_returns_configured_client() {
        let client =
            A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy::hardened());
        assert!(client.security.require_tls);
        assert!(client.security.ssrf_protection);
    }

    #[test]
    fn default_client_no_security() {
        let client = A2aClient::new(reqwest::Client::new());
        assert!(!client.security.require_tls);
        assert!(!client.security.ssrf_protection);
    }

    #[test]
    fn needs_hardened_client_reflects_policy() {
        assert!(!A2aClient::new(reqwest::Client::new()).needs_hardened_client());
        assert!(
            A2aClient::new(reqwest::Client::new())
                .with_security(SecurityPolicy {
                    require_tls: true,
                    ssrf_protection: false,
                })
                .needs_hardened_client()
        );
        assert!(
            A2aClient::new(reqwest::Client::new())
                .with_security(SecurityPolicy {
                    require_tls: false,
                    ssrf_protection: true,
                })
                .needs_hardened_client()
        );
        assert!(
            A2aClient::new(reqwest::Client::new())
                .with_security(SecurityPolicy::hardened())
                .needs_hardened_client()
        );
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
        assert_matches!(back, TaskEvent::StatusUpdate(_));
    }
}

#[cfg(test)]
mod wiremock_tests {
    use std::assert_matches;
    use tokio_stream::StreamExt;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::client::{A2aClient, PinnedTarget, SecurityPolicy};
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
        assert_matches!(err, crate::error::A2aError::JsonRpc { code: -32001, .. });
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
        assert_matches!(err, crate::error::A2aError::Stream(_));
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

    /// Proves the DNS-rebinding TOCTOU is closed: `resolve_to_addrs` pins the connection to
    /// the address validated by `resolve_and_validate`, so reqwest never re-resolves `fake_host`
    /// (a hostname reserved by RFC 2606 and guaranteed to never resolve via real DNS) at connect
    /// time. If the client re-resolved instead of using the pinned address, this request would
    /// fail with a DNS lookup error rather than reaching the mock server.
    #[tokio::test]
    async fn hardened_client_pins_connection_bypassing_dns() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let addr = *server.address();
        let fake_host = "zeph-a2a-pin-test.invalid";
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: false,
            ssrf_protection: true,
        });
        let pinned = PinnedTarget {
            host: fake_host.to_owned(),
            addrs: vec![addr],
        };
        let hardened = client.build_hardened_client(Some(&pinned)).unwrap();

        let resp = hardened
            .get(format!("http://{fake_host}:{}/", addr.port()))
            .send()
            .await
            .unwrap_or_else(|e| panic!("pinned request to unresolvable host failed: {e}"));
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    /// Proves the redirect-based SSRF bypass is closed: the hardened client does not
    /// automatically follow a `3xx` response, even when `Location` points at a private
    /// address. `rpc_call`/`stream_message` treat the raw redirect response as a normal
    /// (non-2xx) response and surface an error instead of connecting to `Location`.
    #[tokio::test]
    async fn hardened_client_does_not_auto_follow_redirect_to_private_ip() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "http://127.0.0.1:9/internal"),
            )
            .mount(&server)
            .await;

        let addr = *server.address();
        let fake_host = "zeph-a2a-redirect-test.invalid";
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: false,
            ssrf_protection: true,
        });
        let pinned = PinnedTarget {
            host: fake_host.to_owned(),
            addrs: vec![addr],
        };
        let hardened = client.build_hardened_client(Some(&pinned)).unwrap();

        let resp = hardened
            .get(format!("http://{fake_host}:{}/start", addr.port()))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::FOUND);
        assert_eq!(
            resp.headers().get(reqwest::header::LOCATION).unwrap(),
            "http://127.0.0.1:9/internal"
        );
    }

    /// Proves TLS enforcement holds even on the hardened per-request client: `https_only(true)`
    /// rejects a plaintext `http://` connection outright, closing the https-to-http downgrade
    /// gap that an unvalidated redirect could otherwise exploit.
    #[tokio::test]
    async fn hardened_client_with_require_tls_rejects_plaintext_connection() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let addr = *server.address();
        let fake_host = "zeph-a2a-tls-test.invalid";
        let client = A2aClient::new(reqwest::Client::new()).with_security(SecurityPolicy {
            require_tls: true,
            ssrf_protection: true,
        });
        let pinned = PinnedTarget {
            host: fake_host.to_owned(),
            addrs: vec![addr],
        };
        let hardened = client.build_hardened_client(Some(&pinned)).unwrap();

        let result = hardened
            .get(format!("http://{fake_host}:{}/", addr.port()))
            .send()
            .await;
        assert!(
            result.is_err(),
            "https_only(true) must reject a plain http:// URL"
        );
    }
}
