// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Dynamic add/remove of servers, server queries, instructions, and shutdown.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::client::{McpClient, ToolRefreshEvent};
use crate::elicitation::ElicitationEvent;
use crate::error::McpError;
use crate::tool::McpTool;

use super::ingest::{apply_injection_penalties, ingest_tools};
use super::retry::connect_entry;
use super::{IngestConfig, McpManager, ServerEntry};

impl McpManager {
    /// Take the elicitation receiver to wire into the agent loop.
    ///
    /// May only be called once. Returns `None` if already taken.
    #[must_use]
    pub fn take_elicitation_rx(&self) -> Option<mpsc::Receiver<ElicitationEvent>> {
        self.elicitation_rx.lock().take()
    }

    /// Return the stored instructions for a connected server, if any.
    ///
    /// Instructions are captured from `ServerInfo.instructions` after the MCP handshake
    /// and truncated to `max_instructions_bytes`.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(
            name = "mcp.manager.server_instructions",
            skip(self),
            fields(server_id)
        )
    )]
    pub async fn server_instructions(&self, server_id: &str) -> Option<String> {
        self.server_instructions
            .read()
            .await
            .get(server_id)
            .cloned()
    }

    /// Subscribe to tool list change notifications.
    ///
    /// Returns a `watch::Receiver` that receives the full flattened tool list
    /// after any server's tool list is refreshed via `tools/list_changed`.
    ///
    /// The initial value is an empty `Vec`. To get the current tools after
    /// `connect_all()`, use `subscribe_tool_changes()` and then check
    /// `watch::Receiver::has_changed()` — or obtain the initial list directly
    /// from `connect_all()`'s return value.
    #[must_use]
    pub fn subscribe_tool_changes(&self) -> watch::Receiver<Vec<McpTool>> {
        self.tools_watch_tx.subscribe()
    }

    /// Returns the number of configured servers (connected or not).
    #[must_use]
    pub fn configured_server_count(&self) -> usize {
        self.configs.len()
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
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.manager.add_server", skip(self, entry), fields(server_id = %entry.id), err)
    )]
    pub async fn add_server(&self, entry: &ServerEntry) -> Result<Vec<McpTool>, McpError> {
        self.check_not_already_connected(&entry.id).await?;

        // NOTE: add_server retains single-attempt behaviour intentionally; retry policy for
        // dynamic connections requires idempotency review of probe/ingest/commit stages.
        // A follow-up issue tracks extending retry to this path.
        let tx = self
            .clone_refresh_tx()
            .ok_or_else(|| McpError::ManagerShuttingDown {
                server_id: entry.id.clone(),
            })?;

        let (client, raw_tools) = self.connect_and_list_tools(entry, tx).await?;

        if let Err(e) = self.probe_or_cleanup(entry, &client).await {
            client.shutdown().await;
            return Err(e);
        }

        self.store_server_instructions(entry, &client).await;

        let prev_fingerprints = self
            .server_fingerprints
            .read()
            .await
            .get(&entry.id)
            .cloned();
        let (tools, sanitize_result, new_fingerprints) = ingest_tools(
            raw_tools,
            &IngestConfig {
                server_id: &entry.id,
                trust_level: entry.trust_level,
                allowlist: entry.tool_allowlist.as_deref(),
                expected_tools: &entry.expected_tools,
                status_tx: self.status_tx.as_ref(),
                max_description_bytes: self.max_description_bytes,
                tool_metadata: &entry.tool_metadata,
                previous_fingerprints: prev_fingerprints.as_ref(),
            },
        );
        apply_injection_penalties(
            self.trust_store.as_ref(),
            &entry.id,
            &sanitize_result,
            &self.server_trust,
        )
        .await;

        self.commit_added_server(entry, client, tools.clone(), new_fingerprints)
            .await?;

        // Detect collisions against the full current tool list (SF-1: add_server path).
        let all_tools: Vec<McpTool> = self
            .server_tools
            .read()
            .await
            .values()
            .flatten()
            .cloned()
            .collect();
        self.log_tool_collisions(&all_tools).await;

        tracing::info!(
            server_id = entry.id,
            tools = tools.len(),
            "dynamically added MCP server"
        );
        Ok(tools)
    }

    #[tracing::instrument(name = "mcp.manager.check_not_already_connected", skip(self), fields(server_id = %server_id), err)]
    async fn check_not_already_connected(&self, server_id: &str) -> Result<(), McpError> {
        let clients = self.clients.read().await;
        if clients.contains_key(server_id) {
            return Err(McpError::ServerAlreadyConnected {
                server_id: server_id.to_owned(),
            });
        }
        Ok(())
    }

    #[tracing::instrument(name = "mcp.manager.connect_and_list_tools", skip(self), err)]
    async fn connect_and_list_tools(
        &self,
        entry: &ServerEntry,
        tx: mpsc::Sender<ToolRefreshEvent>,
    ) -> Result<(McpClient, Vec<McpTool>), McpError> {
        // MF-2: insert lock BEFORE connecting so no refresh can slip through before the lock is set.
        if self.lock_tool_list {
            self.tool_list_locked.insert(entry.id.clone(), ());
        }
        let handler_cfg = self.handler_cfg_for(entry).await;
        let client = match connect_entry(
            entry,
            &self.allowed_commands,
            self.suppress_stderr,
            tx,
            Arc::clone(&self.last_refresh),
            &handler_cfg,
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                // Remove pre-inserted lock on failure so the server can be retried.
                self.tool_list_locked.remove(&entry.id);
                return Err(e);
            }
        };
        let raw_tools = match client.list_tools().await {
            Ok(tools) => tools,
            Err(e) => {
                self.tool_list_locked.remove(&entry.id);
                client.shutdown().await;
                return Err(e);
            }
        };
        Ok((client, raw_tools))
    }

    #[tracing::instrument(name = "mcp.manager.probe_or_cleanup", skip(self), err)]
    async fn probe_or_cleanup(
        &self,
        entry: &ServerEntry,
        client: &McpClient,
    ) -> Result<(), McpError> {
        if let Err(e) = self.run_probe(&entry.id, client).await {
            self.tool_list_locked.remove(&entry.id);
            return Err(e);
        }
        Ok(())
    }

    #[tracing::instrument(name = "mcp.manager.store_server_instructions", skip(self))]
    async fn store_server_instructions(&self, entry: &ServerEntry, client: &McpClient) {
        if let Some(ref instructions) = client.server_instructions() {
            let truncated = crate::sanitize::truncate_instructions(
                instructions,
                &entry.id,
                self.max_instructions_bytes,
            );
            self.server_instructions
                .write()
                .await
                .insert(entry.id.clone(), truncated);
        }
    }

    #[tracing::instrument(name = "mcp.manager.commit_added_server", skip(self), err)]
    pub(super) async fn commit_added_server(
        &self,
        entry: &ServerEntry,
        client: McpClient,
        tools: Vec<McpTool>,
        fingerprints: Option<HashMap<String, crate::attestation::ToolFingerprint>>,
    ) -> Result<(), McpError> {
        // Serialize add/remove operations to prevent the TOCTOU race where
        // remove_server removes the client between the clients write and the
        // server_trust/server_tools writes, leaving orphaned entries.
        let add_remove_guard = self.add_remove_lock.lock().await;

        // Re-check under write lock to prevent TOCTOU race.
        {
            let mut clients = self.clients.write().await;
            if clients.contains_key(&entry.id) {
                drop(clients);
                drop(add_remove_guard);
                client.shutdown().await;
                return Err(McpError::ServerAlreadyConnected {
                    server_id: entry.id.clone(),
                });
            }
            clients.insert(entry.id.clone(), client);
        } // clients guard released here — satisfies Await Discipline §4

        self.connected_server_ids.write().insert(entry.id.clone());

        // Register trust and tools after the client is visible.
        // Each guard is acquired and dropped independently — no guard crosses an .await.
        //
        // Invariant: the refresh task cannot deliver events for entry.id before this function
        // returns Ok, because the refresh channel is wired through the client's notification
        // handler which is not active until after connect_and_list_tools completes.
        self.server_trust.write().await.insert(
            entry.id.clone(),
            (
                entry.trust_level,
                entry.tool_allowlist.clone(),
                entry.expected_tools.clone(),
            ),
        );
        self.server_tools
            .write()
            .await
            .insert(entry.id.clone(), tools);
        if let Some(fp) = fingerprints {
            self.server_fingerprints
                .write()
                .await
                .insert(entry.id.clone(), fp);
        }

        Ok(())
    }

    /// Disconnect and remove a server by ID.
    ///
    /// # Errors
    ///
    /// Returns `McpError::ServerNotFound` if the server is not connected.
    ///
    /// # Panics
    ///
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(
            name = "mcp.manager.remove_server",
            skip(self),
            fields(server_id),
            err
        )
    )]
    pub async fn remove_server(&self, server_id: &str) -> Result<(), McpError> {
        // Serialize with commit_added_server to prevent orphaned trust/tools entries.
        let add_remove_guard = self.add_remove_lock.lock().await;

        let client = {
            let mut clients = self.clients.write().await;
            clients
                .remove(server_id)
                .ok_or_else(|| McpError::ServerNotFound {
                    server_id: server_id.into(),
                })?
        };

        tracing::info!(server_id, "shutting down dynamically removed MCP server");
        self.connected_server_ids.write().remove(server_id);
        // Clean up per-server state.
        self.server_tools.write().await.remove(server_id);
        self.server_trust.write().await.remove(server_id);
        self.server_fingerprints.write().await.remove(server_id);
        self.last_refresh.remove(server_id);
        // Release the serialization lock before the potentially slow shutdown call.
        drop(add_remove_guard);
        client.shutdown().await;
        Ok(())
    }

    /// Return all non-empty server instructions, concatenated with double newlines.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.manager.all_server_instructions", skip_all)
    )]
    pub async fn all_server_instructions(&self) -> String {
        let map = self.server_instructions.read().await;
        let mut parts: Vec<&str> = map.values().map(String::as_str).collect();
        parts.sort_unstable();
        parts.join("\n\n")
    }

    /// Return sorted list of connected server IDs.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.manager.list_servers", skip_all)
    )]
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
    #[must_use]
    pub fn is_server_connected(&self, server_id: &str) -> bool {
        self.connected_server_ids.read().contains(server_id)
    }

    /// Graceful shutdown of all connections (takes ownership).
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.manager.shutdown_all", skip_all)
    )]
    pub async fn shutdown_all(self) {
        self.shutdown_all_shared().await;
    }

    /// Graceful shutdown of all connections via shared reference.
    ///
    /// Drops the manager's `refresh_tx` sender. Once all connected clients are shut down
    /// (dropping their handler senders too), the refresh task terminates naturally.
    ///
    /// # Panics
    ///
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.manager.shutdown_all_shared", skip_all)
    )]
    pub async fn shutdown_all_shared(&self) {
        // Signal all in-flight startup retry tasks to stop sleeping and exit immediately.
        // Must be the very first action so that retry sleeps are interrupted before we
        // begin draining clients and dropping senders.
        self.shutdown_token.cancel();

        // Drop the manager's sender so the refresh task can terminate once
        // all ToolListChangedHandler senders are also dropped (via client shutdown).
        let _ = self.refresh_tx.lock().take();

        let mut clients = self.clients.write().await;
        let drained: Vec<(String, McpClient)> = clients.drain().collect();
        self.connected_server_ids.write().clear();
        self.server_tools.write().await.clear();
        self.server_fingerprints.write().await.clear();
        self.last_refresh.clear();
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
