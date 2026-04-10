// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! JSON-RPC 2.0 envelope types and A2A method constants.
//!
//! The A2A protocol is built on JSON-RPC 2.0. Every request is a [`JsonRpcRequest`] and
//! every response is a [`JsonRpcResponse`]. Use [`JsonRpcRequest::new`] to construct
//! outgoing requests with a fresh UUID `id`.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::types::Message;

/// A2A method name for sending a non-streaming message to an agent.
pub const METHOD_SEND_MESSAGE: &str = "message/send";
/// A2A method name for sending a message and receiving an SSE stream of events.
pub const METHOD_SEND_STREAMING_MESSAGE: &str = "message/stream";
/// A2A method name for retrieving a task by ID.
pub const METHOD_GET_TASK: &str = "tasks/get";
/// A2A method name for canceling a task that is not yet in a terminal state.
pub const METHOD_CANCEL_TASK: &str = "tasks/cancel";

/// JSON-RPC error code indicating that the requested task ID does not exist.
pub const ERR_TASK_NOT_FOUND: i32 = -32001;
/// JSON-RPC error code indicating that the task is in a terminal state and cannot be canceled.
pub const ERR_TASK_NOT_CANCELABLE: i32 = -32002;

/// A JSON-RPC 2.0 request envelope carrying typed parameters `P`.
///
/// The `id` is always a UUID v4 string, generated in [`JsonRpcRequest::new`].
///
/// # Examples
///
/// ```rust
/// use zeph_a2a::jsonrpc::{JsonRpcRequest, METHOD_GET_TASK, TaskIdParams};
///
/// let req = JsonRpcRequest::new(METHOD_GET_TASK, TaskIdParams {
///     id: "task-123".into(),
///     history_length: Some(10),
/// });
/// assert_eq!(req.method, "tasks/get");
/// assert_eq!(req.jsonrpc, "2.0");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest<P> {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Unique request identifier echoed back in the response.
    pub id: serde_json::Value,
    /// The A2A method being invoked (one of the `METHOD_*` constants).
    pub method: String,
    /// Method-specific parameters.
    pub params: P,
}

/// A JSON-RPC 2.0 response envelope carrying either a typed result `R` or an error.
///
/// Call [`into_result`](JsonRpcResponse::into_result) to unwrap the response ergonomically.
/// Error takes precedence if both `result` and `error` are somehow present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(deserialize = "R: Deserialize<'de>"))]
pub struct JsonRpcResponse<R> {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Echoed from the corresponding request `id`.
    pub id: serde_json::Value,
    /// Successful result payload. `None` when `error` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
    /// Error payload. `None` when `result` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object returned by the agent on method-level failures.
///
/// Well-known A2A codes are [`ERR_TASK_NOT_FOUND`] (`-32001`) and
/// [`ERR_TASK_NOT_CANCELABLE`] (`-32002`).
/// Standard JSON-RPC codes: `-32700` (parse error), `-32601` (method not found),
/// `-32602` (invalid params), `-32603` (internal error).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code.
    pub code: i32,
    /// Human-readable error description.
    pub message: String,
    /// Optional structured detail data (not exposed to end users to avoid leaking internals).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcError {}

/// Parameters for the `message/send` and `message/stream` JSON-RPC methods.
///
/// # Examples
///
/// ```rust
/// use zeph_a2a::jsonrpc::SendMessageParams;
/// use zeph_a2a::Message;
///
/// let params = SendMessageParams {
///     message: Message::user_text("Hello!"),
///     configuration: None,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageParams {
    /// The message to process.
    pub message: Message,
    /// Optional per-request task configuration overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<TaskConfiguration>,
}

/// Per-request configuration overrides for task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskConfiguration {
    /// When `true`, the `message/send` call blocks until the task completes
    /// and returns the full result in one response instead of returning immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
}

/// Parameters for the `tasks/get` and `tasks/cancel` JSON-RPC methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskIdParams {
    /// The task ID to look up or cancel.
    pub id: String,
    /// If set, limits the number of history messages returned (most recent N).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
}

