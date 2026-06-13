// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Server connection establishment, OAuth handshakes, probing, and the
//! background tool-refresh task.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use rmcp::transport::auth::CredentialStore;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::client::{McpClient, OAuthConnectResult, ToolRefreshEvent};
use crate::elicitation::ElicitationEvent;
use crate::error::McpError;
use crate::tool::McpTool;

use super::ingest::{apply_injection_penalties, ingest_tools};
use super::retry::connect_with_retry;
use super::{
    ConnectOutput, IngestConfig, IngestLimits, McpManager, McpTransport, McpTrustLevel,
    ServerConnectOutcome, ServerEntry, StatusTx,
};

impl McpManager {
    /// Clone the refresh sender for use in `ToolListChangedHandler`.
    ///
    /// Returns `None` if the manager has already been shut down.
    pub(super) fn clone_refresh_tx(&self) -> Option<mpsc::Sender<ToolRefreshEvent>> {
        self.refresh_tx.lock().as_ref().cloned()
    }

    /// Clone the elicitation sender for a specific server, if elicitation is enabled for it.
    ///
    /// Returns `None` if elicitation is disabled for this server, the server is Sandboxed
    /// (never allowed to elicit), or the manager has shut down.
    pub(super) fn clone_elicitation_tx_for(
        &self,
        server_id: &str,
        trust_level: McpTrustLevel,
    ) -> Option<mpsc::Sender<ElicitationEvent>> {
        // Sandboxed servers may never elicit regardless of config.
        if trust_level == McpTrustLevel::Sandboxed {
            return None;
        }
        let enabled = self
            .server_elicitation
            .get(server_id)
            .copied()
            .unwrap_or(false);
        if !enabled {
            return None;
        }
        self.elicitation_tx.lock().as_ref().cloned()
    }

    /// Elicitation timeout for a specific server.
    fn elicitation_timeout_for(&self, server_id: &str) -> std::time::Duration {
        let secs = self
            .server_elicitation_timeout
            .get(server_id)
            .copied()
            .unwrap_or(120);
        std::time::Duration::from_secs(secs)
    }

    #[tracing::instrument(name = "mcp.manager.handler_cfg_for", skip_all)]
    pub(super) async fn handler_cfg_for(
        &self,
        entry: &ServerEntry,
    ) -> crate::client::HandlerConfig {
        let roots = Arc::new(validate_roots(&entry.roots, &entry.id).await);
        crate::client::HandlerConfig {
            roots,
            max_description_bytes: self.max_description_bytes,
            elicitation_tx: self.clone_elicitation_tx_for(&entry.id, entry.trust_level),
            elicitation_timeout: self.elicitation_timeout_for(&entry.id),
        }
    }

