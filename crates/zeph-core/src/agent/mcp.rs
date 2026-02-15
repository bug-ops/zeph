use super::{Agent, Channel, LlmProvider, ToolExecutor};

impl<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor> Agent<P, C, T> {
    pub(super) async fn handle_mcp_command(&mut self, args: &str) -> anyhow::Result<()> {
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

    async fn handle_mcp_add(&mut self, args: &[&str]) -> anyhow::Result<()> {
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
        };

        match manager.add_server(&entry).await {
            Ok(tools) => {
                let count = tools.len();
                self.mcp.tools.extend(tools);
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
                self.channel
                    .send(&format!(
                        "Connected MCP server '{}' ({count} tool(s))",
                        entry.id
                    ))
                    .await?;
                Ok(())
            }
            Err(e) => {
                tracing::warn!(server_id = entry.id, "MCP add failed: {e:#}");
                self.channel
                    .send(&format!("Failed to connect server '{}': {e}", entry.id))
                    .await?;
                Ok(())
            }
        }
    }

    async fn handle_mcp_list(&mut self) -> anyhow::Result<()> {
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

    async fn handle_mcp_tools(&mut self, server_id: Option<&str>) -> anyhow::Result<()> {
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

    async fn handle_mcp_remove(&mut self, server_id: Option<&str>) -> anyhow::Result<()> {
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
        let mcp_servers = self
            .mcp
            .tools
            .iter()
            .map(|t| &t.server_id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        self.update_metrics(|m| {
            m.active_mcp_tools = active_mcp;
            m.mcp_tool_count = mcp_total;
            m.mcp_server_count = mcp_servers;
        });
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
        let provider = self.provider.clone();
        registry
            .search(query, self.skill_state.max_active_skills, |text| {
                let owned = text.to_owned();
                let p = provider.clone();
                Box::pin(async move { p.embed(&owned).await })
            })
            .await
    }

    pub(super) async fn sync_mcp_registry(&mut self) {
        let Some(ref mut registry) = self.mcp.registry else {
            return;
        };
        if !self.provider.supports_embeddings() {
            return;
        }
        let provider = self.provider.clone();
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
}
