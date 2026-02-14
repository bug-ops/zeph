use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransport;
use tokio::process::Command;

use crate::error::McpError;
use crate::tool::McpTool;

type ClientService = RunningService<rmcp::RoleClient, ()>;

pub struct McpClient {
    server_id: String,
    service: Arc<ClientService>,
    timeout: Duration,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("server_id", &self.server_id)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// Spawn child process, perform MCP handshake.
    ///
    /// # Errors
    ///
    /// Returns `McpError::Connection` if the process cannot be spawned or handshake fails.
    pub async fn connect(
        server_id: &str,
        command: &str,
        args: &[String],
        env: &std::collections::HashMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let transport = TokioChildProcess::new(cmd).map_err(|e| McpError::Connection {
            server_id: server_id.into(),
            message: e.to_string(),
        })?;

        let service =
            ().serve(transport)
                .await
                .map_err(|e| McpError::Connection {
                    server_id: server_id.into(),
                    message: e.to_string(),
                })?;

        Ok(Self {
            server_id: server_id.into(),
            service: Arc::new(service),
            timeout,
        })
    }

    /// Connect to a remote MCP server over Streamable HTTP.
    ///
    /// # Errors
    ///
    /// Returns `McpError::Connection` if the HTTP connection or handshake fails.
    pub async fn connect_url(
        server_id: &str,
        url: &str,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let transport = StreamableHttpClientTransport::from_uri(url.to_owned());

        let service =
            ().serve(transport)
                .await
                .map_err(|e| McpError::Connection {
                    server_id: server_id.into(),
                    message: e.to_string(),
                })?;

        Ok(Self {
            server_id: server_id.into(),
            service: Arc::new(service),
            timeout,
        })
    }

    /// Call tools/list, convert to `McpTool` vec.
    ///
    /// # Errors
    ///
    /// Returns `McpError::ToolCall` if listing fails.
    pub async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        let tools = self
            .service
            .list_all_tools()
            .await
            .map_err(|e| McpError::ToolCall {
                server_id: self.server_id.clone(),
                tool_name: "tools/list".into(),
                message: e.to_string(),
            })?;

        Ok(tools
            .into_iter()
            .map(|t| McpTool {
                server_id: self.server_id.clone(),
                name: t.name.to_string(),
                description: t.description.map_or_else(String::new, |d| d.to_string()),
                input_schema: serde_json::to_value(&*t.input_schema).unwrap_or_default(),
            })
            .collect())
    }

    /// Call tools/call with JSON args, return the result.
    ///
    /// # Errors
    ///
    /// Returns `McpError::Timeout` or `McpError::ToolCall` on failure.
    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        let arguments: Option<serde_json::Map<String, serde_json::Value>> = args
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect());

        let params = CallToolRequestParams {
            name: Cow::Owned(name.to_owned()),
            arguments,
            task: None,
            meta: None,
        };

        let result = tokio::time::timeout(self.timeout, self.service.call_tool(params))
            .await
            .map_err(|_| McpError::Timeout {
                server_id: self.server_id.clone(),
                tool_name: name.into(),
                timeout_secs: self.timeout.as_secs(),
            })?
            .map_err(|e| McpError::ToolCall {
                server_id: self.server_id.clone(),
                tool_name: name.into(),
                message: e.to_string(),
            })?;

        Ok(result)
    }

    /// Graceful shutdown.
    pub async fn shutdown(self) {
        match Arc::try_unwrap(self.service) {
            Ok(service) => {
                let _ = service.cancel().await;
            }
            Err(_arc) => {
                tracing::warn!(
                    server_id = self.server_id,
                    "cannot shutdown: service has multiple references"
                );
            }
        }
    }
}
