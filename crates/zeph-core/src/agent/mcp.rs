// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use rmcp::model::{CreateElicitationResult, ElicitationAction};

use super::{Agent, Channel, LlmProvider};

impl<C: Channel> Agent<C> {
    pub(super) async fn handle_mcp_command(
        &mut self,
        args: &str,
    ) -> Result<(), super::error::AgentError> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        match parts.first().copied() {
            Some("add") => self.handle_mcp_add(&parts[1..]).await,
            Some("list") => self.handle_mcp_list().await,
            Some("tools") => self.handle_mcp_tools(parts.get(1).copied()).await,
            Some("remove") => self.handle_mcp_remove(parts.get(1).copied()).await,
            _ => {
                self.channel
                    .send("Usage: /mcp add|list|tools|remove")
                    .await?;
                Ok(())
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_mcp_add(&mut self, args: &[&str]) -> Result<(), super::error::AgentError> {
        if args.len() < 2 {
            self.channel
                .send("Usage: /mcp add <id> <command> [args...] | /mcp add <id> <url>")
                .await?;
            return Ok(());
        }

        let Some(ref manager) = self.mcp.manager else {
            self.channel.send("MCP is not enabled.").await?;
            return Ok(());
        };

        let target = args[1];
        let is_url = target.starts_with("http://") || target.starts_with("https://");

        // SEC-MCP-01: validate command against allowlist (stdio only)
        if !is_url
            && !self.mcp.allowed_commands.is_empty()
            && !self.mcp.allowed_commands.iter().any(|c| c == target)
        {
            self.channel
                .send(&format!(
                    "Command '{target}' is not allowed. Permitted: {}",
                    self.mcp.allowed_commands.join(", ")
                ))
                .await?;
            return Ok(());
        }

        // SEC-MCP-03: enforce server limit
        let current_count = manager.list_servers().await.len();
        if current_count >= self.mcp.max_dynamic {
            self.channel
                .send(&format!(
                    "Server limit reached ({}/{}).",
                    current_count, self.mcp.max_dynamic
                ))
                .await?;
            return Ok(());
        }

        let transport = if is_url {
            zeph_mcp::McpTransport::Http {
                url: target.to_owned(),
                headers: std::collections::HashMap::new(),
            }
        } else {
            zeph_mcp::McpTransport::Stdio {
                command: target.to_owned(),
                args: args[2..].iter().map(|&s| s.to_owned()).collect(),
                env: std::collections::HashMap::new(),
            }
        };

        let entry = zeph_mcp::ServerEntry {
            id: args[0].to_owned(),
            transport,
            timeout: std::time::Duration::from_secs(30),
            trust_level: zeph_mcp::McpTrustLevel::Untrusted,
            tool_allowlist: None,
            expected_tools: Vec::new(),
            roots: Vec::new(),
            tool_metadata: std::collections::HashMap::new(),
            elicitation_enabled: false,
            elicitation_timeout_secs: 120,
            env_isolation: false,
        };

        let _ = self.channel.send_status("connecting to mcp...").await;
        match manager.add_server(&entry).await {
            Ok(tools) => {
                let _ = self.channel.send_status("").await;
                let count = tools.len();
                self.mcp
                    .server_outcomes
                    .push(zeph_mcp::ServerConnectOutcome {
                        id: entry.id.clone(),
                        connected: true,
                        tool_count: count,
                        error: String::new(),
                    });
                self.mcp.tools.extend(tools);
                self.mcp.sync_executor_tools();
                self.mcp.pruning_cache.reset();
                self.rebuild_semantic_index().await;
                self.sync_mcp_registry().await;
                let mcp_total = self.mcp.tools.len();
                let mcp_server_count = self.mcp.server_outcomes.len();
                let mcp_connected_count = self
                    .mcp
                    .server_outcomes
                    .iter()
                    .filter(|o| o.connected)
                    .count();
                let mcp_servers: Vec<crate::metrics::McpServerStatus> = self
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
                    })
                    .collect();
                self.update_metrics(|m| {
                    m.mcp_tool_count = mcp_total;
                    m.mcp_server_count = mcp_server_count;
                    m.mcp_connected_count = mcp_connected_count;
                    m.mcp_servers = mcp_servers;
                });
                self.channel
                    .send(&format!(
                        "Connected MCP server '{}' ({count} tool(s))",
                        entry.id
                    ))
                    .await?;
                Ok(())
            }
            Err(e) => {
                let _ = self.channel.send_status("").await;
                tracing::warn!(server_id = entry.id, "MCP add failed: {e:#}");
                self.channel
                    .send(&format!("Failed to connect server '{}': {e}", entry.id))
                    .await?;
                Ok(())
            }
        }
    }

