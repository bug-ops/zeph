// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use rmcp::model::{ElicitResult, ElicitationAction};

use super::{Agent, Channel, LlmProvider};

impl<C: Channel> Agent<C> {
    /// Dispatch a `/mcp` subcommand, returning the output as a `String`.
    ///
    /// All output is collected into the returned string; no channel sends are
    /// performed.  This makes the future `Send`-compatible for use in
    /// `AgentAccess::handle_mcp`.
    #[tracing::instrument(skip_all, name = "core.agent.handle_mcp_command")]
    pub(super) async fn handle_mcp_command(
        &mut self,
        args: &str,
    ) -> Result<String, super::error::AgentError> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        match parts.first().copied() {
            Some("add") => self.handle_mcp_add(&parts[1..]).await,
            Some("list") => self.handle_mcp_list().await,
            Some("tools") => Ok(self.handle_mcp_tools(parts.get(1).copied())),
            Some("remove") => self.handle_mcp_remove(parts.get(1).copied()).await,
            _ => Ok("Usage: /mcp add|list|tools|remove".to_owned()),
        }
    }

    async fn handle_mcp_add(&mut self, args: &[&str]) -> Result<String, super::error::AgentError> {
        if args.len() < 2 {
            return Ok("Usage: /mcp add <id> <command> [args...] | /mcp add <id> <url>".to_owned());
        }

        // Clone the Arc so no borrow of self.services.mcp.manager is held across .await.
        let Some(manager) = self.services.mcp.manager.clone() else {
            return Ok("MCP is not enabled.".to_owned());
        };

        let target = args[1];
        if let Some(err) = validate_mcp_command(target, &self.services.mcp.allowed_commands) {
            return Ok(err);
        }

        // SEC-MCP-03: enforce server limit
        let current_count = manager.list_servers().await.len();
        if current_count >= self.services.mcp.max_dynamic {
            return Ok(format!(
                "Server limit reached ({}/{}).",
                current_count, self.services.mcp.max_dynamic
            ));
        }

        let entry = build_server_entry(args[0], target, &args[2..]);

        match manager.add_server(&entry).await {
            Ok(tools) => {
                let count = tools.len();
                self.services
                    .mcp
                    .server_outcomes
                    .push(zeph_mcp::ServerConnectOutcome {
                        id: entry.id.clone(),
                        connected: true,
                        tool_count: count,
                        error: String::new(),
                        // `McpManager::add_server` doesn't surface sanitizer schema-drop
                        // counts to its caller today — dynamic add is a pre-existing gap in
                        // this metric, not a regression from this fix.
                        input_schemas_dropped: 0,
                        output_schemas_dropped: 0,
                    });
                self.services.mcp.tools.extend(tools);
                self.services.mcp.sync_executor_tools();
                self.services.mcp.pruning_cache.reset();
                // Defer rebuild to check_tool_refresh (next turn) so this method
                // stays Send-compatible for use in AgentAccess::handle_mcp.
                self.services.mcp.pending_semantic_rebuild = true;
                self.update_mcp_metrics();
                Ok(format!(
                    "Connected MCP server '{}' ({count} tool(s))",
                    entry.id
                ))
            }
            Err(e) => {
                tracing::warn!(server_id = entry.id, "MCP add failed: {e:#}");
                Ok(format!("Failed to connect server '{}': {e}", entry.id))
            }
        }
    }

    async fn handle_mcp_list(&mut self) -> Result<String, super::error::AgentError> {
        use std::fmt::Write;

        let Some(manager) = self.services.mcp.manager.clone() else {
            return Ok("MCP is not enabled.".to_owned());
        };

        let server_ids = manager.list_servers().await;
        if server_ids.is_empty() {
            return Ok("No MCP servers connected.".to_owned());
        }

        let mut output = String::from("Connected MCP servers:\n");
        let mut total = 0usize;
        for id in &server_ids {
            let count = self
                .services
                .mcp
                .tools
                .iter()
                .filter(|t| t.server_id == *id)
                .count();
            total += count;
            let _ = writeln!(output, "- {id} ({count} tools)");
        }
        let _ = write!(output, "Total: {total} tool(s)");

        Ok(output)
    }

    fn handle_mcp_tools(&mut self, server_id: Option<&str>) -> String {
        use std::fmt::Write;

        let Some(server_id) = server_id else {
            return "Usage: /mcp tools <server_id>".to_owned();
        };

        let tools: Vec<_> = self
            .services
            .mcp
            .tools
            .iter()
            .filter(|t| t.server_id == server_id)
            .collect();

        if tools.is_empty() {
            return format!("No tools found for server '{server_id}'.");
        }

        let mut output = format!("Tools for '{server_id}' ({} total):\n", tools.len());
        for t in &tools {
            if t.description.is_empty() {
                let _ = writeln!(output, "- {}", t.name);
            } else {
                let _ = writeln!(output, "- {} — {}", t.name, t.description);
            }
        }
        output
    }

    async fn handle_mcp_remove(
        &mut self,
        server_id: Option<&str>,
    ) -> Result<String, super::error::AgentError> {
        let Some(server_id) = server_id else {
            return Ok("Usage: /mcp remove <id>".to_owned());
        };

        // Clone the Arc so no borrow of self.services.mcp.manager is held across .await.
        let Some(manager) = self.services.mcp.manager.clone() else {
            return Ok("MCP is not enabled.".to_owned());
        };

        match manager.remove_server(server_id).await {
            Ok(()) => {
                let before = self.services.mcp.tools.len();
                self.services.mcp.tools.retain(|t| t.server_id != server_id);
                let removed = before - self.services.mcp.tools.len();
                self.services
                    .mcp
                    .server_outcomes
                    .retain(|o| o.id != server_id);
                self.services.mcp.sync_executor_tools();
                self.services.mcp.pruning_cache.reset();
                // Defer rebuild to check_tool_refresh (next turn) so this method
                // stays Send-compatible for use in AgentAccess::handle_mcp.
                self.services.mcp.pending_semantic_rebuild = true;
                self.update_mcp_metrics();
                let sid = server_id.to_owned();
                self.update_metrics(|m| {
                    m.active_mcp_tools
                        .retain(|name| !name.starts_with(&format!("{sid}:")));
                });
                Ok(format!(
                    "Disconnected MCP server '{server_id}' (removed {removed} tools)"
                ))
            }
            Err(e) => {
                tracing::warn!(server_id, "MCP remove failed: {e:#}");
                Ok(format!("Failed to remove server '{server_id}': {e}"))
            }
        }
    }

    pub(super) async fn append_mcp_prompt(&mut self, query: &str, system_prompt: &mut String) {
        let matched_tools = self.match_mcp_tools(query).await;
        let active_mcp: Vec<String> = matched_tools
            .iter()
            .map(zeph_mcp::McpTool::qualified_name)
            .collect();
        let mcp_total = self.services.mcp.tools.len();
        let (mcp_server_count, mcp_connected_count) =
            if self.services.mcp.server_outcomes.is_empty() {
                let connected = self
                    .services
                    .mcp
                    .tools
                    .iter()
                    .map(|t| &t.server_id)
                    .collect::<std::collections::HashSet<_>>()
                    .len();
                (connected, connected)
            } else {
                let total = self.services.mcp.server_outcomes.len();
                let connected = self
                    .services
                    .mcp
                    .server_outcomes
                    .iter()
                    .filter(|o| o.connected)
                    .count();
                (total, connected)
            };
        self.update_metrics(|m| {
            m.active_mcp_tools = active_mcp;
            m.mcp_tool_count = mcp_total;
            m.mcp_server_count = mcp_server_count;
            m.mcp_connected_count = mcp_connected_count;
        });
        if let Some(ref manager) = self.services.mcp.manager {
            let instructions = manager.all_server_instructions().await;
            if !instructions.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&instructions);
            }
        }
        if !matched_tools.is_empty() {
            let tool_names: Vec<&str> = matched_tools.iter().map(|t| t.name.as_str()).collect();
            tracing::debug!(
                skills = ?self.services.skill.active_skill_names,
                mcp_tools = ?tool_names,
                "matched items"
            );
            let tools_prompt = zeph_mcp::format_mcp_tools_prompt(&matched_tools);
            if !tools_prompt.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&tools_prompt);
            }
        }
    }

    async fn match_mcp_tools(&self, query: &str) -> Vec<zeph_mcp::McpTool> {
        let Some(ref registry) = self.services.mcp.registry else {
            return self.services.mcp.tools.clone();
        };
        let provider = self.embedding_provider.clone();
        let hits = registry
            .search(query, self.services.skill.max_active_skills, |text| {
                let owned = text.to_owned();
                let p = provider.clone();
                Box::pin(async move { p.embed(&owned).await })
            })
            .await;
        self.rehydrate_mcp_tools(hits)
    }

    /// Rehydrate Qdrant-derived tool stubs against the live, in-memory tool list.
    ///
    /// `McpToolRegistry::search` returns tools with an empty `input_schema` and default
    /// `security_meta` — the Qdrant payload only stores description fields, never the full
    /// schema (#5935). This replaces each hit with its live counterpart from
    /// `self.services.mcp.tools`, matched by `(server_id, name)`, so the LLM prompt gets the
    /// real `input_schema` instead of `{}`.
    ///
    /// A hit with no live match (server disconnected, tool removed since the last sync) is
    /// dropped rather than surfaced with an empty schema — that would just reproduce the bug
    /// this fixes.
    fn rehydrate_mcp_tools(&self, hits: Vec<zeph_mcp::McpTool>) -> Vec<zeph_mcp::McpTool> {
        hits.into_iter()
            .filter_map(|hit| {
                let live = self
                    .services
                    .mcp
                    .tools
                    .iter()
                    .find(|t| t.server_id == hit.server_id && t.name == hit.name)
                    .cloned();
                if live.is_none() {
                    tracing::warn!(
                        server_id = hit.server_id,
                        tool = hit.name,
                        "MCP tool from semantic search has no live match; dropping stale Qdrant hit"
                    );
                }
                live
            })
            .collect()
    }

    /// Poll the watch receiver for tool list updates from `tools/list_changed` notifications,
    /// and process any deferred semantic index rebuild requests.
    ///
    /// Called once per agent turn, before processing user input.  Two triggers cause a rebuild:
    /// - A `tools/list_changed` notification from an MCP server (via `tool_rx`).
    /// - `pending_semantic_rebuild == true`, set by `/mcp add` or `/mcp remove` when dispatched
    ///   via `AgentAccess::handle_mcp` (which cannot call `rebuild_semantic_index` directly
    ///   because the future would be `!Send`).
    ///
    /// Both branches also call [`refresh_mcp_tool_ids`](Self::refresh_mcp_tool_ids) so
    /// `TrustGateExecutor`'s Quarantine-deny set stays current with MCP servers connected
    /// after startup (#5747) — otherwise it is only ever populated once, at agent construction.
    ///
    /// If neither trigger fires, this is a no-op.
    pub(super) async fn check_tool_refresh(&mut self) {
        // Handle deferred rebuild from /mcp add|remove via AgentAccess.
        if self.services.mcp.pending_semantic_rebuild {
            self.services.mcp.pending_semantic_rebuild = false;
            self.refresh_mcp_tool_ids();
            self.rebuild_semantic_index().await;
            self.sync_mcp_registry().await;
            self.refresh_shadow_sentinel_mcp_tool_ids();
            let mcp_total = self.services.mcp.tools.len();
            let mcp_servers = self
                .services
                .mcp
                .tools
                .iter()
                .map(|t| &t.server_id)
                .collect::<std::collections::HashSet<_>>()
                .len();
            self.update_metrics(|m| {
                m.mcp_tool_count = mcp_total;
                m.mcp_server_count = mcp_servers;
            });
        }

        let Some(ref mut rx) = self.services.mcp.tool_rx else {
            return;
        };
        if !rx.has_changed().unwrap_or(false) {
            return;
        }
        let new_tools = rx.borrow_and_update().clone();
        if new_tools.is_empty() {
            // Guard against replacing a non-empty initial tool list with the watch's empty
            // initial value. The watch is only updated after a real tools/list_changed event.
            //
            // This early return also means `refresh_mcp_tool_ids` below is skipped, so
            // `mcp_tool_ids` can retain stale ids past this point — but only fail-safe (over-deny
            // of ids that no longer map to a live tool), never fail-open. In practice this branch
            // is unreachable with an empty list anyway: the `tools/list_changed` producer never
            // sends an empty vec, and removing the last server goes through the
            // `pending_semantic_rebuild` branch above, which has no such guard.
            return;
        }
        tracing::info!(
            tools = new_tools.len(),
            "tools/list_changed: agent tool list refreshed"
        );
        self.services.mcp.tools = new_tools;
        self.services.mcp.sync_executor_tools();
        self.services.mcp.pruning_cache.reset();
        self.refresh_mcp_tool_ids();
        self.rebuild_semantic_index().await;
        self.sync_mcp_registry().await;
        self.refresh_shadow_sentinel_mcp_tool_ids();
        let mcp_total = self.services.mcp.tools.len();
        let mcp_servers = self
            .services
            .mcp
            .tools
            .iter()
            .map(|t| &t.server_id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        self.update_metrics(|m| {
            m.mcp_tool_count = mcp_total;
            m.mcp_server_count = mcp_servers;
        });
    }

    /// Refreshes `ShadowSentinel`'s registered-MCP-tool-id set from the current
    /// `services.mcp.tools` list, so a server connected after startup (via `/mcp add` or a live
    /// `tools/list_changed` notification) is reflected without waiting for a process restart.
    ///
    /// Only covers `ShadowSentinel`'s own tool-id set, used for risk classification.
    /// `TrustGateExecutor`'s separate Quarantine-deny set is refreshed independently by
    /// [`refresh_mcp_tool_ids`](Self::refresh_mcp_tool_ids) (#5747).
    fn refresh_shadow_sentinel_mcp_tool_ids(&self) {
        let Some(ref sentinel) = self.services.security.shadow_sentinel else {
            return;
        };
        let ids: std::collections::HashSet<String> = self
            .services
            .mcp
            .tools
            .iter()
            .map(zeph_mcp::McpTool::sanitized_id)
            .collect();
        *sentinel.mcp_tool_ids_handle().write() = ids;
    }

    /// Rebuilds `TrustGateExecutor`'s MCP tool-id registry (`self.services.security.mcp_tool_ids`)
    /// from the current `self.services.mcp.tools`, using the same id derivation
    /// (`McpTool::sanitized_id`) and replace-not-union semantics as the startup-time
    /// `register_mcp_tool_ids` (binary crate `agent_setup.rs`). Replace semantics correctly
    /// drop ids for servers that disconnected since the last refresh. A no-op when no handle
    /// was attached via `AgentBuilder::with_mcp_tool_ids_handle`.
    fn refresh_mcp_tool_ids(&self) {
        let Some(ref handle) = self.services.security.mcp_tool_ids else {
            return;
        };
        let ids: std::collections::HashSet<String> = self
            .services
            .mcp
            .tools
            .iter()
            .map(zeph_mcp::McpTool::sanitized_id)
            .collect();
        *handle.write() = ids;
    }

    pub(super) async fn sync_mcp_registry(&mut self) {
        if self.services.mcp.registry.is_none() {
            return;
        }
        if !self.embedding_provider.supports_embeddings() {
            return;
        }
        // Clone tools before .await to avoid holding &self.services.mcp.tools across an await point.
        let tools = self.services.mcp.tools.clone();
        let provider = self.embedding_provider.clone();
        let embedding_model = self.services.skill.embedding_model.clone();
        let embed_timeout =
            std::time::Duration::from_secs(self.runtime.config.timeouts.embedding_seconds);
        let embed_fn = move |text: &str| -> zeph_mcp::registry::EmbedFuture {
            let owned = text.to_owned();
            let p = provider.clone();
            Box::pin(async move {
                if let Ok(result) = tokio::time::timeout(embed_timeout, p.embed(&owned)).await {
                    result
                } else {
                    tracing::warn!(
                        timeout_secs = embed_timeout.as_secs(),
                        "MCP registry: embedding timed out"
                    );
                    Err(zeph_llm::LlmError::Timeout)
                }
            })
        };
        // Take registry out of self to avoid holding &mut self.services.mcp.registry across .await.
        // No early returns between take() and put-back — the await is the only yield point here.
        let Some(mut registry) = self.services.mcp.registry.take() else {
            return;
        };
        if let Err(e) = registry.sync(&tools, &embedding_model, embed_fn).await {
            tracing::warn!("failed to sync MCP tool registry: {e:#}");
        }
        self.services.mcp.registry = Some(registry);
    }

    /// Build (or rebuild) the in-memory semantic tool index for embedding-based discovery.
    /// Build the initial semantic tool index after agent construction.
    ///
    /// Must be called once after `with_mcp` and `with_mcp_discovery` are applied,
    /// before the first user turn.  Subsequent rebuilds happen automatically on
    /// tool list change events (`check_tool_refresh`, `/mcp add`, `/mcp remove`).
    pub async fn init_semantic_index(&mut self) {
        self.rebuild_semantic_index().await;
    }

    /// Drain and process all pending elicitation requests without blocking.
    ///
    /// Call this at the start of each turn and between tool calls to prevent
    /// elicitation events from accumulating while the agent loop is busy.
    pub(super) async fn process_pending_elicitations(&mut self) {
        loop {
            let Some(ref mut rx) = self.services.mcp.elicitation_rx else {
                return;
            };
            match rx.try_recv() {
                Ok(event) => {
                    self.handle_elicitation_event(event).await;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.services.mcp.elicitation_rx = None;
                    return;
                }
            }
        }
    }

    /// Handle a single elicitation event by routing it to the active channel.
    pub(super) async fn handle_elicitation_event(&mut self, event: zeph_mcp::ElicitationEvent) {
        use crate::channel::{ElicitationRequest, ElicitationResponse};

        let decline = ElicitResult::new(ElicitationAction::Decline);

        let channel_request = match &event.request {
            rmcp::model::ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => {
                let fields = build_elicitation_fields(requested_schema);
                ElicitationRequest {
                    server_name: event.server_id.clone(),
                    message: sanitize_elicitation_message(message),
                    fields,
                }
            }
            rmcp::model::ElicitRequestParams::UrlElicitationParams { .. } => {
                // URL elicitation not supported in phase 1 — decline.
                tracing::debug!(
                    server_id = event.server_id,
                    "URL elicitation not supported, declining"
                );
                let _ = event.response_tx.send(decline);
                return;
            }
            // ElicitRequestParams is #[non_exhaustive] — decline unknown future variants.
            _ => {
                tracing::debug!(
                    server_id = event.server_id,
                    "unknown elicitation request variant, declining"
                );
                let _ = event.response_tx.send(decline);
                return;
            }
        };

        if self.services.mcp.elicitation_warn_sensitive_fields {
            let sensitive: Vec<&str> = channel_request
                .fields
                .iter()
                .filter(|f| is_sensitive_field(&f.name))
                .map(|f| f.name.as_str())
                .collect();
            if !sensitive.is_empty() {
                let fields_list = sensitive.join(", ");
                let warning = format!(
                    "Warning: [{}] is requesting sensitive information (field: {}). \
                     Only proceed if you trust this server.",
                    channel_request.server_name, fields_list,
                );
                tracing::warn!(
                    server_id = event.server_id,
                    fields = %fields_list,
                    "elicitation requests sensitive fields"
                );
                let _ = self.channel.send(&warning).await;
            }
        }

        self.channel
            .send_status_best_effort("MCP server requesting input…")
            .await;
        let response = match self.channel.elicit(channel_request).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    server_id = event.server_id,
                    "elicitation channel error: {e:#}"
                );
                self.channel.send_status_best_effort("").await;
                let _ = event.response_tx.send(decline);
                return;
            }
        };
        self.channel.send_status_best_effort("").await;

        let result = match response {
            ElicitationResponse::Accepted(value) => {
                ElicitResult::new(ElicitationAction::Accept).with_content(value)
            }
            ElicitationResponse::Declined => ElicitResult::new(ElicitationAction::Decline),
            ElicitationResponse::Cancelled => ElicitResult::new(ElicitationAction::Cancel),
        };

        if event.response_tx.send(result).is_err() {
            tracing::warn!(
                server_id = event.server_id,
                "elicitation response dropped — handler disconnected"
            );
        }
    }

    fn update_mcp_metrics(&mut self) {
        let mcp_total = self.services.mcp.tools.len();
        let mcp_server_count = self.services.mcp.server_outcomes.len();
        let mcp_connected_count = self
            .services
            .mcp
            .server_outcomes
            .iter()
            .filter(|o| o.connected)
            .count();
        let mcp_servers: Vec<crate::metrics::McpServerStatus> = self
            .services
            .mcp
            .server_outcomes
            .iter()
            .map(|o| crate::metrics::McpServerStatus {
                id: o.id.clone(),
                status: if o.connected {
                    crate::metrics::McpServerConnectionStatus::Connected
                } else {
                    crate::metrics::McpServerConnectionStatus::Failed
                },
                tool_count: o.tool_count,
                error: o.error.clone(),
                input_schemas_dropped: o.input_schemas_dropped,
                output_schemas_dropped: o.output_schemas_dropped,
            })
            .collect();
        self.update_metrics(|m| {
            m.mcp_tool_count = mcp_total;
            m.mcp_server_count = mcp_server_count;
            m.mcp_connected_count = mcp_connected_count;
            m.mcp_servers = mcp_servers;
        });
    }

    /// Rebuild the in-memory semantic tool index.
    ///
    /// Only runs when `discovery_strategy == Embedding`.  On failure (all embeddings fail),
    /// sets `semantic_index = None` and logs at WARN — the caller falls back to all tools.
    ///
    /// Called at:
    /// - initial setup via `init_semantic_index()`
    /// - `tools/list_changed` notification
    /// - `/mcp add` and `/mcp remove`
    pub(in crate::agent) async fn rebuild_semantic_index(&mut self) {
        if self.services.mcp.discovery_strategy != zeph_mcp::ToolDiscoveryStrategy::Embedding {
            return;
        }

        if self.services.mcp.tools.is_empty() {
            self.services.mcp.semantic_index = None;
            return;
        }

        // Resolve embedding provider: dedicated discovery provider → primary embedding provider.
        let provider = self
            .services
            .mcp
            .discovery_provider
            .clone()
            .unwrap_or_else(|| self.embedding_provider.clone());

        let inner_embed = provider.embed_fn();
        let embed_timeout =
            std::time::Duration::from_secs(self.runtime.config.timeouts.embedding_seconds);
        let embed_fn = move |text: &str| -> zeph_llm::provider::EmbedFuture {
            let fut = inner_embed(text);
            Box::pin(async move {
                if let Ok(result) = tokio::time::timeout(embed_timeout, fut).await {
                    result
                } else {
                    tracing::warn!(
                        timeout_secs = embed_timeout.as_secs(),
                        "semantic index: embedding probe timed out"
                    );
                    Err(zeph_llm::LlmError::Timeout)
                }
            })
        };

        // Clone tools before .await to avoid holding &self.services.mcp.tools across an await point.
        let tools = self.services.mcp.tools.clone();
        match zeph_mcp::SemanticToolIndex::build(&tools, &embed_fn).await {
            Ok(idx) => {
                tracing::info!(
                    indexed = idx.len(),
                    total = self.services.mcp.tools.len(),
                    "semantic tool index built"
                );
                self.services.mcp.semantic_index = Some(idx);
            }
            Err(e) => {
                tracing::warn!(
                    "semantic tool index build failed, falling back to all tools: {e:#}"
                );
                self.services.mcp.semantic_index = None;
            }
        }
    }
}

