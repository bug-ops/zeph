use crate::jsonrpc::JsonRpcError;

#[derive(Debug, thiserror::Error)]
pub enum A2aError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON serialization/deserialization failed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("JSON-RPC error {code}: {message}")]
    JsonRpc { code: i32, message: String },

    #[error("agent discovery failed for {url}: {reason}")]
    Discovery { url: String, reason: String },

    #[error("SSE stream error: {0}")]
    Stream(String),

    #[error("server error: {0}")]
    Server(String),

    #[error("security policy violation: {0}")]
    Security(String),
}

impl From<JsonRpcError> for A2aError {
    fn from(e: JsonRpcError) -> Self {
        Self::JsonRpc {
            code: e.code,
            message: e.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_jsonrpc_error() {
        let rpc_err = JsonRpcError {
            code: -32001,
            message: "task not found".into(),
            data: None,
        };
        let err: A2aError = rpc_err.into();
        match err {
            A2aError::JsonRpc { code, message } => {
                assert_eq!(code, -32001);
                assert_eq!(message, "task not found");
            }
            _ => panic!("expected JsonRpc variant"),
        }
    }

    #[test]
    fn error_display() {
        let err = A2aError::Discovery {
            url: "http://example.com".into(),
            reason: "connection refused".into(),
        };
        assert_eq!(
            err.to_string(),
            "agent discovery failed for http://example.com: connection refused"
        );

        let err = A2aError::Stream("unexpected EOF".into());
        assert_eq!(err.to_string(), "SSE stream error: unexpected EOF");
    }

    #[test]
    fn security_error_display() {
        let err = A2aError::Security("TLS required but endpoint uses HTTP".into());
        assert_eq!(
            err.to_string(),
            "security policy violation: TLS required but endpoint uses HTTP"
        );
    }

    #[test]
    fn from_serde_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let err: A2aError = json_err.into();
        assert!(matches!(err, A2aError::Json(_)));
    }
}
