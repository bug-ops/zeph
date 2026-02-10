#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("connection failed for server '{server_id}': {source}")]
    Connection {
        server_id: String,
        source: anyhow::Error,
    },

    #[error("tool call failed: {server_id}/{tool_name}: {source}")]
    ToolCall {
        server_id: String,
        tool_name: String,
        source: anyhow::Error,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_error_display() {
        let err = McpError::Connection {
            server_id: "github".into(),
            source: anyhow::anyhow!("refused"),
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
            source: anyhow::anyhow!("not found"),
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