impl<P: Serialize> JsonRpcRequest<P> {
    /// Create a new JSON-RPC 2.0 request with a fresh UUID `id`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::jsonrpc::{JsonRpcRequest, METHOD_CANCEL_TASK, TaskIdParams};
    ///
    /// let req = JsonRpcRequest::new(METHOD_CANCEL_TASK, TaskIdParams {
    ///     id: "task-abc".into(),
    ///     history_length: None,
    /// });
    /// assert_eq!(req.method, "tasks/cancel");
    /// ```
    #[must_use]
    pub fn new(method: &str, params: P) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: serde_json::Value::String(uuid::Uuid::new_v4().to_string()),
            method: method.into(),
            params,
        }
    }
}

impl<R: DeserializeOwned> JsonRpcResponse<R> {
    /// Unwrap the response into `Ok(result)` or `Err(error)`.
    ///
    /// If both `error` and `result` are somehow present, the error takes precedence.
    /// If neither is present (malformed response), returns an internal error with code `-32603`.
    ///
    /// # Errors
    ///
    /// Returns [`JsonRpcError`] if the response carries an error object, or if the response
    /// is malformed (neither `result` nor `error`).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_a2a::jsonrpc::{JsonRpcResponse, JsonRpcError};
    ///
    /// let resp: JsonRpcResponse<String> = JsonRpcResponse {
    ///     jsonrpc: "2.0".into(),
    ///     id: serde_json::Value::String("1".into()),
    ///     result: Some("hello".into()),
    ///     error: None,
    /// };
    /// assert_eq!(resp.into_result().unwrap(), "hello");
    /// ```
    pub fn into_result(self) -> Result<R, JsonRpcError> {
        if let Some(err) = self.error {
            return Err(err);
        }
        self.result.ok_or_else(|| JsonRpcError {
            code: -32603,
            message: "response contains neither result nor error".into(),
            data: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_new_sets_jsonrpc_and_uuid_id() {
        let req = JsonRpcRequest::new(
            METHOD_SEND_MESSAGE,
            TaskIdParams {
                id: "task-1".into(),
                history_length: None,
            },
        );
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "message/send");
        let id_str = req.id.as_str().unwrap();
        assert!(uuid::Uuid::parse_str(id_str).is_ok());
    }

    #[test]
    fn request_serde_round_trip() {
        let req = JsonRpcRequest::new(
            METHOD_GET_TASK,
            TaskIdParams {
                id: "t-1".into(),
                history_length: Some(10),
            },
        );
        let json = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest<TaskIdParams> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, METHOD_GET_TASK);
        assert_eq!(back.params.id, "t-1");
        assert_eq!(back.params.history_length, Some(10));
    }

    #[test]
    fn response_into_result_ok() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::Value::String("1".into()),
            result: Some(serde_json::json!({"id": "task-1"})),
            error: None,
        };
        let val: serde_json::Value = resp.into_result().unwrap();
        assert_eq!(val["id"], "task-1");
    }

    #[test]
    fn response_into_result_error() {
        let resp: JsonRpcResponse<serde_json::Value> = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::Value::String("1".into()),
            result: None,
            error: Some(JsonRpcError {
                code: ERR_TASK_NOT_FOUND,
                message: "task not found".into(),
                data: None,
            }),
        };
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.code, ERR_TASK_NOT_FOUND);
    }

    #[test]
    fn response_into_result_neither() {
        let resp: JsonRpcResponse<serde_json::Value> = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: serde_json::Value::String("1".into()),
            result: None,
            error: None,
        };
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.code, -32603);
    }

    #[test]
    fn send_message_params_serde() {
        let params = SendMessageParams {
            message: Message::user_text("hello"),
            configuration: Some(TaskConfiguration {
                blocking: Some(true),
            }),
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: SendMessageParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.message.text_content(), Some("hello"));
        assert_eq!(back.configuration.unwrap().blocking, Some(true));
    }

    #[test]
    fn task_id_params_skips_none() {
        let params = TaskIdParams {
            id: "t-1".into(),
            history_length: None,
        };
        let json = serde_json::to_string(&params).unwrap();
        assert!(!json.contains("historyLength"));
    }

    #[test]
    fn jsonrpc_error_display() {
        let err = JsonRpcError {
            code: -32001,
            message: "not found".into(),
            data: None,
        };
        assert_eq!(err.to_string(), "JSON-RPC error -32001: not found");
    }
}
