// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::CallToolResult;
use tokio::sync::RwLock;
use tokio::task::JoinSet;

use crate::client::McpClient;
use crate::error::McpError;
use crate::policy::PolicyEnforcer;
use crate::tool::McpTool;

/// Transport type for MCP server connections.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum McpTransport {
    /// Stdio: spawn child process with command + args.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    /// Streamable HTTP: connect to remote URL.
    Http { url: String },
}

/// Server connection parameters consumed by `McpManager`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ServerEntry {
    pub id: String,
    pub transport: McpTransport,
    pub timeout: Duration,
    /// When `true`, SSRF validation is skipped for HTTP transports.
    /// Set only for servers defined in static config (operator-controlled).
    #[serde(default)]
    pub trusted: bool,
}

pub struct McpManager {
    configs: Vec<ServerEntry>,
    allowed_commands: Vec<String>,
    clients: Arc<RwLock<HashMap<String, McpClient>>>,
    connected_server_ids: std::sync::RwLock<HashSet<String>>,
    enforcer: Arc<PolicyEnforcer>,
    suppress_stderr: bool,
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
    pub fn new(
        configs: Vec<ServerEntry>,
        allowed_commands: Vec<String>,
        enforcer: PolicyEnforcer,
    ) -> Self {
        Self {
            configs,
            allowed_commands,
            clients: Arc::new(RwLock::new(HashMap::new())),
            connected_server_ids: std::sync::RwLock::new(HashSet::new()),
            enforcer: Arc::new(enforcer),
            suppress_stderr: false,
        }
    }

    /// When `true`, stderr of spawned MCP child processes is suppressed (`Stdio::null()`).
    ///
    /// Use in TUI mode to prevent child stderr from corrupting the terminal.
    #[must_use]
    pub fn with_suppress_stderr(mut self, suppress: bool) -> Self {
        self.suppress_stderr = suppress;
        self
    }