/// SEC-MCP-01: validate that a stdio command target is on the allowlist.
///
/// Returns `Some(error_message)` when the command is blocked, `None` when it is allowed.
fn validate_mcp_command(target: &str, allowed_commands: &[String]) -> Option<String> {
    let is_url = target.starts_with("http://") || target.starts_with("https://");
    if !is_url && !allowed_commands.is_empty() && !allowed_commands.iter().any(|c| c == target) {
        Some(format!(
            "Command '{target}' is not allowed. Permitted: {}",
            allowed_commands.join(", ")
        ))
    } else {
        None
    }
}

/// Build a `ServerEntry` for a newly added MCP server from parsed `/mcp add` arguments.
fn build_server_entry(id: &str, target: &str, extra_args: &[&str]) -> zeph_mcp::ServerEntry {
    let is_url = target.starts_with("http://") || target.starts_with("https://");
    let transport = if is_url {
        zeph_mcp::McpTransport::Http {
            url: target.to_owned(),
            headers: std::collections::HashMap::new(),
        }
    } else {
        zeph_mcp::McpTransport::Stdio {
            command: target.to_owned(),
            args: extra_args.iter().map(|&s| s.to_owned()).collect(),
            env: std::collections::HashMap::new(),
        }
    };
    zeph_mcp::ServerEntry {
        id: id.to_owned(),
        transport,
        timeout: std::time::Duration::from_secs(30),
        trust_level: zeph_config::McpTrustLevel::Untrusted,
        tool_allowlist: None,
        allow_untrusted_without_allowlist: false,
        expected_tools: Vec::new(),
        roots: Vec::new(),
        tool_metadata: std::collections::HashMap::new(),
        elicitation_enabled: false,
        elicitation_timeout_secs: 120,
        env_isolation: false,
        media_passthrough: false,
    }
}

