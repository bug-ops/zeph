use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::types::Message;

pub const METHOD_SEND_MESSAGE: &str = "message/send";
pub const METHOD_SEND_STREAMING_MESSAGE: &str = "message/stream";
pub const METHOD_GET_TASK: &str = "tasks/get";
pub const METHOD_CANCEL_TASK: &str = "tasks/cancel";

pub const ERR_TASK_NOT_FOUND: i32 = -32001;
pub const ERR_TASK_NOT_CANCELABLE: i32 = -32002;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest<P> {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    pub params: P,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(deserialize = "R: Deserialize<'de>"))]
pub struct JsonRpcResponse<R> {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<R>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMessageParams {
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<TaskConfiguration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskIdParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
}

impl<P: Serialize> JsonRpcRequest<P> {
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
    /// # Errors
    /// Returns `JsonRpcError` if the response contains an error or no result.
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