    async fn handle_mcp_list(&mut self) -> Result<(), super::error::AgentError> {
        use std::fmt::Write;

        let Some(ref manager) = self.mcp.manager else {
            self.channel.send("MCP is not enabled.").await?;
            return Ok(());
        };

        let server_ids = manager.list_servers().await;
        if server_ids.is_empty() {
            self.channel.send("No MCP servers connected.").await?;
            return Ok(());
        }

        let mut output = String::from("Connected MCP servers:\n");
        let mut total = 0usize;
        for id in &server_ids {
            let count = self.mcp.tools.iter().filter(|t| t.server_id == *id).count();
            total += count;
            let _ = writeln!(output, "- {id} ({count} tools)");
        }
        let _ = write!(output, "Total: {total} tool(s)");

        self.channel.send(&output).await?;
        Ok(())
    }

    async fn handle_mcp_tools(
        &mut self,
        server_id: Option<&str>,
    ) -> Result<(), super::error::AgentError> {
        use std::fmt::Write;

        let Some(server_id) = server_id else {
            self.channel.send("Usage: /mcp tools <server_id>").await?;
            return Ok(());
        };

        let tools: Vec<_> = self
            .mcp
            .tools
            .iter()
            .filter(|t| t.server_id == server_id)
            .collect();

        if tools.is_empty() {
            self.channel
                .send(&format!("No tools found for server '{server_id}'."))
                .await?;
            return Ok(());
        }

        let mut output = format!("Tools for '{server_id}' ({} total):\n", tools.len());
        for t in &tools {
            if t.description.is_empty() {
                let _ = writeln!(output, "- {}", t.name);
            } else {
                let _ = writeln!(output, "- {} — {}", t.name, t.description);
            }
        }
        self.channel.send(&output).await?;
        Ok(())
    }