/// Convert an rmcp `ElicitationSchema` into channel-agnostic `ElicitationField` list.
fn build_elicitation_fields(
    schema: &rmcp::model::ElicitationSchema,
) -> Vec<crate::channel::ElicitationField> {
    use crate::channel::{ElicitationField, ElicitationFieldType};
    use rmcp::model::PrimitiveSchemaDefinition;

    schema
        .properties
        .iter()
        .map(|(name, prop)| {
            // Extract field type and description by serializing the PrimitiveSchemaDefinition
            // to JSON and reading the discriminator field.  This avoids deep-matching the
            // nested EnumSchema / StringSchema / … variants of rmcp's type-safe schema
            // hierarchy.
            let json = serde_json::to_value(prop).unwrap_or_default();
            let description = json
                .get("description")
                .and_then(|v| v.as_str())
                .map(sanitize_elicitation_message);

            let field_type = match prop {
                PrimitiveSchemaDefinition::Boolean(_) => ElicitationFieldType::Boolean,
                PrimitiveSchemaDefinition::Integer(_) => ElicitationFieldType::Integer,
                PrimitiveSchemaDefinition::Number(_) => ElicitationFieldType::Number,
                PrimitiveSchemaDefinition::Enum(_) => {
                    // Extract enum values from the serialized form.  All EnumSchema variants
                    // serialise their allowed values under "enum" or inside "items.enum".
                    let vals = json
                        .get("enum")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .map(sanitize_elicitation_message)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    ElicitationFieldType::Enum(vals)
                }
                PrimitiveSchemaDefinition::String(_) => ElicitationFieldType::String,
                // Any future `#[non_exhaustive]` variant falls back to
                // `ElicitationFieldType::String` rather than panicking.
                _ => {
                    tracing::debug!(
                        "unknown PrimitiveSchemaDefinition variant, defaulting to String"
                    );
                    ElicitationFieldType::String
                }
            };
            let required = schema.required.as_deref().is_some_and(|r| r.contains(name));
            ElicitationField {
                // Keep the raw schema key intact — it is used verbatim as the response map key.
                // Channels sanitize it at display time only (see build_field_prompt / build_telegram_field_prompt).
                name: name.clone(),
                description,
                field_type,
                required,
            }
        })
        .collect()
}