    /// Spawn the background refresh task that processes `tools/list_changed` events.
    ///
    /// Must be called once, after `connect_all()`. The task terminates automatically
    /// when all senders are dropped (i.e., after `shutdown_all_shared()` drops `refresh_tx`
    /// and all connected clients are shut down).
    ///
    /// When `supervisor` is `Some`, the task is registered under `"mcp.refresh_task"` and
    /// participates in graceful shutdown via `shutdown_all()`. When `None` (test contexts),
    /// a raw `tokio::spawn` is used.
    ///
    /// # Panics
    ///
    /// Panics if the refresh receiver has already been taken (i.e., this method is called twice).
    pub fn spawn_refresh_task(&self, supervisor: Option<&zeph_common::TaskSupervisor>) {
        let rx = self
            .refresh_rx
            .lock()
            .take()
            .expect("spawn_refresh_task must only be called once");

        let server_tools = Arc::clone(&self.server_tools);
        let tools_watch_tx = self.tools_watch_tx.clone();
        let server_trust = Arc::clone(&self.server_trust);
        let status_tx = self.status_tx.clone();
        let max_description_bytes = self.max_description_bytes;
        let trust_store = self.trust_store.clone();
        let server_tool_metadata = Arc::clone(&self.server_tool_metadata);
        let lock_tool_list = self.lock_tool_list;
        let tool_list_locked = Arc::clone(&self.tool_list_locked);

        let task = async move {
            let mut rx = rx;
            while let Some(event) = rx.recv().await {
                // MF-2: reject refresh for locked servers before any processing.
                if lock_tool_list && tool_list_locked.contains_key(&event.server_id) {
                    tracing::warn!(
                        server_id = event.server_id,
                        "tools/list_changed rejected: tool list is locked after initial connect"
                    );
                    continue;
                }
                let (filtered, sanitize_result) = {
                    let trust_guard = server_trust.read().await;
                    let (trust_level, allowlist, expected_tools) =
                        trust_guard.get(&event.server_id).map_or(
                            (McpTrustLevel::Untrusted, None, Vec::new()),
                            |(tl, al, et)| (*tl, al.clone(), et.clone()),
                        );
                    let empty = HashMap::new();
                    let tool_metadata =
                        server_tool_metadata.get(&event.server_id).unwrap_or(&empty);
                    ingest_tools(
                        event.tools,
                        &IngestConfig {
                            server_id: &event.server_id,
                            trust_level,
                            allowlist: allowlist.as_deref(),
                            expected_tools: &expected_tools,
                            status_tx: status_tx.as_ref(),
                            max_description_bytes,
                            tool_metadata,
                        },
                    )
                };
                apply_injection_penalties(
                    trust_store.as_ref(),
                    &event.server_id,
                    &sanitize_result,
                    &server_trust,
                )
                .await;
                let all_tools = {
                    let mut guard = server_tools.write().await;
                    guard.insert(event.server_id.clone(), filtered);
                    guard.values().flatten().cloned().collect::<Vec<_>>()
                };
                tracing::info!(
                    server_id = event.server_id,
                    total_tools = all_tools.len(),
                    "tools/list_changed: tool list refreshed"
                );
                // Ignore send error — no subscribers is not a problem.
                let _ = tools_watch_tx.send(all_tools);
            }
            tracing::debug!("MCP refresh task terminated: channel closed");
        };

        if let Some(sup) = supervisor {
            let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(task)));
            // spawn() requires Fn; wrap the FnOnce payload in Arc<Mutex<Option>> so the
            // factory can be called once for RunOnce without capturing by move.
            sup.spawn(zeph_common::TaskDescriptor {
                name: "mcp.refresh_task",
                restart: zeph_common::RestartPolicy::RunOnce,
                factory: move || {
                    let fut = cell.lock().take();
                    async move {
                        if let Some(f) = fut {
                            f.await;
                        }
                    }
                },
            });
        } else {
            tokio::spawn(task); // EXEMPT(test): no supervisor available in unit-test context
        }
    }

    /// Connect to all non-OAuth configured servers concurrently.
    ///
    /// Returns `(all_tools, outcomes)` where `all_tools` is the flattened set of tools
    /// from all successfully connected servers, and `outcomes` contains one
    /// [`ServerConnectOutcome`] per configured server.
    ///
    /// **OAuth servers are skipped** — call [`connect_oauth_deferred`](Self::connect_oauth_deferred)
    /// after the UI channel is ready so the authorization URL is visible and startup is not blocked.
    ///
    /// Each connection goes through the full security pipeline:
    /// command validation → SSRF check → handshake → probe → attestation → sanitization →
    /// data-flow policy.
    ///
    /// # Panics
    ///
    /// Does not panic under normal conditions.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.manager.connect_all", skip_all, fields(connected = tracing::field::Empty, failed = tracing::field::Empty))
    )]
    pub async fn connect_all(&self) -> (Vec<McpTool>, Vec<ServerConnectOutcome>) {
        let join_set = self.spawn_non_oauth_connections(&self.last_refresh).await;
        let raw = drain_connect_results(join_set).await;
        let limits = IngestLimits {
            description_bytes: self.max_description_bytes,
            instructions_bytes: self.max_instructions_bytes,
        };
        let outputs = self.process_connect_results(raw, limits).await;
        let (all_tools, outcomes) = self.commit_connect_outputs(outputs).await;
        self.log_tool_collisions(&all_tools).await;
        (all_tools, outcomes)
    }

    #[tracing::instrument(name = "mcp.manager.spawn_non_oauth_connections", skip_all)]
    async fn spawn_non_oauth_connections(
        &self,
        last_refresh: &Arc<DashMap<String, Instant>>,
    ) -> JoinSet<(String, Result<McpClient, McpError>)> {
        let allowed = self.allowed_commands.clone();
        let suppress = self.suppress_stderr;
        let cloned_status_tx = self.status_tx.clone();
        let max_attempts = self.max_connect_attempts;
        let retry_backoff_base_ms = self.startup_retry_backoff_ms;

        let non_oauth: Vec<_> = self
            .configs
            .iter()
            .filter(|&c| !matches!(c.transport, McpTransport::OAuth { .. }))
            .cloned()
            .collect();

        let mut join_set = JoinSet::new();
        for config in non_oauth {
            let allowed = allowed.clone();
            let last_refresh = Arc::clone(last_refresh);
            let Some(tx) = self.clone_refresh_tx() else {
                continue;
            };
            let handler_cfg = self.handler_cfg_for(&config).await;
            // MF-2: register the lock BEFORE spawning the connection task so there is no
            // window between connect handshake completion and lock insertion.
            // The lock entry is removed inside handle_connect_result if connection fails.
            if self.lock_tool_list {
                self.tool_list_locked.insert(config.id.clone(), ());
            }
            let status_tx = cloned_status_tx.clone();
            let shutdown = self.shutdown_token.clone();
            join_set.spawn(async move {
                let result = connect_with_retry(
                    &config,
                    &allowed,
                    suppress,
                    tx,
                    last_refresh,
                    &handler_cfg,
                    max_attempts,
                    retry_backoff_base_ms,
                    status_tx.as_ref(),
                    &shutdown,
                )
                .await;
                (config.id, result)
            });
        }
        join_set
    }

    #[tracing::instrument(name = "mcp.manager.process_connect_results", skip_all)]
    async fn process_connect_results(
        &self,
        raw: Vec<(String, Result<McpClient, McpError>)>,
        limits: IngestLimits,
    ) -> Vec<ConnectOutput> {
        let mut outputs = Vec::with_capacity(raw.len());
        for (server_id, connect_result) in raw {
            outputs.push(
                self.handle_connect_result(server_id, connect_result, limits)
                    .await,
            );
        }
        outputs
    }

    #[tracing::instrument(name = "mcp.manager.commit_connect_outputs", skip_all)]
    async fn commit_connect_outputs(
        &self,
        outputs: Vec<ConnectOutput>,
    ) -> (Vec<McpTool>, Vec<ServerConnectOutcome>) {
        // All async work is done. Collect into vecs first, then commit each lock
        // in its own guarded block — never hold one lock across another .await.
        let mut pending_instructions: Vec<(String, String)> = Vec::new();
        let mut pending_clients: Vec<(String, _)> = Vec::new();
        let mut pending_tools: Vec<(String, _)> = Vec::new();
        let mut all_tools = Vec::new();
        let mut outcomes: Vec<ServerConnectOutcome> = Vec::new();
        for output in outputs {
            if let Some((sid, instr)) = output.instructions {
                pending_instructions.push((sid, instr));
            }
            if let Some((sid, client)) = output.client_entry {
                pending_clients.push((sid, client));
            }
            if let Some((sid, tools)) = output.tools_entry {
                pending_tools.push((sid, tools));
            }
            all_tools.extend(output.tools);
            outcomes.push(output.outcome);
        }
        {
            let mut g = self.server_instructions.write().await;
            for (sid, instr) in pending_instructions {
                g.insert(sid, instr);
            }
        }
        {
            let mut g = self.clients.write().await;
            for (sid, client) in pending_clients {
                g.insert(sid, client);
            }
        }
        {
            let mut g = self.server_tools.write().await;
            for (sid, tools) in pending_tools {
                g.insert(sid, tools);
            }
        }
        (all_tools, outcomes)
    }

    /// Returns `true` if any configured server uses OAuth transport.
    #[must_use]
    pub fn has_oauth_servers(&self) -> bool {
        self.configs
            .iter()
            .any(|c| matches!(c.transport, McpTransport::OAuth { .. }))
    }

    /// Connect OAuth servers in the background.
    ///
    /// Must be called after the UI channel is running so that auth URLs are
    /// visible to the user. For each server requiring authorization, the
    /// browser is opened automatically and the callback is awaited (up to 300 s).
    /// All OAuth handshakes run concurrently via `JoinSet`. Discovered tools are
    /// published via `tools_watch_tx` after all connections complete.
    ///
    /// # Panics
    ///
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "mcp.manager.connect_oauth_deferred", skip_all)
    )]
    pub async fn connect_oauth_deferred(&self) {
        let limits = IngestLimits {
            description_bytes: self.max_description_bytes,
            instructions_bytes: self.max_instructions_bytes,
        };
        let join_set = self.spawn_oauth_connections(&self.last_refresh).await;
        let raw = drain_oauth_results(join_set).await;
        let outputs = self.process_oauth_results(raw, limits).await;
        let all_tools: Vec<McpTool> = outputs
            .iter()
            .flat_map(|o| o.tools.iter().cloned())
            .collect();
        self.commit_oauth_outputs(outputs).await;
        self.log_tool_collisions(&all_tools).await;
    }

    #[tracing::instrument(name = "mcp.manager.spawn_oauth_connections", skip_all)]
    async fn spawn_oauth_connections(
        &self,
        last_refresh: &Arc<DashMap<String, Instant>>,
    ) -> JoinSet<(String, Result<McpClient, String>)> {
        let oauth_configs: Vec<_> = self
            .configs
            .iter()
            .filter(|&c| matches!(c.transport, McpTransport::OAuth { .. }))
            .cloned()
            .collect();

        // Spawn one task per OAuth server. Each task runs the full OAuth flow
        // (initial connect + optional browser callback) and returns the connected
        // client or a pre-formatted error string. No &self reference crosses the
        // task boundary — only cloned/Arc values are captured.
        let mut join_set: JoinSet<(String, Result<McpClient, String>)> = JoinSet::new();

        for config in oauth_configs {
            let McpTransport::OAuth {
                ref url,
                ref scopes,
                callback_port,
                ref client_name,
            } = config.transport
            else {
                continue;
            };

            let Some(credential_store_ref) = self.oauth_credentials.get(&config.id) else {
                tracing::warn!(
                    server_id = config.id,
                    "OAuth server has no credential store registered — skipping"
                );
                continue;
            };
            let credential_store = Arc::clone(credential_store_ref);

            let Some(tx) = self.clone_refresh_tx() else {
                continue;
            };

            let url = url.clone();
            let scopes = scopes.clone();
            let client_name = client_name.clone();
            let server_id = config.id.clone();
            let trusted = matches!(config.trust_level, McpTrustLevel::Trusted);
            let timeout = config.timeout;
            let handler_cfg = self.handler_cfg_for(&config).await;
            let status_tx = self.status_tx.clone();
            let last_refresh = Arc::clone(last_refresh);

            join_set.spawn(run_oauth_handshake(
                server_id,
                url,
                scopes,
                callback_port,
                client_name,
                credential_store,
                trusted,
                tx,
                last_refresh,
                timeout,
                handler_cfg,
                status_tx,
            ));
        }
        join_set
    }

    #[tracing::instrument(name = "mcp.manager.process_oauth_results", skip_all)]
    async fn process_oauth_results(
        &self,
        raw: Vec<(String, Result<McpClient, String>)>,
        limits: IngestLimits,
    ) -> Vec<ConnectOutput> {
        // Process each result through handle_connect_result (no locks held).
        let mut outputs = Vec::with_capacity(raw.len());
        for (server_id, client_result) in raw {
            match client_result {
                Ok(client) => {
                    outputs.push(
                        self.handle_connect_result(server_id, Ok(client), limits)
                            .await,
                    );
                }
                Err(error) => {
                    outputs.push(ConnectOutput {
                        client_entry: None,
                        tools_entry: None,
                        tools: Vec::new(),
                        outcome: ServerConnectOutcome {
                            id: server_id,
                            connected: false,
                            tool_count: 0,
                            error,
                        },
                        instructions: None,
                    });
                }
            }
        }
        outputs
    }

    #[tracing::instrument(name = "mcp.manager.commit_oauth_outputs", skip_all)]
    async fn commit_oauth_outputs(&self, outputs: Vec<ConnectOutput>) {
        // Batch-commit to shared maps in separate guarded blocks — never hold one
        // lock across another .await (same pattern as connect_all).
        let mut pending_instructions: Vec<(String, String)> = Vec::new();
        let mut pending_clients: Vec<(String, McpClient)> = Vec::new();
        let mut pending_tools: Vec<(String, Vec<McpTool>)> = Vec::new();
        for output in outputs {
            if let Some((sid, instr)) = output.instructions {
                pending_instructions.push((sid, instr));
            }
            if let Some((sid, client)) = output.client_entry {
                pending_clients.push((sid, client));
            }
            if let Some((sid, tools)) = output.tools_entry {
                pending_tools.push((sid, tools));
            }
        }
        {
            let mut g = self.server_instructions.write().await;
            for (sid, instr) in pending_instructions {
                g.insert(sid, instr);
            }
        }
        {
            let mut g = self.clients.write().await;
            for (sid, client) in pending_clients {
                g.insert(sid, client);
            }
        }
        let updated = {
            let mut g = self.server_tools.write().await;
            for (sid, tools) in pending_tools {
                g.insert(sid, tools);
            }
            g.values().flatten().cloned().collect::<Vec<McpTool>>()
        };
        if !updated.is_empty() {
            let _ = self.tools_watch_tx.send(updated);
        }
    }

    /// Log warnings for all `sanitized_id` collisions in `tools`.
    ///
    /// When trust levels differ, the lower-trust tool is shadowed — its `sanitized_id` is
    /// claimed by a higher-trust tool. When trust levels are equal, the first-registered
    /// tool wins dispatch. Either way the collision is a misconfiguration and must be logged
    /// so the operator can disambiguate (MF-1 / SF-6 fix).
    #[tracing::instrument(name = "mcp.manager.log_tool_collisions", skip_all)]
    pub(super) async fn log_tool_collisions(&self, tools: &[McpTool]) {
        use crate::tool::detect_collisions;

        let trust_guard = self.server_trust.read().await;
        let trust_map: std::collections::HashMap<String, McpTrustLevel> = trust_guard
            .iter()
            .map(|(id, (tl, _, _))| (id.clone(), *tl))
            .collect();
        drop(trust_guard);

        for col in detect_collisions(tools, &trust_map) {
            tracing::warn!(
                sanitized_id = %col.sanitized_id,
                server_a = %col.server_a,
                qualified_a = %col.qualified_a,
                trust_a = ?col.trust_a,
                server_b = %col.server_b,
                qualified_b = %col.qualified_b,
                trust_b = ?col.trust_b,
                "MCP tool sanitized_id collision: '{}' shadows '{}' — executor will always dispatch to the first-registered tool",
                col.qualified_a, col.qualified_b,
            );
        }
    }

    /// Process a single server connection result without holding any shared write locks.
    ///
    /// Returns a [`ConnectOutput`] with all owned data the caller must commit to the
    /// shared maps. The caller is responsible for inserting this data under a write
    /// guard after all async work completes.
    #[tracing::instrument(name = "mcp.manager.handle_connect_result", skip_all, fields(server_id = %server_id))]
    async fn handle_connect_result(
        &self,
        server_id: String,
        connect_result: Result<McpClient, McpError>,
        limits: IngestLimits,
    ) -> ConnectOutput {
        let fail = |error: String| ConnectOutput {
            client_entry: None,
            tools_entry: None,
            tools: Vec::new(),
            instructions: None,
            outcome: ServerConnectOutcome {
                id: server_id.clone(),
                connected: false,
                tool_count: 0,
                error,
            },
        };

        match connect_result {
            Ok(client) => match client.list_tools().await {
                Ok(raw_tools) => {
                    // Phase 1: run pre-connect probe if configured.
                    if let Err(e) = self.run_probe(&server_id, &client).await {
                        client.shutdown().await;
                        return fail(format!("{e:#}"));
                    }

                    // Capture server instructions from handshake and apply cap.
                    let instructions = client.server_instructions().as_ref().map(|instr| {
                        let truncated = crate::sanitize::truncate_instructions(
                            instr,
                            &server_id,
                            limits.instructions_bytes,
                        );
                        (server_id.clone(), truncated)
                    });

                    let (trust_level, allowlist, expected_tools) =
                        self.server_trust.read().await.get(&server_id).map_or(
                            (McpTrustLevel::Untrusted, None, Vec::new()),
                            |(tl, al, et)| (*tl, al.clone(), et.clone()),
                        );
                    let empty = HashMap::new();
                    let tool_metadata = self.server_tool_metadata.get(&server_id).unwrap_or(&empty);
                    let (tools, sanitize_result) = ingest_tools(
                        raw_tools,
                        &IngestConfig {
                            server_id: &server_id,
                            trust_level,
                            allowlist: allowlist.as_deref(),
                            expected_tools: &expected_tools,
                            status_tx: self.status_tx.as_ref(),
                            max_description_bytes: limits.description_bytes,
                            tool_metadata,
                        },
                    );
                    apply_injection_penalties(
                        self.trust_store.as_ref(),
                        &server_id,
                        &sanitize_result,
                        &self.server_trust,
                    )
                    .await;
                    tracing::info!(server_id, tools = tools.len(), "connected to MCP server");
                    let tool_count = tools.len();
                    self.connected_server_ids.write().insert(server_id.clone());
                    ConnectOutput {
                        client_entry: Some((server_id.clone(), client)),
                        tools_entry: Some((server_id.clone(), tools.clone())),
                        tools,
                        instructions,
                        outcome: ServerConnectOutcome {
                            id: server_id,
                            connected: true,
                            tool_count,
                            error: String::new(),
                        },
                    }
                }
                Err(e) => {
                    tracing::warn!(server_id, "failed to list tools: {e:#}");
                    // Connection failed — remove lock so the server is not left permanently locked.
                    self.tool_list_locked.remove(&server_id);
                    fail(format!("{e:#}"))
                }
            },
            Err(e) => {
                tracing::warn!(server_id, "MCP server connection failed: {e:#}");
                // Connection failed — remove lock so the server is not left permanently locked.
                self.tool_list_locked.remove(&server_id);
                fail(format!("{e:#}"))
            }
        }
    }

    /// Run the pre-connect probe for `server_id` against `client`.
    ///
    /// Returns `Ok(())` if the probe passes or no prober is configured.
    /// Returns `Err` and calls `client.shutdown()` if the probe blocks the server.
    #[tracing::instrument(name = "mcp.manager.run_probe", skip_all, fields(server_id = %server_id), err)]
    pub(super) async fn run_probe(
        &self,
        server_id: &str,
        client: &McpClient,
    ) -> Result<(), McpError> {
        let Some(ref prober) = self.prober else {
            return Ok(());
        };
        let probe = prober.probe(server_id, client).await;
        tracing::info!(
            server_id,
            score_delta = probe.score_delta,
            block = probe.block,
            summary = probe.summary,
            "MCP pre-connect probe complete"
        );
        if let Some(ref store) = self.trust_store {
            let _ = store
                .load_and_apply_delta(server_id, probe.score_delta, 0, u64::from(probe.block))
                .await;
        }
        if probe.block {
            return Err(McpError::Connection {
                server_id: server_id.into(),
                message: format!("blocked by pre-connect probe: {}", probe.summary),
            });
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "mcp.manager.run_oauth_handshake", skip_all, fields(server_id = %server_id))]
async fn run_oauth_handshake(
    server_id: String,
    url: String,
    scopes: Vec<String>,
    callback_port: u16,
    client_name: String,
    credential_store: Arc<dyn CredentialStore>,
    trusted: bool,
    tx: mpsc::Sender<ToolRefreshEvent>,
    last_refresh: Arc<DashMap<String, Instant>>,
    timeout: Duration,
    handler_cfg: crate::client::HandlerConfig,
    status_tx: Option<StatusTx>,
) -> (String, Result<McpClient, String>) {
    let connect_result = McpClient::connect_url_oauth(
        &server_id,
        &url,
        &scopes,
        callback_port,
        &client_name,
        credential_store,
        trusted,
        tx,
        last_refresh,
        timeout,
        handler_cfg,
    )
    .await;

    let client_result = match connect_result {
        Ok(OAuthConnectResult::Connected(client)) => Ok(client),
        Ok(OAuthConnectResult::AuthorizationRequired(pending_box)) => {
            let mut pending = *pending_box;
            tracing::info!(
                server_id,
                auth_url = pending.auth_url,
                callback_port = pending.actual_port,
                "OAuth authorization required — open this URL to authorize"
            );
            let auth_msg = format!(
                "MCP OAuth: Open this URL to authorize '{}': {}",
                server_id, pending.auth_url
            );
            if let Some(ref stx) = status_tx {
                let _ = stx.send(format!("Waiting for OAuth: {server_id}"));
                let _ = stx.send(auth_msg.clone());
            } else {
                eprintln!("{auth_msg}");
            }
            // open::that_in_background spawns an OS thread; ignore the handle —
            // we don't need to wait for the browser to open.
            let _ = open::that_in_background(pending.auth_url.clone());

            let callback_timeout = std::time::Duration::from_mins(5);
            let listener = pending
                .listener
                .take()
                .expect("listener always set by connect_url_oauth");
            match crate::oauth::await_oauth_callback(listener, callback_timeout, &server_id).await {
                Ok((code, csrf_token)) => {
                    if let Some(ref stx) = status_tx {
                        let _ = stx.send(String::new());
                    }
                    McpClient::complete_oauth(pending, &code, &csrf_token)
                        .await
                        .map_err(|e| format!("OAuth token exchange failed: {e:#}"))
                }
                Err(e) => {
                    if let Some(ref stx) = status_tx {
                        let _ = stx.send(String::new());
                    }
                    tracing::warn!(server_id, "OAuth callback failed: {e:#}");
                    Err(format!("OAuth callback failed: {e:#}"))
                }
            }
        }
        Err(e) => {
            tracing::warn!(server_id, "OAuth connection failed: {e:#}");
            Err(format!("{e:#}"))
        }
    };

    (server_id, client_result)
}