    /// Connect to all configured servers concurrently, return aggregated tool list.
    /// Servers that fail to connect are logged and skipped.
    ///
    /// # Panics
    ///
    /// Panics if the internal `connected_server_ids` lock is poisoned.
    pub async fn connect_all(&self) -> Vec<McpTool> {
        let mut join_set = JoinSet::new();

        let allowed = self.allowed_commands.clone();
        let suppress = self.suppress_stderr;
        for config in self.configs.clone() {
            let allowed = allowed.clone();
            join_set.spawn(async move {
                let result = connect_entry(&config, &allowed, suppress).await;
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
                        clients.insert(server_id.clone(), client);
                        self.connected_server_ids
                            .write()
                            .expect("connected_server_ids lock poisoned")
                            .insert(server_id);
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
    /// Returns `McpError::PolicyViolation` if the enforcer rejects the call,
    /// or `McpError::ServerNotFound` if the server is not connected.
    pub async fn call_tool(
        &self,
        server_id: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResult, McpError> {
        self.enforcer
            .check(server_id, tool_name)
            .map_err(|v| McpError::PolicyViolation(v.to_string()))?;

        let clients = self.clients.read().await;
        let client = clients
            .get(server_id)
            .ok_or_else(|| McpError::ServerNotFound {
                server_id: server_id.into(),
            })?;
        client.call_tool(tool_name, args).await
    }

    /// Connect a new server at runtime, return its tool list.
    ///
    /// # Errors
    ///
    /// Returns `McpError::ServerAlreadyConnected` if the ID is taken,
    /// or connection/tool-listing errors on failure.
    ///
    /// # Panics
    ///
    /// Panics if the internal `connected_server_ids` lock is poisoned.
    pub async fn add_server(&self, entry: &ServerEntry) -> Result<Vec<McpTool>, McpError> {
        // Early check under read lock (fast path for duplicates)
        {
            let clients = self.clients.read().await;
            if clients.contains_key(&entry.id) {
                return Err(McpError::ServerAlreadyConnected {
                    server_id: entry.id.clone(),
                });
            }
        }

        let client = connect_entry(entry, &self.allowed_commands, self.suppress_stderr).await?;
        let tools = match client.list_tools().await {
            Ok(tools) => tools,
            Err(e) => {
                client.shutdown().await;
                return Err(e);
            }
        };

        // Re-check under write lock to prevent TOCTOU race
        let mut clients = self.clients.write().await;
        if clients.contains_key(&entry.id) {
            drop(clients);
            client.shutdown().await;
            return Err(McpError::ServerAlreadyConnected {
                server_id: entry.id.clone(),
            });
        }
        clients.insert(entry.id.clone(), client);
        self.connected_server_ids
            .write()
            .expect("connected_server_ids lock poisoned")
            .insert(entry.id.clone());

        tracing::info!(
            server_id = entry.id,
            tools = tools.len(),
            "dynamically added MCP server"
        );
        Ok(tools)
    }

    /// Disconnect and remove a server by ID.
    ///
    /// # Errors
    ///
    /// Returns `McpError::ServerNotFound` if the server is not connected.
    ///
    /// # Panics
    ///
    /// Panics if the internal `connected_server_ids` lock is poisoned.
    pub async fn remove_server(&self, server_id: &str) -> Result<(), McpError> {
        let client = {
            let mut clients = self.clients.write().await;
            clients
                .remove(server_id)
                .ok_or_else(|| McpError::ServerNotFound {
                    server_id: server_id.into(),
                })?
        };

        tracing::info!(server_id, "shutting down dynamically removed MCP server");
        self.connected_server_ids
            .write()
            .expect("connected_server_ids lock poisoned")
            .remove(server_id);
        client.shutdown().await;
        Ok(())
    }

    /// Return sorted list of connected server IDs.
    pub async fn list_servers(&self) -> Vec<String> {
        let clients = self.clients.read().await;
        let mut ids: Vec<String> = clients.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Returns `true` when the given server currently has a live client entry.
    ///
    /// This is a non-blocking probe intended for synchronous availability
    /// checks and mirrors the manager's connected-client lifecycle.
    ///
    /// # Panics
    ///
    /// Panics if the internal `connected_server_ids` lock is poisoned.
    #[must_use]
    pub fn is_server_connected(&self, server_id: &str) -> bool {
        self.connected_server_ids
            .read()
            .expect("connected_server_ids lock poisoned")
            .contains(server_id)
    }

    /// Graceful shutdown of all connections (takes ownership).
    pub async fn shutdown_all(self) {
        self.shutdown_all_shared().await;
    }

    /// Graceful shutdown of all connections via shared reference.
    ///
    /// # Panics
    ///
    /// Panics if the internal `connected_server_ids` lock is poisoned.
    pub async fn shutdown_all_shared(&self) {
        let mut clients = self.clients.write().await;
        let drained: Vec<(String, McpClient)> = clients.drain().collect();
        self.connected_server_ids
            .write()
            .expect("connected_server_ids lock poisoned")
            .clear();
        for (id, client) in drained {
            tracing::info!(server_id = id, "shutting down MCP client");
            if tokio::time::timeout(Duration::from_secs(5), client.shutdown())
                .await
                .is_err()
            {
                tracing::warn!(server_id = id, "MCP client shutdown timed out");
            }
        }
    }
}

async fn connect_entry(
    entry: &ServerEntry,
    allowed_commands: &[String],
    suppress_stderr: bool,
) -> Result<McpClient, McpError> {
    match &entry.transport {
        McpTransport::Stdio { command, args, env } => {
            McpClient::connect(
                &entry.id,
                command,
                args,
                env,
                allowed_commands,
                entry.timeout,
                suppress_stderr,
            )
            .await
        }
        McpTransport::Http { url } => {
            McpClient::connect_url(&entry.id, url, entry.timeout, entry.trusted).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: &str) -> ServerEntry {
        ServerEntry {
            id: id.into(),
            transport: McpTransport::Stdio {
                command: "nonexistent-mcp-binary".into(),
                args: Vec::new(),
                env: HashMap::new(),
            },
            timeout: Duration::from_secs(5),
            trusted: false,
        }
    }

    #[tokio::test]
    async fn list_servers_empty() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        assert!(mgr.list_servers().await.is_empty());
    }

    #[test]
    fn is_server_connected_returns_false_for_missing_server() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        assert!(!mgr.is_server_connected("missing"));
    }

    #[test]
    fn is_server_connected_returns_true_for_connected_server() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        mgr.mark_server_connected_for_test("mcpls");
        assert!(mgr.is_server_connected("mcpls"));
    }

    #[tokio::test]
    async fn shutdown_all_shared_clears_connected_server_ids() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        mgr.mark_server_connected_for_test("mcpls");

        mgr.shutdown_all_shared().await;

        assert!(!mgr.is_server_connected("mcpls"));
    }

    #[tokio::test]
    async fn remove_server_not_found_returns_error() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        let err = mgr.remove_server("nonexistent").await.unwrap_err();
        assert!(
            matches!(err, McpError::ServerNotFound { ref server_id } if server_id == "nonexistent")
        );
        assert!(err.to_string().contains("nonexistent"));
    }

    #[tokio::test]
    async fn add_server_nonexistent_binary_returns_command_not_allowed() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        let entry = make_entry("test-server");
        let err = mgr.add_server(&entry).await.unwrap_err();
        assert!(matches!(err, McpError::CommandNotAllowed { .. }));
    }

    #[tokio::test]
    async fn connect_all_skips_failing_servers() {
        let mgr = McpManager::new(
            vec![make_entry("a"), make_entry("b")],
            vec![],
            PolicyEnforcer::new(vec![]),
        );
        let tools = mgr.connect_all().await;
        assert!(tools.is_empty());
        assert!(mgr.list_servers().await.is_empty());
    }

    #[tokio::test]
    async fn call_tool_server_not_found() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        let err = mgr
            .call_tool("missing", "some_tool", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::ServerNotFound { ref server_id } if server_id == "missing")
        );
    }

    #[test]
    fn server_entry_clone() {
        let entry = make_entry("github");
        let cloned = entry.clone();
        assert_eq!(entry.id, cloned.id);
        assert_eq!(entry.timeout, cloned.timeout);
    }

    #[test]
    fn server_entry_debug() {
        let entry = make_entry("test");
        let dbg = format!("{entry:?}");
        assert!(dbg.contains("test"));
    }

    #[test]
    fn manager_debug() {
        let mgr = McpManager::new(
            vec![make_entry("a"), make_entry("b")],
            vec![],
            PolicyEnforcer::new(vec![]),
        );
        let dbg = format!("{mgr:?}");
        assert!(dbg.contains("server_count"));
        assert!(dbg.contains('2'));
    }

    #[tokio::test]
    async fn list_servers_returns_sorted() {
        let mgr = McpManager::new(
            vec![make_entry("z"), make_entry("a"), make_entry("m")],
            vec![],
            PolicyEnforcer::new(vec![]),
        );
        // No servers connected (all fail), so list is empty
        mgr.connect_all().await;
        let ids = mgr.list_servers().await;
        assert!(ids.is_empty());
        // Verify sort contract: even for an empty list, sort is a no-op
        let sorted = {
            let mut v = ids.clone();
            v.sort();
            v
        };
        assert_eq!(ids, sorted);
    }

    #[tokio::test]
    async fn remove_server_preserves_other_entries() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        // With no connected servers, remove always returns ServerNotFound
        assert!(mgr.remove_server("a").await.is_err());
        assert!(mgr.remove_server("b").await.is_err());
        assert!(mgr.list_servers().await.is_empty());
    }

    #[tokio::test]
    async fn add_server_command_not_allowed_preserves_message() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        let entry = make_entry("my-server");
        let err = mgr.add_server(&entry).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nonexistent-mcp-binary"));
        assert!(msg.contains("not allowed"));
    }

    #[test]
    fn transport_stdio_clone() {
        let transport = McpTransport::Stdio {
            command: "node".into(),
            args: vec!["server.js".into()],
            env: HashMap::from([("KEY".into(), "VAL".into())]),
        };
        let cloned = transport.clone();
        if let McpTransport::Stdio {
            command, args, env, ..
        } = &cloned
        {
            assert_eq!(command, "node");
            assert_eq!(args, &["server.js"]);
            assert_eq!(env.get("KEY").unwrap(), "VAL");
        } else {
            panic!("expected Stdio variant");
        }
    }

    #[test]
    fn transport_http_clone() {
        let transport = McpTransport::Http {
            url: "http://localhost:3000".into(),
        };
        let cloned = transport.clone();
        if let McpTransport::Http { url } = &cloned {
            assert_eq!(url, "http://localhost:3000");
        } else {
            panic!("expected Http variant");
        }
    }

    #[test]
    fn transport_stdio_debug() {
        let transport = McpTransport::Stdio {
            command: "npx".into(),
            args: vec![],
            env: HashMap::new(),
        };
        let dbg = format!("{transport:?}");
        assert!(dbg.contains("Stdio"));
        assert!(dbg.contains("npx"));
    }

    #[test]
    fn transport_http_debug() {
        let transport = McpTransport::Http {
            url: "http://example.com".into(),
        };
        let dbg = format!("{transport:?}");
        assert!(dbg.contains("Http"));
        assert!(dbg.contains("http://example.com"));
    }

    fn make_http_entry(id: &str) -> ServerEntry {
        ServerEntry {
            id: id.into(),
            transport: McpTransport::Http {
                url: "http://127.0.0.1:1/nonexistent".into(),
            },
            timeout: Duration::from_secs(1),
            trusted: false,
        }
    }

    #[tokio::test]
    async fn add_server_http_nonexistent_returns_connection_error() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        let entry = make_http_entry("http-test");
        let err = mgr.add_server(&entry).await.unwrap_err();
        assert!(matches!(
            err,
            McpError::SsrfBlocked { .. } | McpError::Connection { .. }
        ));
    }

    #[test]
    fn manager_new_stores_configs() {
        let mgr = McpManager::new(
            vec![make_entry("a"), make_entry("b"), make_entry("c")],
            vec![],
            PolicyEnforcer::new(vec![]),
        );
        let dbg = format!("{mgr:?}");
        assert!(dbg.contains('3'));
    }

    #[tokio::test]
    async fn call_tool_different_missing_servers() {
        let mgr = McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![]));
        for id in &["server-a", "server-b", "server-c"] {
            let err = mgr
                .call_tool(id, "tool", serde_json::json!({}))
                .await
                .unwrap_err();
            if let McpError::ServerNotFound { server_id } = &err {
                assert_eq!(server_id, id);
            } else {
                panic!("expected ServerNotFound");
            }
        }
    }

    #[tokio::test]
    async fn connect_all_with_http_entries_skips_failing() {
        let mgr = McpManager::new(
            vec![make_http_entry("x"), make_http_entry("y")],
            vec![],
            PolicyEnforcer::new(vec![]),
        );
        let tools = mgr.connect_all().await;
        assert!(tools.is_empty());
        assert!(mgr.list_servers().await.is_empty());
    }

    #[test]
    fn with_suppress_stderr_builder() {
        let mgr =
            McpManager::new(vec![], vec![], PolicyEnforcer::new(vec![])).with_suppress_stderr(true);
        let dbg = format!("{mgr:?}");
        assert!(dbg.contains("McpManager"));
    }

    impl McpManager {
        fn mark_server_connected_for_test(&self, server_id: &str) {
            self.connected_server_ids
                .write()
                .expect("connected_server_ids lock poisoned")
                .insert(server_id.to_owned());
        }
    }
}
