use std::net::IpAddr;
use std::pin::Pin;

use eventsource_stream::Eventsource;
use futures_core::Stream;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio_stream::StreamExt;

use crate::error::A2aError;
use crate::jsonrpc::{
    JsonRpcRequest, JsonRpcResponse, METHOD_CANCEL_TASK, METHOD_GET_TASK, METHOD_SEND_MESSAGE,
    METHOD_SEND_STREAMING_MESSAGE, SendMessageParams, TaskIdParams,
};
use crate::types::{Task, TaskArtifactUpdateEvent, TaskStatusUpdateEvent};

pub type TaskEventStream = Pin<Box<dyn Stream<Item = Result<TaskEvent, A2aError>> + Send>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TaskEvent {
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
}

pub struct A2aClient {
    client: reqwest::Client,
    require_tls: bool,
    ssrf_protection: bool,
}

impl A2aClient {
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            require_tls: false,
            ssrf_protection: false,
        }
    }

    #[must_use]
    pub fn with_security(mut self, require_tls: bool, ssrf_protection: bool) -> Self {
        self.require_tls = require_tls;
        self.ssrf_protection = ssrf_protection;
        self
    }

    /// # Errors
    /// Returns `A2aError` on network, JSON, or JSON-RPC errors.
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
        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
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
    /// Returns `A2aError` on network, JSON, or JSON-RPC errors.
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
    /// Returns `A2aError` on network, JSON, or JSON-RPC errors.
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
        let resp = req.send().await?;
        let rpc_response: JsonRpcResponse<R> = resp.json().await?;
        rpc_response.into_result().map_err(A2aError::from)
    }
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            let seg = v6.segments();
            // fe80::/10 — link-local
            if seg[0] & 0xffc0 == 0xfe80 {
                return true;
            }
            // fc00::/7 — unique local
            if seg[0] & 0xfe00 == 0xfc00 {
                return true;
            }
            // ::ffff:x.x.x.x — IPv4-mapped, check inner IPv4
            if seg[0..6] == [0, 0, 0, 0, 0, 0xffff] {
                let v4 = v6
                    .to_ipv4_mapped()
                    .unwrap_or(std::net::Ipv4Addr::UNSPECIFIED);
                return v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast();
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
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