#[tracing::instrument(name = "mcp.manager.drain_connect_results", skip_all)]
async fn drain_connect_results(
    mut join_set: JoinSet<(String, Result<McpClient, McpError>)>,
) -> Vec<(String, Result<McpClient, McpError>)> {
    // Drain join_set without holding any locks, then process each result through
    // handle_connect_result — which also holds no locks. All async work (network
    // calls, probing, lock-free reads) happens here with zero contention on the
    // shared maps.
    let mut raw_results = Vec::new();
    while let Some(result) = join_set.join_next().await {
        let Ok((server_id, connect_result)) = result else {
            tracing::warn!("MCP connection task panicked");
            continue;
        };
        raw_results.push((server_id, connect_result));
    }
    raw_results
}

#[tracing::instrument(name = "mcp.manager.drain_oauth_results", skip_all)]
async fn drain_oauth_results(
    mut join_set: JoinSet<(String, Result<McpClient, String>)>,
) -> Vec<(String, Result<McpClient, String>)> {
    let mut raw_results: Vec<(String, Result<McpClient, String>)> =
        Vec::with_capacity(join_set.len());
    while let Some(res) = join_set.join_next().await {
        if let Ok(item) = res {
            raw_results.push(item);
        } else {
            tracing::warn!("MCP OAuth connection task panicked");
        }
    }
    raw_results
}

