use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::CallToolResult;
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use crate::client::McpClient;
use crate::error::McpError;
use crate::tool::McpTool;

/// Server connection parameters consumed by `McpManager`.
#[derive(Debug, Clone)]
pub struct ServerEntry {
    pub id: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub timeout: Duration,
}

pub struct McpManager {
    configs: Vec<ServerEntry>,
    clients: Arc<RwLock<HashMap<String, McpClient>>>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager")
            .field("server_count", &self.configs.len())
            .finish_non_exhaustive()
    }
}

impl McpManager {
    #[must_use]
    pub fn new(configs: Vec<ServerEntry>) -> Self {
        Self {
            configs,
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Connect to all configured servers concurrently, return aggregated tool list.
    /// Servers that fail to connect are logged and skipped.
    pub async fn connect_all(&self) -> Vec<McpTool> {
        let mut join_set = JoinSet::new();

        for config in self.configs.clone() {
            join_set.spawn(async move {
                let result = McpClient::connect(
                    &config.id,
                    &config.command,
                    &config.args,
                    &config.env,
                    config.timeout,
                )
                .await;
                (config.id, result)
            });
        }

        let mut all_tools = Vec::new();
        let mut clients = self.clients.write().await;

        while let Some(result) = join_set.join_next().await {
            let Ok((server_id, connect_result)) = result else {
                tracing::warn!("MCP connection task panicked");
                continue;
            };

            match connect_result {
                Ok(client) => match client.list_tools().await {
                    Ok(tools) => {
                        tracing::info!(server_id, tools = tools.len(), "connected to MCP server");
                        all_tools.extend(tools);
                        clients.insert(server_id, client);
                    }
                    Err(e) => {
                        tracing::warn!(server_id, "failed to list tools: {e:#}");
                    }
                },
                Err(e) => {
                    tracing::warn!(server_id, "MCP server connection failed: {e:#}");
                }
            }
        }

        all_tools
    }

    /// Route tool call to the correct server's client.
    ///
    /// # Errors
    ///
    /// Returns `McpError::ServerNotFound` if the server is not connected.
    pub async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        let clients = self.clients.read().await;
        let client = clients
            .get(server_id)
            .ok_or_else(|| McpError::ServerNotFound {
                server_id: server_id.into(),
            })?;
        client.call_tool(tool_name, args).await
    }

    /// Graceful shutdown of all connections.
    pub async fn shutdown_all(self) {
        let mut clients = self.clients.write().await;
        let drained: Vec<(String, McpClient)> = clients.drain().collect();
        for (id, client) in drained {
            tracing::info!(server_id = id, "shutting down MCP client");
            client.shutdown().await;
        }
    }
}