    async fn handle_mcp_remove(
        &mut self,
        server_id: Option<&str>,
    ) -> Result<(), super::error::AgentError> {
        let Some(server_id) = server_id else {
            self.channel.send("Usage: /mcp remove <id>").await?;
            return Ok(());
        };

        let Some(ref manager) = self.mcp.manager else {
            self.channel.send("MCP is not enabled.").await?;
            return Ok(());
        };

        match manager.remove_server(server_id).await {
            Ok(()) => {
                let before = self.mcp.tools.len();
                self.mcp.tools.retain(|t| t.server_id != server_id);
                let removed = before - self.mcp.tools.len();
                self.mcp.server_outcomes.retain(|o| o.id != server_id);
                self.mcp.sync_executor_tools();
                self.mcp.pruning_cache.reset();
                self.rebuild_semantic_index().await;
                self.sync_mcp_registry().await;
                let mcp_total = self.mcp.tools.len();
                let mcp_server_count = self.mcp.server_outcomes.len();
                let mcp_connected_count = self
                    .mcp
                    .server_outcomes
                    .iter()
                    .filter(|o| o.connected)
                    .count();
                let mcp_servers: Vec<crate::metrics::McpServerStatus> = self
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
                    })
                    .collect();
                self.update_metrics(|m| {
                    m.mcp_tool_count = mcp_total;
                    m.mcp_server_count = mcp_server_count;
                    m.mcp_connected_count = mcp_connected_count;
                    m.mcp_servers = mcp_servers;
                    m.active_mcp_tools
                        .retain(|name| !name.starts_with(&format!("{server_id}:")));
                });
                self.channel
                    .send(&format!(
                        "Disconnected MCP server '{server_id}' (removed {removed} tools)"
                    ))
                    .await?;
                Ok(())
            }
            Err(e) => {
                tracing::warn!(server_id, "MCP remove failed: {e:#}");
                self.channel
                    .send(&format!("Failed to remove server '{server_id}': {e}"))
                    .await?;
                Ok(())
            }
        }
    }

    pub(super) async fn append_mcp_prompt(&mut self, query: &str, system_prompt: &mut String) {
        let matched_tools = self.match_mcp_tools(query).await;
        let active_mcp: Vec<String> = matched_tools
            .iter()
            .map(zeph_mcp::McpTool::qualified_name)
            .collect();
        let mcp_total = self.mcp.tools.len();
        let (mcp_server_count, mcp_connected_count) = if self.mcp.server_outcomes.is_empty() {
            let connected = self
                .mcp
                .tools
                .iter()
                .map(|t| &t.server_id)
                .collect::<std::collections::HashSet<_>>()
                .len();
            (connected, connected)
        } else {
            let total = self.mcp.server_outcomes.len();
            let connected = self
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
        if let Some(ref manager) = self.mcp.manager {
            let instructions = manager.all_server_instructions().await;
            if !instructions.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&instructions);
            }
        }
        if !matched_tools.is_empty() {
            let tool_names: Vec<&str> = matched_tools.iter().map(|t| t.name.as_str()).collect();
            tracing::debug!(
                skills = ?self.skill_state.active_skill_names,
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
        let Some(ref registry) = self.mcp.registry else {
            return self.mcp.tools.clone();
        };
        let provider = self.embedding_provider.clone();
        registry
            .search(query, self.skill_state.max_active_skills, |text| {
                let owned = text.to_owned();
                let p = provider.clone();
                Box::pin(async move { p.embed(&owned).await })
            })
            .await
    }

    /// Poll the watch receiver for tool list updates from `tools/list_changed` notifications.
    ///
    /// Called once per agent turn, before processing user input. When the tool list has changed,
    /// updates `mcp.tools`, syncs the executor, and schedules a registry sync.
    /// If no receiver is set (MCP disabled), or no change has occurred, this is a no-op.
    pub(super) async fn check_tool_refresh(&mut self) {
        let Some(ref mut rx) = self.mcp.tool_rx else {
            return;
        };
        if !rx.has_changed().unwrap_or(false) {
            return;
        }
        let new_tools = rx.borrow_and_update().clone();
        if new_tools.is_empty() {
            // Guard against replacing a non-empty initial tool list with the watch's empty
            // initial value. The watch is only updated after a real tools/list_changed event.
            return;
        }
        tracing::info!(
            tools = new_tools.len(),
            "tools/list_changed: agent tool list refreshed"
        );
        self.mcp.tools = new_tools;
        self.mcp.sync_executor_tools();
        self.mcp.pruning_cache.reset();
        self.rebuild_semantic_index().await;
        self.sync_mcp_registry().await;
        let mcp_total = self.mcp.tools.len();
        let mcp_servers = self
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

    pub(super) async fn sync_mcp_registry(&mut self) {
        let Some(ref mut registry) = self.mcp.registry else {
            return;
        };
        if !self.embedding_provider.supports_embeddings() {
            return;
        }
        let provider = self.embedding_provider.clone();
        let embed_fn = |text: &str| -> zeph_mcp::registry::EmbedFuture {
            let owned = text.to_owned();
            let p = provider.clone();
            Box::pin(async move { p.embed(&owned).await })
        };
        if let Err(e) = registry
            .sync(&self.mcp.tools, &self.skill_state.embedding_model, embed_fn)
            .await
        {
            tracing::warn!("failed to sync MCP tool registry: {e:#}");
        }
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
            let Some(ref mut rx) = self.mcp.elicitation_rx else {
                return;
            };
            match rx.try_recv() {
                Ok(event) => {
                    self.handle_elicitation_event(event).await;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.mcp.elicitation_rx = None;
                    return;
                }
            }
        }
    }

    /// Handle a single elicitation event by routing it to the active channel.
    pub(super) async fn handle_elicitation_event(&mut self, event: zeph_mcp::ElicitationEvent) {
        use crate::channel::{ElicitationRequest, ElicitationResponse};

        let decline = CreateElicitationResult {
            action: ElicitationAction::Decline,
            content: None,
            meta: None,
        };

        let channel_request = match &event.request {
            rmcp::model::CreateElicitationRequestParams::FormElicitationParams {
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
            rmcp::model::CreateElicitationRequestParams::UrlElicitationParams { .. } => {
                // URL elicitation not supported in phase 1 — decline.
                tracing::debug!(
                    server_id = event.server_id,
                    "URL elicitation not supported, declining"
                );
                let _ = event.response_tx.send(decline);
                return;
            }
        };

        if self.mcp.elicitation_warn_sensitive_fields {
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

        let _ = self
            .channel
            .send_status("MCP server requesting input…")
            .await;
        let response = match self.channel.elicit(channel_request).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    server_id = event.server_id,
                    "elicitation channel error: {e:#}"
                );
                let _ = self.channel.send_status("").await;
                let _ = event.response_tx.send(decline);
                return;
            }
        };
        let _ = self.channel.send_status("").await;

        let result = match response {
            ElicitationResponse::Accepted(value) => CreateElicitationResult {
                action: ElicitationAction::Accept,
                content: Some(value),
                meta: None,
            },
            ElicitationResponse::Declined => CreateElicitationResult {
                action: ElicitationAction::Decline,
                content: None,
                meta: None,
            },
            ElicitationResponse::Cancelled => CreateElicitationResult {
                action: ElicitationAction::Cancel,
                content: None,
                meta: None,
            },
        };

        if event.response_tx.send(result).is_err() {
            tracing::warn!(
                server_id = event.server_id,
                "elicitation response dropped — handler disconnected"
            );
        }
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
        if self.mcp.discovery_strategy != zeph_mcp::ToolDiscoveryStrategy::Embedding {
            return;
        }

        if self.mcp.tools.is_empty() {
            self.mcp.semantic_index = None;
            return;
        }

        // Resolve embedding provider: dedicated discovery provider → primary embedding provider.
        let provider = self
            .mcp
            .discovery_provider
            .clone()
            .unwrap_or_else(|| self.embedding_provider.clone());

        let embed_fn = provider.embed_fn();

        match zeph_mcp::SemanticToolIndex::build(&self.mcp.tools, &embed_fn).await {
            Ok(idx) => {
                tracing::info!(
                    indexed = idx.len(),
                    total = self.mcp.tools.len(),
                    "semantic tool index built"
                );
                self.mcp.semantic_index = Some(idx);
            }
            Err(e) => {
                tracing::warn!(
                    "semantic tool index build failed, falling back to all tools: {e:#}"
                );
                self.mcp.semantic_index = None;
            }
        }
    }
}

