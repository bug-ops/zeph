#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("connection failed for server '{server_id}': {message}")]
    Connection { server_id: String, message: String },

    #[error("tool call failed: {server_id}/{tool_name}: {message}")]
    ToolCall {
        server_id: String,
        tool_name: String,
        message: String,
    },

    #[error("server '{server_id}' not found")]
    ServerNotFound { server_id: String },

    #[error("server '{server_id}' is already connected")]
    ServerAlreadyConnected { server_id: String },

    #[error("tool '{tool_name}' not found on server '{server_id}'")]
    ToolNotFound {
        server_id: String,
        tool_name: String,
    },

    #[error("tool call timed out after {timeout_secs}s: {server_id}/{tool_name}")]
    Timeout {
        server_id: String,
        tool_name: String,
        timeout_secs: u64,
    },

    #[error("Qdrant error: {0}")]
    Qdrant(#[from] Box<qdrant_client::QdrantError>),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("integer conversion: {0}")]
    IntConversion(#[from] std::num::TryFromIntError),

    #[error("SSRF blocked: URL '{url}' resolves to private/reserved IP {addr}")]
    SsrfBlocked { url: String, addr: String },

    #[error("invalid URL '{url}': {message}")]
    InvalidUrl { url: String, message: String },

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("MCP command '{command}' not allowed")]
    CommandNotAllowed { command: String },

    #[error("env var '{var_name}' is blocked for MCP server processes")]
    EnvVarBlocked { var_name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_error_display() {
        let err = McpError::Connection {
            server_id: "github".into(),
            message: "refused".into(),
        };
        assert_eq!(
            err.to_string(),
            "connection failed for server 'github': refused"
        );
    }

    #[test]
    fn tool_call_error_display() {
        let err = McpError::ToolCall {
            server_id: "fs".into(),
            tool_name: "read_file".into(),
            message: "not found".into(),
        };
        assert_eq!(err.to_string(), "tool call failed: fs/read_file: not found");
    }

    #[test]
    fn server_not_found_display() {
        let err = McpError::ServerNotFound {
            server_id: "missing".into(),
        };
        assert_eq!(err.to_string(), "server 'missing' not found");
    }

    #[test]
    fn tool_not_found_display() {
        let err = McpError::ToolNotFound {
            server_id: "fs".into(),
            tool_name: "delete".into(),
        };
        assert_eq!(err.to_string(), "tool 'delete' not found on server 'fs'");
    }

    #[test]
    fn server_already_connected_display() {
        let err = McpError::ServerAlreadyConnected {
            server_id: "github".into(),
        };
        assert_eq!(err.to_string(), "server 'github' is already connected");
    }

    #[test]
    fn timeout_error_display() {
        let err = McpError::Timeout {
            server_id: "slow".into(),
            tool_name: "query".into(),
            timeout_secs: 30,
        };
        assert_eq!(err.to_string(), "tool call timed out after 30s: slow/query");
    }
}