/// Sensitive field name patterns (case-insensitive substring match).
const SENSITIVE_FIELD_PATTERNS: &[&str] = &[
    "password",
    "passwd",
    "token",
    "secret",
    "key",
    "credential",
    "apikey",
    "api_key",
    "auth",
    "authorization",
    "private",
    "passphrase",
    "pin",
];

/// Returns `true` when `field_name` matches any sensitive pattern (case-insensitive).
fn is_sensitive_field(field_name: &str) -> bool {
    let lower = field_name.to_lowercase();
    SENSITIVE_FIELD_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

/// Sanitize an elicitation message: cap length (in chars, not bytes) and strip control chars.
fn sanitize_elicitation_message(message: &str) -> String {
    const MAX_CHARS: usize = 500;
    // Collect up to MAX_CHARS chars, filtering control characters that could manipulate terminals.
    message
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;
    use std::assert_matches;

    #[tokio::test]
    async fn handle_mcp_command_unknown_subcommand_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.handle_mcp_command("unknown").await.unwrap();
        assert!(
            result.contains("Usage: /mcp"),
            "expected usage message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_list_no_manager_shows_disabled() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.handle_mcp_command("list").await.unwrap();
        assert!(
            result.contains("MCP is not enabled"),
            "expected not-enabled message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_tools_no_server_id_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.handle_mcp_command("tools").await.unwrap();
        assert!(
            result.contains("Usage: /mcp tools"),
            "expected tools usage message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_remove_no_server_id_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.handle_mcp_command("remove").await.unwrap();
        assert!(
            result.contains("Usage: /mcp remove"),
            "expected remove usage message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_remove_no_manager_shows_disabled() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.handle_mcp_command("remove my-server").await.unwrap();
        assert!(
            result.contains("MCP is not enabled"),
            "expected not-enabled message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_add_insufficient_args_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // "add" with only 1 arg (needs at least 2: id + command)
        let result = agent.handle_mcp_command("add server-id").await.unwrap();
        assert!(
            result.contains("Usage: /mcp add"),
            "expected add usage message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_tools_with_unknown_server_shows_no_tools() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // mcp.tools is empty, so any server will have no tools
        let result = agent
            .handle_mcp_command("tools nonexistent-server")
            .await
            .unwrap();
        assert!(
            result.contains("No tools found"),
            "expected no-tools message, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn mcp_tool_count_starts_at_zero() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        assert_eq!(agent.services.mcp.tool_count(), 0);
    }

    fn test_mcp_tool(
        server_id: &str,
        name: &str,
        input_schema: serde_json::Value,
    ) -> zeph_mcp::McpTool {
        zeph_mcp::McpTool {
            server_id: server_id.to_owned(),
            name: name.to_owned(),
            description: format!("{name} description"),
            input_schema,
            output_schema: None,
            security_meta: zeph_config::mcp_security::ToolSecurityMeta::default(),
        }
    }

    /// #5935: `McpToolRegistry::search` returns stubs with an empty `input_schema` — a hit
    /// whose `(server_id, name)` matches a live tool must be replaced wholesale so the real
    /// schema (and `output_schema`/`security_meta`) reach the LLM prompt.
    #[tokio::test]
    async fn rehydrate_mcp_tools_replaces_stub_with_live_schema() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let real_schema =
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}});
        agent.services.mcp.tools = vec![test_mcp_tool("fs", "read_file", real_schema.clone())];

        // Shape of what McpToolRegistry::search actually returns: same (server_id, name),
        // empty schema/default security meta.
        let stub = test_mcp_tool("fs", "read_file", serde_json::json!({}));

        let rehydrated = agent.rehydrate_mcp_tools(vec![stub]);

        assert_eq!(rehydrated.len(), 1);
        assert_eq!(rehydrated[0].input_schema, real_schema);
    }

    /// A search hit with no live counterpart (server disconnected, tool removed since the
    /// last Qdrant sync) must be dropped — never passed through with an empty schema, which
    /// would just reproduce the original #5935 symptom silently.
    #[tokio::test]
    async fn rehydrate_mcp_tools_drops_hit_with_no_live_match() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.services.mcp.tools = vec![test_mcp_tool("fs", "other_tool", serde_json::json!({}))];

        let stub = test_mcp_tool("fs", "read_file", serde_json::json!({}));

        let rehydrated = agent.rehydrate_mcp_tools(vec![stub]);

        assert!(
            rehydrated.is_empty(),
            "stale hit with no live match must be dropped, not passed through with an empty schema"
        );
    }

    #[tokio::test]
    async fn rehydrate_mcp_tools_mixed_batch_keeps_only_matches() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let schema_a =
            serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let schema_c =
            serde_json::json!({"type": "object", "properties": {"c": {"type": "number"}}});
        agent.services.mcp.tools = vec![
            test_mcp_tool("srv1", "tool_a", schema_a.clone()),
            test_mcp_tool("srv2", "tool_c", schema_c.clone()),
        ];

        let hits = vec![
            test_mcp_tool("srv1", "tool_a", serde_json::json!({})),
            test_mcp_tool("srv1", "tool_b", serde_json::json!({})), // no live match — dropped
            test_mcp_tool("srv2", "tool_c", serde_json::json!({})),
        ];

        let rehydrated = agent.rehydrate_mcp_tools(hits);

        assert_eq!(rehydrated.len(), 2);
        assert_eq!(rehydrated[0].name, "tool_a");
        assert_eq!(rehydrated[0].input_schema, schema_a);
        assert_eq!(rehydrated[1].name, "tool_c");
        assert_eq!(rehydrated[1].input_schema, schema_c);
    }

    /// #5935 end-to-end: a tool rehydrated from a Qdrant-derived stub must reach the LLM
    /// system prompt (`format_mcp_tools_prompt`) with its real `input_schema`, not the empty
    /// `{}` the stub carried — this is the actual user-visible symptom the fix addresses.
    #[tokio::test]
    async fn rehydrated_tool_schema_reaches_llm_prompt() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let real_schema = serde_json::json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        });
        agent.services.mcp.tools = vec![test_mcp_tool("search", "web_search", real_schema)];

        let stub = test_mcp_tool("search", "web_search", serde_json::json!({}));
        let rehydrated = agent.rehydrate_mcp_tools(vec![stub]);

        let prompt = zeph_mcp::format_mcp_tools_prompt(&rehydrated);

        assert!(
            !prompt.contains("<parameters>{}</parameters>"),
            "expected real schema in prompt, got empty parameters block: {prompt}"
        );
        assert!(
            prompt.contains("\"query\""),
            "expected real schema fields in prompt: {prompt}"
        );
    }

    #[tokio::test]
    async fn check_tool_refresh_no_rx_is_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // No tool_rx set; check_tool_refresh should be a no-op.
        agent.check_tool_refresh().await;
        assert_eq!(agent.services.mcp.tool_count(), 0);
    }

    #[tokio::test]
    async fn check_tool_refresh_no_change_is_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let (tx, rx) = tokio::sync::watch::channel(Vec::new());
        agent.services.mcp.tool_rx = Some(rx);
        // No changes sent; has_changed() returns false.
        agent.check_tool_refresh().await;
        assert_eq!(agent.services.mcp.tool_count(), 0);
        drop(tx);
    }

    #[tokio::test]
    async fn check_tool_refresh_with_empty_initial_value_does_not_replace_tools() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.services.mcp.tools = vec![zeph_mcp::McpTool {
            server_id: "srv".into(),
            name: "existing_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: zeph_config::mcp_security::ToolSecurityMeta::default(),
        }];

        let (_tx, rx) = tokio::sync::watch::channel(Vec::<zeph_mcp::McpTool>::new());
        agent.services.mcp.tool_rx = Some(rx);
        // has_changed() is false for a fresh receiver; tools unchanged.
        agent.check_tool_refresh().await;
        assert_eq!(agent.services.mcp.tool_count(), 1);
    }

    #[tokio::test]
    async fn check_tool_refresh_applies_update() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let (tx, rx) = tokio::sync::watch::channel(Vec::<zeph_mcp::McpTool>::new());
        agent.services.mcp.tool_rx = Some(rx);

        let new_tools = vec![zeph_mcp::McpTool {
            server_id: "srv".into(),
            name: "refreshed_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: zeph_config::mcp_security::ToolSecurityMeta::default(),
        }];
        tx.send(new_tools).unwrap();

        agent.check_tool_refresh().await;
        assert_eq!(agent.services.mcp.tool_count(), 1);
        assert_eq!(agent.services.mcp.tools[0].name, "refreshed_tool");
    }

    /// #5736 follow-up (S1): a server connected after startup via a `tools/list_changed`
    /// notification must be reflected in `ShadowSentinel`'s registered-MCP-tool-id set, not just
    /// `services.mcp.tools` — otherwise the escalation fix in `classify_tool` stays blind to any
    /// MCP tool the agent didn't already know about at process start.
    #[tokio::test]
    async fn check_tool_refresh_updates_shadow_sentinel_mcp_tool_ids() {
        use crate::agent::shadow_sentinel::{
            ProbeVerdict, SafetyProbe, ShadowEventStore, ShadowSentinel,
        };

        struct NoopProbe;
        impl SafetyProbe for NoopProbe {
            fn evaluate<'a>(
                &'a self,
                _: &'a str,
                _: &'a serde_json::Value,
                _: &'a [crate::agent::shadow_sentinel::SentinelEvent],
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeVerdict> + Send + 'a>>
            {
                Box::pin(async { ProbeVerdict::Allow })
            }
        }

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let pool = zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        }
        .connect()
        .await
        .expect("connect + migrate in-memory sqlite pool");
        let store = ShadowEventStore::new(pool);
        let sentinel = std::sync::Arc::new(ShadowSentinel::new(
            store,
            Box::new(NoopProbe),
            zeph_config::ShadowSentinelConfig::default(),
            "test-session",
        ));
        agent.services.security.shadow_sentinel = Some(std::sync::Arc::clone(&sentinel));

        let (tx, rx) = tokio::sync::watch::channel(Vec::<zeph_mcp::McpTool>::new());
        agent.services.mcp.tool_rx = Some(rx);

        let new_tool = zeph_mcp::McpTool {
            server_id: "srv".into(),
            name: "refreshed_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: zeph_config::mcp_security::ToolSecurityMeta::default(),
        };
        let expected_id = new_tool.sanitized_id();
        tx.send(vec![new_tool]).unwrap();

        agent.check_tool_refresh().await;

        assert!(
            sentinel.mcp_tool_ids_handle().read().contains(&expected_id),
            "ShadowSentinel's mcp_tool_ids must be refreshed after a tools/list_changed event"
        );
    }

    #[tokio::test]
    async fn check_tool_refresh_without_mcp_tool_ids_handle_does_not_panic() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // No handle attached (services.security.mcp_tool_ids is None by default) — refresh
        // must be a no-op w.r.t. that field, and the tool list update must still apply.
        assert!(agent.services.security.mcp_tool_ids.is_none());

        let (tx, rx) = tokio::sync::watch::channel(Vec::<zeph_mcp::McpTool>::new());
        agent.services.mcp.tool_rx = Some(rx);
        let new_tools = vec![zeph_mcp::McpTool {
            server_id: "srv".into(),
            name: "refreshed_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: zeph_config::mcp_security::ToolSecurityMeta::default(),
        }];
        tx.send(new_tools).unwrap();

        agent.check_tool_refresh().await;
        assert_eq!(agent.services.mcp.tool_count(), 1);
    }

    #[tokio::test]
    async fn check_tool_refresh_updates_attached_mcp_tool_ids_handle() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let handle =
            std::sync::Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new()));
        agent.services.security.mcp_tool_ids = Some(std::sync::Arc::clone(&handle));

        let (tx, rx) = tokio::sync::watch::channel(Vec::<zeph_mcp::McpTool>::new());
        agent.services.mcp.tool_rx = Some(rx);
        let new_tools = vec![zeph_mcp::McpTool {
            server_id: "srv".into(),
            name: "refreshed_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: zeph_config::mcp_security::ToolSecurityMeta::default(),
        }];
        tx.send(new_tools).unwrap();

        agent.check_tool_refresh().await;

        assert!(
            handle.read().contains("srv_refreshed_tool"),
            "expected the sanitized id of the newly-connected tool in the handle, got: {:?}",
            *handle.read()
        );
    }

    #[tokio::test]
    async fn check_tool_refresh_drops_disconnected_tool_from_mcp_tool_ids_handle() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Simulate a stale id left over from an earlier (startup-time) population, for a
        // server that has since disconnected.
        let handle =
            std::sync::Arc::new(parking_lot::RwLock::new(std::collections::HashSet::from([
                "stale_server_old_tool".to_owned(),
            ])));
        agent.services.security.mcp_tool_ids = Some(std::sync::Arc::clone(&handle));

        let (tx, rx) = tokio::sync::watch::channel(Vec::<zeph_mcp::McpTool>::new());
        agent.services.mcp.tool_rx = Some(rx);
        let new_tools = vec![zeph_mcp::McpTool {
            server_id: "srv".into(),
            name: "refreshed_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: zeph_config::mcp_security::ToolSecurityMeta::default(),
        }];
        tx.send(new_tools).unwrap();

        agent.check_tool_refresh().await;

        let ids = handle.read();
        assert!(
            !ids.contains("stale_server_old_tool"),
            "disconnected server's tool id must be dropped (replace, not union), got: {ids:?}"
        );
        assert!(ids.contains("srv_refreshed_tool"));
    }

    /// Covers the `pending_semantic_rebuild` trigger (set by `/mcp add`/`/mcp remove`), the
    /// second of the two `check_tool_refresh` branches that call `refresh_mcp_tool_ids` — the
    /// other tests above only exercise the `tools/list_changed` (`tool_rx`) branch.
    #[tokio::test]
    async fn check_tool_refresh_updates_mcp_tool_ids_handle_via_pending_semantic_rebuild() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let handle =
            std::sync::Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new()));
        agent.services.security.mcp_tool_ids = Some(std::sync::Arc::clone(&handle));

        // Mirrors what handle_mcp_add/handle_mcp_remove already do before setting the flag:
        // self.services.mcp.tools is updated first, then pending_semantic_rebuild = true.
        agent.services.mcp.tools = vec![zeph_mcp::McpTool {
            server_id: "srv".into(),
            name: "added_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            security_meta: zeph_config::mcp_security::ToolSecurityMeta::default(),
        }];
        agent.services.mcp.pending_semantic_rebuild = true;

        agent.check_tool_refresh().await;

        assert!(
            handle.read().contains("srv_added_tool"),
            "expected the sanitized id of the /mcp add-connected tool in the handle, got: {:?}",
            *handle.read()
        );
        assert!(!agent.services.mcp.pending_semantic_rebuild);
    }

    #[test]
    fn sanitize_elicitation_message_strips_control_chars() {
        let input = "hello\x01world\x1b[31mred\x1b[0m";
        let output = sanitize_elicitation_message(input);
        assert!(!output.contains('\x01'));
        assert!(!output.contains('\x1b'));
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
    }

    #[test]
    fn sanitize_elicitation_message_preserves_newline_and_tab() {
        let input = "line1\nline2\ttabbed";
        let output = sanitize_elicitation_message(input);
        assert_eq!(output, "line1\nline2\ttabbed");
    }

    #[test]
    fn sanitize_elicitation_message_caps_at_500_chars() {
        // Build a 600-char ASCII string — no multi-byte boundary issue.
        let input: String = "a".repeat(600);
        let output = sanitize_elicitation_message(&input);
        assert_eq!(output.chars().count(), 500);
    }

    #[test]
    fn sanitize_elicitation_message_handles_multibyte_boundary() {
        // "é" is 2 bytes.  Build a string where a naive &str[..500] would panic.
        let input: String = "é".repeat(300); // 300 chars = 600 bytes
        let output = sanitize_elicitation_message(&input);
        // Should truncate to exactly 500 chars without panic.
        assert_eq!(output.chars().count(), 300);
    }

    #[test]
    fn build_elicitation_fields_maps_primitive_types() {
        use crate::channel::ElicitationFieldType;
        use rmcp::model::{
            BooleanSchema, ElicitationSchema, IntegerSchema, NumberSchema,
            PrimitiveSchemaDefinition, StringSchema,
        };
        use std::collections::BTreeMap;

        let mut props = BTreeMap::new();
        props.insert(
            "flag".to_owned(),
            PrimitiveSchemaDefinition::Boolean(BooleanSchema::new()),
        );
        props.insert(
            "count".to_owned(),
            PrimitiveSchemaDefinition::Integer(IntegerSchema::new()),
        );
        props.insert(
            "ratio".to_owned(),
            PrimitiveSchemaDefinition::Number(NumberSchema::new()),
        );
        props.insert(
            "name".to_owned(),
            PrimitiveSchemaDefinition::String(StringSchema::new()),
        );

        let schema = ElicitationSchema::new(props);
        let fields = build_elicitation_fields(&schema);

        let get = |n: &str| fields.iter().find(|f| f.name == n).unwrap();
        assert_matches!(get("flag").field_type, ElicitationFieldType::Boolean);
        assert_matches!(get("count").field_type, ElicitationFieldType::Integer);
        assert_matches!(get("ratio").field_type, ElicitationFieldType::Number);
        assert_matches!(get("name").field_type, ElicitationFieldType::String);
    }

    #[test]
    fn build_elicitation_fields_required_flag() {
        use rmcp::model::{ElicitationSchema, PrimitiveSchemaDefinition, StringSchema};
        use std::collections::BTreeMap;

        let mut props = BTreeMap::new();
        props.insert(
            "req".to_owned(),
            PrimitiveSchemaDefinition::String(StringSchema::new()),
        );
        props.insert(
            "opt".to_owned(),
            PrimitiveSchemaDefinition::String(StringSchema::new()),
        );

        let mut schema = ElicitationSchema::new(props);
        schema.required = Some(vec!["req".to_owned()]);

        let fields = build_elicitation_fields(&schema);
        let req = fields.iter().find(|f| f.name == "req").unwrap();
        let opt = fields.iter().find(|f| f.name == "opt").unwrap();
        assert!(req.required);
        assert!(!opt.required);
    }

    #[test]
    fn is_sensitive_field_detects_common_patterns() {
        assert!(is_sensitive_field("password"));
        assert!(is_sensitive_field("PASSWORD"));
        assert!(is_sensitive_field("user_password"));
        assert!(is_sensitive_field("api_token"));
        assert!(is_sensitive_field("SECRET_KEY"));
        assert!(is_sensitive_field("auth_header"));
        assert!(is_sensitive_field("private_key"));
    }

    #[test]
    fn is_sensitive_field_allows_non_sensitive_names() {
        assert!(!is_sensitive_field("username"));
        assert!(!is_sensitive_field("email"));
        assert!(!is_sensitive_field("message"));
        assert!(!is_sensitive_field("description"));
        assert!(!is_sensitive_field("subject"));
    }
}