/// Validate root URIs at connection time.
///
/// - Warns if a URI does not use `file://` scheme.
/// - Warns if the path does not exist on the filesystem.
/// - Filters out roots with non-`file://` URIs (MCP spec requires filesystem roots).
#[tracing::instrument(name = "mcp.manager.validate_roots", skip_all, fields(server_id = %server_id))]
pub(super) async fn validate_roots(
    roots: &[rmcp::model::Root],
    server_id: &str,
) -> Vec<rmcp::model::Root> {
    let server_id = server_id.to_owned();
    let mut result = Vec::with_capacity(roots.len());
    for r in roots {
        if !r.uri.starts_with("file://") {
            tracing::warn!(
                server_id,
                uri = r.uri,
                "MCP root URI does not use file:// scheme — skipping"
            );
            continue;
        }
        let raw_path = r.uri.trim_start_matches("file://");
        if let Ok(canonical) = tokio::fs::canonicalize(raw_path).await {
            let canonical_uri = format!("file://{}", canonical.display());
            let mut root = rmcp::model::Root::new(canonical_uri);
            if let Some(ref name) = r.name {
                root = root.with_name(name.clone());
            }
            result.push(root);
        } else {
            tracing::warn!(
                server_id,
                uri = r.uri,
                "MCP root path does not exist on filesystem"
            );
            result.push(r.clone());
        }
    }
    result
}