/// Convert an rmcp `ElicitationSchema` into channel-agnostic `ElicitationField` list.
fn build_elicitation_fields(
    schema: &rmcp::model::ElicitationSchema,
) -> Vec<crate::channel::ElicitationField> {
    use crate::channel::{ElicitationField, ElicitationFieldType};
    use rmcp::model::PrimitiveSchema;

    schema
        .properties
        .iter()
        .map(|(name, prop)| {
            // Extract field type and description by serializing the PrimitiveSchema to JSON
            // and reading the discriminator field.  This avoids deep-matching the nested
            // EnumSchema / StringSchema / … variants of rmcp's type-safe schema hierarchy.
            let json = serde_json::to_value(prop).unwrap_or_default();
            let description = json
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from);

            let field_type = match prop {
                PrimitiveSchema::Boolean(_) => ElicitationFieldType::Boolean,
                PrimitiveSchema::Integer(_) => ElicitationFieldType::Integer,
                PrimitiveSchema::Number(_) => ElicitationFieldType::Number,
                PrimitiveSchema::String(_) => ElicitationFieldType::String,
                PrimitiveSchema::Enum(_) => {
                    // Extract enum values from the serialized form.  All EnumSchema variants
                    // serialise their allowed values under "enum" or inside "items.enum".
                    let vals = json
                        .get("enum")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .map(String::from)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    ElicitationFieldType::Enum(vals)
                }
            };
            let required = schema.required.as_deref().is_some_and(|r| r.contains(name));
            ElicitationField {
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

    #[tokio::test]
    async fn handle_mcp_command_unknown_subcommand_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_mcp_command("unknown").await.unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Usage: /mcp")),
            "expected usage message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_list_no_manager_shows_disabled() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_mcp_command("list").await.unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("MCP is not enabled")),
            "expected not-enabled message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_tools_no_server_id_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_mcp_command("tools").await.unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Usage: /mcp tools")),
            "expected tools usage message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_remove_no_server_id_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.handle_mcp_command("remove").await.unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Usage: /mcp remove")),
            "expected remove usage message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_remove_no_manager_shows_disabled() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // "remove server-id" but no manager
        agent.handle_mcp_command("remove my-server").await.unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("MCP is not enabled")),
            "expected not-enabled message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_mcp_add_insufficient_args_shows_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // "add" with only 1 arg (needs at least 2)
        agent.handle_mcp_command("add server-id").await.unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("Usage: /mcp add")),
            "expected add usage message, got: {sent:?}"
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
        agent
            .handle_mcp_command("tools nonexistent-server")
            .await
            .unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|s| s.contains("No tools found")),
            "expected no-tools message, got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn mcp_tool_count_starts_at_zero() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        assert_eq!(agent.mcp.tool_count(), 0);
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
        assert_eq!(agent.mcp.tool_count(), 0);
    }

    #[tokio::test]
    async fn check_tool_refresh_no_change_is_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let (tx, rx) = tokio::sync::watch::channel(Vec::new());
        agent.mcp.tool_rx = Some(rx);
        // No changes sent; has_changed() returns false.
        agent.check_tool_refresh().await;
        assert_eq!(agent.mcp.tool_count(), 0);
        drop(tx);
    }

    #[tokio::test]
    async fn check_tool_refresh_with_empty_initial_value_does_not_replace_tools() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.mcp.tools = vec![zeph_mcp::McpTool {
            server_id: "srv".into(),
            name: "existing_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            security_meta: zeph_mcp::tool::ToolSecurityMeta::default(),
        }];

        let (_tx, rx) = tokio::sync::watch::channel(Vec::<zeph_mcp::McpTool>::new());
        agent.mcp.tool_rx = Some(rx);
        // has_changed() is false for a fresh receiver; tools unchanged.
        agent.check_tool_refresh().await;
        assert_eq!(agent.mcp.tool_count(), 1);
    }

    #[tokio::test]
    async fn check_tool_refresh_applies_update() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let (tx, rx) = tokio::sync::watch::channel(Vec::<zeph_mcp::McpTool>::new());
        agent.mcp.tool_rx = Some(rx);

        let new_tools = vec![zeph_mcp::McpTool {
            server_id: "srv".into(),
            name: "refreshed_tool".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            security_meta: zeph_mcp::tool::ToolSecurityMeta::default(),
        }];
        tx.send(new_tools).unwrap();

        agent.check_tool_refresh().await;
        assert_eq!(agent.mcp.tool_count(), 1);
        assert_eq!(agent.mcp.tools[0].name, "refreshed_tool");
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
            BooleanSchema, ElicitationSchema, IntegerSchema, NumberSchema, PrimitiveSchema,
            StringSchema,
        };
        use std::collections::BTreeMap;

        let mut props = BTreeMap::new();
        props.insert(
            "flag".to_owned(),
            PrimitiveSchema::Boolean(BooleanSchema::new()),
        );
        props.insert(
            "count".to_owned(),
            PrimitiveSchema::Integer(IntegerSchema::new()),
        );
        props.insert(
            "ratio".to_owned(),
            PrimitiveSchema::Number(NumberSchema::new()),
        );
        props.insert(
            "name".to_owned(),
            PrimitiveSchema::String(StringSchema::new()),
        );

        let schema = ElicitationSchema::new(props);
        let fields = build_elicitation_fields(&schema);

        let get = |n: &str| fields.iter().find(|f| f.name == n).unwrap();
        assert!(matches!(
            get("flag").field_type,
            ElicitationFieldType::Boolean
        ));
        assert!(matches!(
            get("count").field_type,
            ElicitationFieldType::Integer
        ));
        assert!(matches!(
            get("ratio").field_type,
            ElicitationFieldType::Number
        ));
        assert!(matches!(
            get("name").field_type,
            ElicitationFieldType::String
        ));
    }

    #[test]
    fn build_elicitation_fields_required_flag() {
        use rmcp::model::{ElicitationSchema, PrimitiveSchema, StringSchema};
        use std::collections::BTreeMap;

        let mut props = BTreeMap::new();
        props.insert(
            "req".to_owned(),
            PrimitiveSchema::String(StringSchema::new()),
        );
        props.insert(
            "opt".to_owned(),
            PrimitiveSchema::String(StringSchema::new()),
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
