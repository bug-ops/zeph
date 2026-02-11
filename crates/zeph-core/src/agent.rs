use std::path::PathBuf;

use tokio::sync::{mpsc, watch};
use tokio_stream::StreamExt;
use zeph_llm::provider::{LlmProvider, Message, Role};
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::sqlite::role_str;
use zeph_skills::loader::Skill;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::prompt::format_skills_prompt;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::SkillEvent;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

use crate::channel::Channel;
#[cfg(feature = "self-learning")]
use crate::config::LearningConfig;
use crate::config::{SecurityConfig, TimeoutConfig};
use crate::context::build_system_prompt;
use crate::redact::redact_secrets;

// TODO(M14): Make configurable via AgentConfig (currently hardcoded for MVP)
const MAX_SHELL_ITERATIONS: usize = 3;

pub struct Agent<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor> {
    provider: P,
    channel: C,
    tool_executor: T,
    messages: Vec<Message>,
    registry: SkillRegistry,
    skill_paths: Vec<PathBuf>,
    matcher: Option<SkillMatcherBackend>,
    max_active_skills: usize,
    embedding_model: String,
    skill_reload_rx: Option<mpsc::Receiver<SkillEvent>>,
    memory: Option<SemanticMemory<P>>,
    conversation_id: Option<i64>,
    history_limit: u32,
    recall_limit: usize,
    summarization_threshold: usize,
    shutdown: watch::Receiver<bool>,
    active_skill_names: Vec<String>,
    security: SecurityConfig,
    timeouts: TimeoutConfig,
    #[cfg(feature = "self-learning")]
    learning_config: Option<LearningConfig>,
    #[cfg(feature = "self-learning")]
    reflection_used: bool,
    #[cfg(feature = "mcp")]
    mcp_tools: Vec<zeph_mcp::McpTool>,
    #[cfg(feature = "mcp")]
    mcp_registry: Option<zeph_mcp::McpToolRegistry>,
    #[cfg(feature = "mcp")]
    mcp_manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
    #[cfg(feature = "mcp")]
    mcp_allowed_commands: Vec<String>,
    #[cfg(feature = "mcp")]
    mcp_max_dynamic: usize,
}

impl<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor> Agent<P, C, T> {
    #[must_use]
    pub fn new(
        provider: P,
        channel: C,
        registry: SkillRegistry,
        matcher: Option<SkillMatcherBackend>,
        max_active_skills: usize,
        tool_executor: T,
    ) -> Self {
        let all_skills: Vec<Skill> = registry
            .all_meta()
            .iter()
            .filter_map(|m| registry.get_skill(&m.name).ok())
            .collect();
        let skills_prompt = format_skills_prompt(&all_skills, std::env::consts::OS);
        let system_prompt = build_system_prompt(&skills_prompt);

        let (_tx, rx) = watch::channel(false);
        Self {
            provider,
            channel,
            tool_executor,
            messages: vec![Message {
                role: Role::System,
                content: system_prompt,
            }],
            registry,
            skill_paths: Vec::new(),
            matcher,
            max_active_skills,
            embedding_model: String::new(),
            skill_reload_rx: None,
            memory: None,
            conversation_id: None,
            history_limit: 50,
            recall_limit: 5,
            summarization_threshold: 100,
            shutdown: rx,
            active_skill_names: Vec::new(),
            security: SecurityConfig::default(),
            timeouts: TimeoutConfig::default(),
            #[cfg(feature = "self-learning")]
            learning_config: None,
            #[cfg(feature = "self-learning")]
            reflection_used: false,
            #[cfg(feature = "mcp")]
            mcp_tools: Vec::new(),
            #[cfg(feature = "mcp")]
            mcp_registry: None,
            #[cfg(feature = "mcp")]
            mcp_manager: None,
            #[cfg(feature = "mcp")]
            mcp_allowed_commands: Vec::new(),
            #[cfg(feature = "mcp")]
            mcp_max_dynamic: 10,
        }
    }

    #[must_use]
    pub fn with_memory(
        mut self,
        memory: SemanticMemory<P>,
        conversation_id: i64,
        history_limit: u32,
        recall_limit: usize,
        summarization_threshold: usize,
    ) -> Self {
        self.memory = Some(memory);
        self.conversation_id = Some(conversation_id);
        self.history_limit = history_limit;
        self.recall_limit = recall_limit;
        self.summarization_threshold = summarization_threshold;
        self
    }

    #[must_use]
    pub fn with_embedding_model(mut self, model: String) -> Self {
        self.embedding_model = model;
        self
    }

    #[must_use]
    pub fn with_shutdown(mut self, rx: watch::Receiver<bool>) -> Self {
        self.shutdown = rx;
        self
    }

    #[must_use]
    pub fn with_skill_reload(
        mut self,
        paths: Vec<PathBuf>,
        rx: mpsc::Receiver<SkillEvent>,
    ) -> Self {
        self.skill_paths = paths;
        self.skill_reload_rx = Some(rx);
        self
    }

    #[cfg(feature = "self-learning")]
    #[must_use]
    pub fn with_learning(mut self, config: LearningConfig) -> Self {
        self.learning_config = Some(config);
        self
    }

    #[cfg(feature = "mcp")]
    #[must_use]
    pub fn with_mcp(
        mut self,
        tools: Vec<zeph_mcp::McpTool>,
        registry: Option<zeph_mcp::McpToolRegistry>,
        manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
        mcp_config: &crate::config::McpConfig,
    ) -> Self {
        self.mcp_tools = tools;
        self.mcp_registry = registry;
        self.mcp_manager = manager;
        self.mcp_allowed_commands
            .clone_from(&mcp_config.allowed_commands);
        self.mcp_max_dynamic = mcp_config.max_dynamic_servers;
        self
    }

    #[must_use]
    pub fn with_security(mut self, security: SecurityConfig, timeouts: TimeoutConfig) -> Self {
        self.security = security;
        self.timeouts = timeouts;
        self
    }

    /// Load conversation history from memory and inject into messages.
    ///
    /// # Errors
    ///
    /// Returns an error if loading history from `SQLite` fails.
    pub async fn load_history(&mut self) -> anyhow::Result<()> {
        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return Ok(());
        };

        let history = memory
            .sqlite()
            .load_history(cid, self.history_limit)
            .await?;
        if !history.is_empty() {
            let mut loaded = 0;
            let mut skipped = 0;

            for msg in history {
                if msg.content.trim().is_empty() {
                    tracing::warn!("skipping empty message from history (role: {:?})", msg.role);
                    skipped += 1;
                    continue;
                }
                self.messages.push(msg);
                loaded += 1;
            }

            tracing::info!("restored {loaded} message(s) from conversation {cid}");
            if skipped > 0 {
                tracing::warn!("skipped {skipped} empty message(s) from history");
            }
        }
        Ok(())
    }

    /// Run the chat loop, receiving messages via the channel until EOF or shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if channel I/O or LLM communication fails.
    pub async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let incoming = tokio::select! {
                result = self.channel.recv() => result?,
                () = shutdown_signal(&mut self.shutdown) => {
                    tracing::info!("shutting down");
                    break;
                }
                Some(_) = recv_skill_event(&mut self.skill_reload_rx) => {
                    self.reload_skills().await;
                    continue;
                }
            };

            let Some(incoming) = incoming else {
                break;
            };

            let trimmed = incoming.text.trim();

            if trimmed == "/skills" {
                self.handle_skills_command().await?;
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("/skill ") {
                self.handle_skill_command(rest).await?;
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("/feedback ") {
                self.handle_feedback(rest).await?;
                continue;
            }

            #[cfg(feature = "mcp")]
            if trimmed == "/mcp" || trimmed.starts_with("/mcp ") {
                let args = trimmed.strip_prefix("/mcp").unwrap_or("").trim();
                self.handle_mcp_command(args).await?;
                continue;
            }

            self.rebuild_system_prompt(&incoming.text).await;

            #[cfg(feature = "self-learning")]
            {
                self.reflection_used = false;
            }

            self.messages.push(Message {
                role: Role::User,
                content: incoming.text.clone(),
            });
            self.persist_message(Role::User, &incoming.text).await;

            if let Err(e) = self.process_response().await {
                tracing::error!("Response processing failed: {e:#}");
                self.channel
                    .send("An error occurred while processing your request. Please try again.")
                    .await?;
                self.messages.pop();
            }
        }

        Ok(())
    }

    async fn handle_skills_command(&mut self) -> anyhow::Result<()> {
        use std::fmt::Write;

        let mut output = String::from("Available skills:\n\n");

        for meta in self.registry.all_meta() {
            let _ = writeln!(output, "- {} — {}", meta.name, meta.description);
        }

        if let Some(memory) = &self.memory {
            match memory.sqlite().load_skill_usage().await {
                Ok(usage) if !usage.is_empty() => {
                    output.push_str("\nUsage statistics:\n\n");
                    for row in &usage {
                        let _ = writeln!(
                            output,
                            "- {}: {} invocations (last: {})",
                            row.skill_name, row.invocation_count, row.last_used_at,
                        );
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("failed to load skill usage: {e:#}"),
            }
        }

        self.channel.send(&output).await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_command(&mut self, args: &str) -> anyhow::Result<()> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        match parts.first().copied() {
            Some("stats") => self.handle_skill_stats().await,
            Some("versions") => self.handle_skill_versions(parts.get(1).copied()).await,
            Some("activate") => {
                self.handle_skill_activate(parts.get(1).copied(), parts.get(2).copied())
                    .await
            }
            Some("approve") => self.handle_skill_approve(parts.get(1).copied()).await,
            Some("reset") => self.handle_skill_reset(parts.get(1).copied()).await,
            _ => {
                self.channel
                    .send("Unknown /skill subcommand. Available: stats, versions, activate, approve, reset")
                    .await
            }
        }
    }

    #[cfg(not(feature = "self-learning"))]
    async fn handle_skill_command(&mut self, _args: &str) -> anyhow::Result<()> {
        self.channel
            .send("Self-learning feature is not enabled.")
            .await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_stats(&mut self) -> anyhow::Result<()> {
        use std::fmt::Write;

        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let stats = memory.sqlite().load_skill_outcome_stats().await?;
        if stats.is_empty() {
            return self.channel.send("No skill outcome data yet.").await;
        }

        let mut output = String::from("Skill outcome statistics:\n\n");
        #[allow(clippy::cast_precision_loss)]
        for row in &stats {
            let rate = if row.total == 0 {
                0.0
            } else {
                row.successes as f64 / row.total as f64 * 100.0
            };
            let _ = writeln!(
                output,
                "- {}: {} total, {} ok, {} fail ({rate:.0}%)",
                row.skill_name, row.total, row.successes, row.failures,
            );
        }

        self.channel.send(&output).await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_versions(&mut self, name: Option<&str>) -> anyhow::Result<()> {
        use std::fmt::Write;

        let Some(name) = name else {
            return self.channel.send("Usage: /skill versions <name>").await;
        };
        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        if versions.is_empty() {
            return self
                .channel
                .send(&format!("No versions found for \"{name}\"."))
                .await;
        }

        let mut output = format!("Versions for \"{name}\":\n\n");
        for v in &versions {
            let active_tag = if v.is_active { ", active" } else { "" };
            let _ = writeln!(
                output,
                "  v{} ({}{active_tag}) — success: {}, failure: {}",
                v.version, v.source, v.success_count, v.failure_count,
            );
        }

        self.channel.send(&output).await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_activate(
        &mut self,
        name: Option<&str>,
        version_str: Option<&str>,
    ) -> anyhow::Result<()> {
        let (Some(name), Some(ver_str)) = (name, version_str) else {
            return self
                .channel
                .send("Usage: /skill activate <name> <version>")
                .await;
        };
        let Ok(ver) = ver_str.parse::<i64>() else {
            return self.channel.send("Invalid version number.").await;
        };
        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        let Some(target) = versions.iter().find(|v| v.version == ver) else {
            return self
                .channel
                .send(&format!("Version {ver} not found for \"{name}\"."))
                .await;
        };

        memory
            .sqlite()
            .activate_skill_version(name, target.id)
            .await?;

        write_skill_file(&self.skill_paths, name, &target.description, &target.body).await?;

        self.channel
            .send(&format!("Activated v{ver} for \"{name}\"."))
            .await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_approve(&mut self, name: Option<&str>) -> anyhow::Result<()> {
        let Some(name) = name else {
            return self.channel.send("Usage: /skill approve <name>").await;
        };
        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        let pending = versions
            .iter()
            .rfind(|v| v.source == "auto" && !v.is_active);

        let Some(target) = pending else {
            return self
                .channel
                .send(&format!("No pending auto version for \"{name}\"."))
                .await;
        };

        memory
            .sqlite()
            .activate_skill_version(name, target.id)
            .await?;

        write_skill_file(&self.skill_paths, name, &target.description, &target.body).await?;

        self.channel
            .send(&format!(
                "Approved and activated v{} for \"{name}\".",
                target.version
            ))
            .await
    }

    #[cfg(feature = "self-learning")]
    async fn handle_skill_reset(&mut self, name: Option<&str>) -> anyhow::Result<()> {
        let Some(name) = name else {
            return self.channel.send("Usage: /skill reset <name>").await;
        };
        let Some(memory) = &self.memory else {
            return self.channel.send("Memory not available.").await;
        };

        let versions = memory.sqlite().load_skill_versions(name).await?;
        let Some(v1) = versions.iter().find(|v| v.version == 1) else {
            return self
                .channel
                .send(&format!("Original version not found for \"{name}\"."))
                .await;
        };

        memory.sqlite().activate_skill_version(name, v1.id).await?;

        write_skill_file(&self.skill_paths, name, &v1.description, &v1.body).await?;

        self.channel
            .send(&format!("Reset \"{name}\" to original v1."))
            .await
    }

    async fn handle_feedback(&mut self, input: &str) -> anyhow::Result<()> {
        #[cfg(feature = "self-learning")]
        {
            let (skill_name, feedback) = match input.split_once(' ') {
                Some((name, rest)) => (name.trim(), rest.trim().trim_matches('"')),
                None => {
                    return self
                        .channel
                        .send("Usage: /feedback <skill_name> <message>")
                        .await;
                }
            };

            if feedback.is_empty() {
                return self
                    .channel
                    .send("Usage: /feedback <skill_name> <message>")
                    .await;
            }

            let Some(memory) = &self.memory else {
                return self.channel.send("Memory not available.").await;
            };

            memory
                .sqlite()
                .record_skill_outcome(
                    skill_name,
                    None,
                    self.conversation_id,
                    "user_rejection",
                    Some(feedback),
                )
                .await?;

            if self.is_learning_enabled() {
                self.generate_improved_skill(skill_name, feedback, "", Some(feedback))
                    .await
                    .ok();
            }

            return self
                .channel
                .send(&format!("Feedback recorded for \"{skill_name}\"."))
                .await;
        }

        #[cfg(not(feature = "self-learning"))]
        {
            let _ = input;
            self.channel
                .send("Self-learning feature is not enabled.")
                .await
        }
    }

    #[cfg(feature = "mcp")]
    async fn handle_mcp_command(&mut self, args: &str) -> anyhow::Result<()> {
        let parts: Vec<&str> = args.split_whitespace().collect();
        match parts.first().copied() {
            Some("add") => self.handle_mcp_add(&parts[1..]).await,
            Some("list") => self.handle_mcp_list().await,
            Some("tools") => self.handle_mcp_tools(parts.get(1).copied()).await,
            Some("remove") => self.handle_mcp_remove(parts.get(1).copied()).await,
            _ => self.channel.send("Usage: /mcp add|list|tools|remove").await,
        }
    }

    #[cfg(feature = "mcp")]
    async fn handle_mcp_add(&mut self, args: &[&str]) -> anyhow::Result<()> {
        if args.len() < 2 {
            return self
                .channel
                .send("Usage: /mcp add <id> <command> [args...] | /mcp add <id> <url>")
                .await;
        }

        let Some(ref manager) = self.mcp_manager else {
            return self.channel.send("MCP is not enabled.").await;
        };

        let target = args[1];
        let is_url = target.starts_with("http://") || target.starts_with("https://");

        // SEC-MCP-01: validate command against allowlist (stdio only)
        if !is_url
            && !self.mcp_allowed_commands.is_empty()
            && !self.mcp_allowed_commands.iter().any(|c| c == target)
        {
            return self
                .channel
                .send(&format!(
                    "Command '{target}' is not allowed. Permitted: {}",
                    self.mcp_allowed_commands.join(", ")
                ))
                .await;
        }

        // SEC-MCP-03: enforce server limit
        let current_count = manager.list_servers().await.len();
        if current_count >= self.mcp_max_dynamic {
            return self
                .channel
                .send(&format!(
                    "Server limit reached ({}/{}).",
                    current_count, self.mcp_max_dynamic
                ))
                .await;
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
                self.mcp_tools.extend(tools);
                self.sync_mcp_registry().await;
                self.channel
                    .send(&format!(
                        "Connected MCP server '{}' ({count} tool(s))",
                        entry.id
                    ))
                    .await
            }
            Err(e) => {
                tracing::warn!(server_id = entry.id, "MCP add failed: {e:#}");
                self.channel
                    .send(&format!("Failed to connect server '{}': {e}", entry.id))
                    .await
            }
        }
    }

    #[cfg(feature = "mcp")]
    async fn handle_mcp_list(&mut self) -> anyhow::Result<()> {
        use std::fmt::Write;

        let Some(ref manager) = self.mcp_manager else {
            return self.channel.send("MCP is not enabled.").await;
        };

        let server_ids = manager.list_servers().await;
        if server_ids.is_empty() {
            return self.channel.send("No MCP servers connected.").await;
        }

        let mut output = String::from("Connected MCP servers:\n");
        let mut total = 0usize;
        for id in &server_ids {
            let count = self.mcp_tools.iter().filter(|t| t.server_id == *id).count();
            total += count;
            let _ = writeln!(output, "- {id} ({count} tools)");
        }
        let _ = write!(output, "Total: {total} tool(s)");

        self.channel.send(&output).await
    }

    #[cfg(feature = "mcp")]
    async fn handle_mcp_tools(&mut self, server_id: Option<&str>) -> anyhow::Result<()> {
        use std::fmt::Write;

        let Some(server_id) = server_id else {
            return self.channel.send("Usage: /mcp tools <server_id>").await;
        };

        let tools: Vec<_> = self
            .mcp_tools
            .iter()
            .filter(|t| t.server_id == server_id)
            .collect();

        if tools.is_empty() {
            return self
                .channel
                .send(&format!("No tools found for server '{server_id}'."))
                .await;
        }

        let mut output = format!("Tools for '{server_id}' ({} total):\n", tools.len());
        for t in &tools {
            if t.description.is_empty() {
                let _ = writeln!(output, "- {}", t.name);
            } else {
                let _ = writeln!(output, "- {} — {}", t.name, t.description);
            }
        }
        self.channel.send(&output).await
    }

    #[cfg(feature = "mcp")]
    async fn handle_mcp_remove(&mut self, server_id: Option<&str>) -> anyhow::Result<()> {
        let Some(server_id) = server_id else {
            return self.channel.send("Usage: /mcp remove <id>").await;
        };

        let Some(ref manager) = self.mcp_manager else {
            return self.channel.send("MCP is not enabled.").await;
        };

        match manager.remove_server(server_id).await {
            Ok(()) => {
                let before = self.mcp_tools.len();
                self.mcp_tools.retain(|t| t.server_id != server_id);
                let removed = before - self.mcp_tools.len();
                self.sync_mcp_registry().await;
                self.channel
                    .send(&format!(
                        "Disconnected MCP server '{server_id}' (removed {removed} tools)"
                    ))
                    .await
            }
            Err(e) => {
                tracing::warn!(server_id, "MCP remove failed: {e:#}");
                self.channel
                    .send(&format!("Failed to remove server '{server_id}': {e}"))
                    .await
            }
        }
    }

    async fn reload_skills(&mut self) {
        let new_registry = SkillRegistry::load(&self.skill_paths);
        if new_registry.fingerprint() == self.registry.fingerprint() {
            return;
        }
        self.registry = new_registry;

        let all_meta = self.registry.all_meta();
        let provider = self.provider.clone();
        let embed_fn = |text: &str| -> zeph_skills::matcher::EmbedFuture {
            let owned = text.to_owned();
            let p = provider.clone();
            Box::pin(async move { p.embed(&owned).await })
        };

        let needs_inmemory_rebuild = !self
            .matcher
            .as_ref()
            .is_some_and(SkillMatcherBackend::is_qdrant);

        if needs_inmemory_rebuild {
            self.matcher = SkillMatcher::new(&all_meta, embed_fn)
                .await
                .map(SkillMatcherBackend::InMemory);
        } else if let Some(ref mut backend) = self.matcher
            && let Err(e) = backend
                .sync(&all_meta, &self.embedding_model, embed_fn)
                .await
        {
            tracing::warn!("failed to sync skill embeddings: {e:#}");
        }

        let all_skills: Vec<Skill> = self
            .registry
            .all_meta()
            .iter()
            .filter_map(|m| self.registry.get_skill(&m.name).ok())
            .collect();
        let skills_prompt = format_skills_prompt(&all_skills, std::env::consts::OS);
        let system_prompt = build_system_prompt(&skills_prompt);
        if let Some(msg) = self.messages.first_mut() {
            msg.content = system_prompt;
        }

        tracing::info!("reloaded {} skill(s)", self.registry.all_meta().len());
    }

    async fn rebuild_system_prompt(&mut self, query: &str) {
        let all_meta = self.registry.all_meta();
        let matched_indices: Vec<usize> = if let Some(matcher) = &self.matcher {
            let provider = self.provider.clone();
            matcher
                .match_skills(&all_meta, query, self.max_active_skills, |text| {
                    let owned = text.to_owned();
                    let p = provider.clone();
                    Box::pin(async move { p.embed(&owned).await })
                })
                .await
        } else {
            (0..all_meta.len()).collect()
        };

        self.active_skill_names = matched_indices
            .iter()
            .filter_map(|&i| all_meta.get(i).map(|m| m.name.clone()))
            .collect();

        if !self.active_skill_names.is_empty()
            && let Some(memory) = &self.memory
        {
            let names: Vec<&str> = self.active_skill_names.iter().map(String::as_str).collect();
            if let Err(e) = memory.sqlite().record_skill_usage(&names).await {
                tracing::warn!("failed to record skill usage: {e:#}");
            }
        }

        let active_skills: Vec<Skill> = self
            .active_skill_names
            .iter()
            .filter_map(|name| self.registry.get_skill(name).ok())
            .collect();

        let skills_prompt = format_skills_prompt(&active_skills, std::env::consts::OS);
        #[allow(unused_mut)]
        let mut system_prompt = build_system_prompt(&skills_prompt);

        #[cfg(feature = "mcp")]
        {
            let matched_tools = self.match_mcp_tools(query).await;
            if !matched_tools.is_empty() {
                let tool_names: Vec<&str> = matched_tools.iter().map(|t| t.name.as_str()).collect();
                tracing::debug!(
                    skills = ?self.active_skill_names,
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

        if let Some(msg) = self.messages.first_mut() {
            msg.content = system_prompt;
        }
    }

    #[cfg(feature = "mcp")]
    async fn match_mcp_tools(&self, query: &str) -> Vec<zeph_mcp::McpTool> {
        let Some(ref registry) = self.mcp_registry else {
            return self.mcp_tools.clone();
        };
        let provider = self.provider.clone();
        registry
            .search(query, self.max_active_skills, |text| {
                let owned = text.to_owned();
                let p = provider.clone();
                Box::pin(async move { p.embed(&owned).await })
            })
            .await
    }

    #[cfg(feature = "mcp")]
    async fn sync_mcp_registry(&mut self) {
        let Some(ref mut registry) = self.mcp_registry else {
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
            .sync(&self.mcp_tools, &self.embedding_model, embed_fn)
            .await
        {
            tracing::warn!("failed to sync MCP tool registry: {e:#}");
        }
    }

    async fn process_response(&mut self) -> anyhow::Result<()> {
        for _ in 0..MAX_SHELL_ITERATIONS {
            self.channel.send_typing().await?;

            let Some(response) = self.call_llm_with_timeout().await? else {
                return Ok(());
            };

            if response.trim().is_empty() {
                tracing::warn!("received empty response from LLM, skipping");
                self.record_skill_outcomes("empty_response", None).await;

                #[cfg(feature = "self-learning")]
                if !self.reflection_used
                    && self
                        .attempt_self_reflection("LLM returned empty response", "")
                        .await?
                {
                    return Ok(());
                }

                self.channel
                    .send("Received an empty response. Please try again.")
                    .await?;
                return Ok(());
            }

            self.messages.push(Message {
                role: Role::Assistant,
                content: response.clone(),
            });
            self.persist_message(Role::Assistant, &response).await;

            let result = self.tool_executor.execute(&response).await;
            if !self.handle_tool_result(&response, result).await? {
                return Ok(());
            }
        }

        Ok(())
    }

    async fn call_llm_with_timeout(&mut self) -> anyhow::Result<Option<String>> {
        let llm_timeout = std::time::Duration::from_secs(self.timeouts.llm_seconds);

        if self.provider.supports_streaming() {
            if let Ok(r) =
                tokio::time::timeout(llm_timeout, self.process_response_streaming()).await
            {
                Ok(Some(r?))
            } else {
                self.channel
                    .send("LLM request timed out. Please try again.")
                    .await?;
                Ok(None)
            }
        } else {
            match tokio::time::timeout(llm_timeout, self.provider.chat(&self.messages)).await {
                Ok(Ok(resp)) => {
                    let display = self.maybe_redact(&resp);
                    self.channel.send(&display).await?;
                    Ok(Some(resp))
                }
                Ok(Err(e)) => Err(e),
                Err(_) => {
                    self.channel
                        .send("LLM request timed out. Please try again.")
                        .await?;
                    Ok(None)
                }
            }
        }
    }

    /// Returns `true` if the tool loop should continue.
    async fn handle_tool_result(
        &mut self,
        response: &str,
        result: Result<Option<ToolOutput>, ToolError>,
    ) -> anyhow::Result<bool> {
        match result {
            Ok(Some(output)) => {
                if output.summary.trim().is_empty() {
                    tracing::warn!("tool execution returned empty output");
                    self.record_skill_outcomes("success", None).await;
                    return Ok(false);
                }

                if output.summary.contains("[error]") || output.summary.contains("[exit code") {
                    self.record_skill_outcomes("tool_failure", Some(&output.summary))
                        .await;

                    #[cfg(feature = "self-learning")]
                    if !self.reflection_used
                        && self
                            .attempt_self_reflection(&output.summary, &output.summary)
                            .await?
                    {
                        return Ok(false);
                    }
                } else {
                    self.record_skill_outcomes("success", None).await;
                }

                let formatted_output = format!("[tool output]\n```\n{output}\n```");
                let display = self.maybe_redact(&formatted_output);
                self.channel.send(&display).await?;

                self.messages.push(Message {
                    role: Role::User,
                    content: formatted_output.clone(),
                });
                self.persist_message(Role::User, &formatted_output).await;
                Ok(true)
            }
            Ok(None) => {
                self.record_skill_outcomes("success", None).await;
                Ok(false)
            }
            Err(ToolError::Blocked { command }) => {
                tracing::warn!("blocked command: {command}");
                self.channel
                    .send("This command is blocked by security policy.")
                    .await?;
                Ok(false)
            }
            Err(ToolError::ConfirmationRequired { command }) => {
                let prompt = format!("Allow command: {command}?");
                if self.channel.confirm(&prompt).await? {
                    if let Ok(Some(out)) = self.tool_executor.execute_confirmed(response).await {
                        let formatted = format!("[tool output]\n```\n{out}\n```");
                        let display = self.maybe_redact(&formatted);
                        self.channel.send(&display).await?;
                        self.messages.push(Message {
                            role: Role::User,
                            content: formatted.clone(),
                        });
                        self.persist_message(Role::User, &formatted).await;
                    }
                } else {
                    self.channel.send("Command cancelled.").await?;
                }
                Ok(false)
            }
            Err(ToolError::SandboxViolation { path }) => {
                tracing::warn!("sandbox violation: {path}");
                self.channel
                    .send("Command targets a path outside the sandbox.")
                    .await?;
                Ok(false)
            }
            Err(e) => {
                let err_str = format!("{e:#}");
                tracing::error!("tool execution error: {err_str}");
                self.record_skill_outcomes("tool_failure", Some(&err_str))
                    .await;

                #[cfg(feature = "self-learning")]
                if !self.reflection_used && self.attempt_self_reflection(&err_str, "").await? {
                    return Ok(false);
                }

                self.channel
                    .send("Tool execution failed. Please try a different approach.")
                    .await?;
                Ok(false)
            }
        }
    }

    async fn process_response_streaming(&mut self) -> anyhow::Result<String> {
        let mut stream = self.provider.chat_stream(&self.messages).await?;
        let mut response = String::with_capacity(2048);

        while let Some(chunk_result) = stream.next().await {
            let chunk: String = chunk_result?;
            response.push_str(&chunk);
            let display = self.maybe_redact(&chunk);
            self.channel.send_chunk(&display).await?;
        }

        self.channel.flush_chunks().await?;
        Ok(response)
    }

    fn maybe_redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.security.redact_secrets {
            redact_secrets(text)
        } else {
            std::borrow::Cow::Borrowed(text)
        }
    }

    async fn persist_message(&self, role: Role, content: &str) {
        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return;
        };
        if let Err(e) = memory.remember(cid, role_str(role), content).await {
            tracing::error!("failed to persist message: {e:#}");
            return;
        }

        self.check_summarization().await;
    }

    async fn check_summarization(&self) {
        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return;
        };

        let count = match memory.message_count(cid).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to get message count: {e:#}");
                return;
            }
        };

        let count_usize = match usize::try_from(count) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("message count overflow: {e:#}");
                return;
            }
        };

        if count_usize > self.summarization_threshold {
            let batch_size = self.summarization_threshold / 2;
            match memory.summarize(cid, batch_size).await {
                Ok(Some(summary_id)) => {
                    tracing::info!("created summary {summary_id} for conversation {cid}");
                }
                Ok(None) => {
                    tracing::debug!("no summarization needed");
                }
                Err(e) => {
                    tracing::error!("summarization failed: {e:#}");
                }
            }
        }
    }

    // --- Self-learning integration ---

    #[cfg(feature = "self-learning")]
    fn is_learning_enabled(&self) -> bool {
        self.learning_config.as_ref().is_some_and(|c| c.enabled)
    }

    #[cfg(not(feature = "self-learning"))]
    #[allow(dead_code, clippy::unused_self)]
    fn is_learning_enabled(&self) -> bool {
        false
    }

    #[cfg(feature = "self-learning")]
    async fn record_skill_outcomes(&self, outcome: &str, error_context: Option<&str>) {
        if self.active_skill_names.is_empty() {
            return;
        }
        let Some(memory) = &self.memory else { return };
        if let Err(e) = memory
            .sqlite()
            .record_skill_outcomes_batch(
                &self.active_skill_names,
                self.conversation_id,
                outcome,
                error_context,
            )
            .await
        {
            tracing::warn!("failed to record skill outcomes: {e:#}");
        }

        if outcome != "success" {
            for name in &self.active_skill_names {
                self.check_rollback(name).await;
            }
        }
    }

    #[cfg(not(feature = "self-learning"))]
    #[allow(clippy::unused_async)]
    async fn record_skill_outcomes(&self, _outcome: &str, _error_context: Option<&str>) {}

    #[cfg(feature = "self-learning")]
    async fn attempt_self_reflection(
        &mut self,
        error_context: &str,
        tool_output: &str,
    ) -> anyhow::Result<bool> {
        if self.reflection_used || !self.is_learning_enabled() {
            return Ok(false);
        }
        self.reflection_used = true;

        let skill_name = self.active_skill_names.first().cloned();

        let Some(name) = skill_name else {
            return Ok(false);
        };

        let Ok(skill) = self.registry.get_skill(&name) else {
            return Ok(false);
        };

        let prompt = zeph_skills::evolution::build_reflection_prompt(
            skill.name(),
            &skill.body,
            error_context,
            tool_output,
        );

        self.messages.push(Message {
            role: Role::User,
            content: prompt,
        });

        let messages_before = self.messages.len();
        // Box::pin to break async recursion cycle (process_response -> attempt_self_reflection -> process_response)
        Box::pin(self.process_response()).await?;
        let retry_succeeded = self.messages.len() > messages_before;

        if retry_succeeded {
            let successful_response = self
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.content.clone())
                .unwrap_or_default();

            self.generate_improved_skill(&name, error_context, &successful_response, None)
                .await
                .ok();
        }

        Ok(retry_succeeded)
    }

    #[cfg(feature = "self-learning")]
    #[allow(clippy::cast_precision_loss)]
    async fn generate_improved_skill(
        &self,
        skill_name: &str,
        error_context: &str,
        successful_response: &str,
        user_feedback: Option<&str>,
    ) -> anyhow::Result<()> {
        if !self.is_learning_enabled() {
            return Ok(());
        }

        let Some(memory) = &self.memory else {
            return Ok(());
        };
        let Some(config) = self.learning_config.as_ref() else {
            return Ok(());
        };

        let skill = self.registry.get_skill(skill_name)?;

        memory
            .sqlite()
            .ensure_skill_version_exists(skill_name, &skill.body, skill.description())
            .await?;

        if !self
            .check_improvement_allowed(memory, config, skill_name, user_feedback)
            .await?
        {
            return Ok(());
        }

        let generated_body = self
            .call_improvement_llm(
                skill_name,
                &skill.body,
                error_context,
                successful_response,
                user_feedback,
            )
            .await?;
        let generated_body = generated_body.trim();

        if generated_body.is_empty()
            || !zeph_skills::evolution::validate_body_size(&skill.body, generated_body)
        {
            tracing::warn!("improvement for {skill_name} rejected (empty or too large)");
            return Ok(());
        }

        self.store_improved_version(
            memory,
            config,
            skill_name,
            generated_body,
            skill.description(),
            error_context,
        )
        .await
    }

    #[cfg(feature = "self-learning")]
    #[allow(clippy::cast_precision_loss)]
    async fn check_improvement_allowed(
        &self,
        memory: &SemanticMemory<P>,
        config: &LearningConfig,
        skill_name: &str,
        user_feedback: Option<&str>,
    ) -> anyhow::Result<bool> {
        if let Some(last_time) = memory.sqlite().last_improvement_time(skill_name).await?
            && let Ok(last) = chrono_parse_sqlite(&last_time)
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let elapsed_minutes = (now.saturating_sub(last)) / 60;
            if elapsed_minutes < config.cooldown_minutes {
                tracing::debug!(
                    "cooldown active for {skill_name}: {elapsed_minutes}m < {}m",
                    config.cooldown_minutes
                );
                return Ok(false);
            }
        }

        if user_feedback.is_none()
            && let Some(metrics) = memory.sqlite().skill_metrics(skill_name).await?
        {
            if metrics.failures < i64::from(config.min_failures) {
                return Ok(false);
            }
            let rate = if metrics.total == 0 {
                1.0
            } else {
                metrics.successes as f64 / metrics.total as f64
            };
            if rate >= config.improve_threshold {
                return Ok(false);
            }
        }

        Ok(true)
    }

    #[cfg(feature = "self-learning")]
    async fn call_improvement_llm(
        &self,
        skill_name: &str,
        original_body: &str,
        error_context: &str,
        successful_response: &str,
        user_feedback: Option<&str>,
    ) -> anyhow::Result<String> {
        let prompt = zeph_skills::evolution::build_improvement_prompt(
            skill_name,
            original_body,
            error_context,
            successful_response,
            user_feedback,
        );

        let messages = vec![
            Message {
                role: Role::System,
                content:
                    "You are a skill improvement assistant. Output only the improved skill body."
                        .into(),
            },
            Message {
                role: Role::User,
                content: prompt,
            },
        ];

        self.provider.chat(&messages).await
    }

    #[cfg(feature = "self-learning")]
    async fn store_improved_version(
        &self,
        memory: &SemanticMemory<P>,
        config: &LearningConfig,
        skill_name: &str,
        generated_body: &str,
        description: &str,
        error_context: &str,
    ) -> anyhow::Result<()> {
        let active = memory.sqlite().active_skill_version(skill_name).await?;
        let predecessor_id = active.as_ref().map(|v| v.id);

        let next_ver = memory.sqlite().next_skill_version(skill_name).await?;
        let version_id = memory
            .sqlite()
            .save_skill_version(
                skill_name,
                next_ver,
                generated_body,
                description,
                "auto",
                Some(error_context),
                predecessor_id,
            )
            .await?;

        tracing::info!("generated v{next_ver} for skill {skill_name} (id={version_id})");

        if config.auto_activate {
            memory
                .sqlite()
                .activate_skill_version(skill_name, version_id)
                .await?;
            write_skill_file(&self.skill_paths, skill_name, description, generated_body).await?;
            tracing::info!("auto-activated v{next_ver} for {skill_name}");
        }

        memory
            .sqlite()
            .prune_skill_versions(skill_name, config.max_versions)
            .await?;

        Ok(())
    }

    #[cfg(feature = "self-learning")]
    #[allow(clippy::cast_precision_loss)]
    async fn check_rollback(&self, skill_name: &str) {
        if !self.is_learning_enabled() {
            return;
        }
        let Some(memory) = &self.memory else { return };
        let Some(config) = &self.learning_config else {
            return;
        };
        let Ok(Some(metrics)) = memory.sqlite().skill_metrics(skill_name).await else {
            return;
        };

        if metrics.total < i64::from(config.min_evaluations) {
            return;
        }

        let rate = if metrics.total == 0 {
            1.0
        } else {
            metrics.successes as f64 / metrics.total as f64
        };

        if rate >= config.rollback_threshold {
            return;
        }

        let Ok(Some(active)) = memory.sqlite().active_skill_version(skill_name).await else {
            return;
        };
        if active.source != "auto" {
            return;
        }
        let Ok(Some(predecessor)) = memory.sqlite().predecessor_version(active.id).await else {
            return;
        };

        tracing::warn!(
            "rolling back {skill_name} from v{} to v{} (rate: {rate:.0}%)",
            active.version,
            predecessor.version,
        );

        if memory
            .sqlite()
            .activate_skill_version(skill_name, predecessor.id)
            .await
            .is_ok()
        {
            write_skill_file(
                &self.skill_paths,
                skill_name,
                &predecessor.description,
                &predecessor.body,
            )
            .await
            .ok();
        }
    }
}

#[cfg(feature = "self-learning")]
async fn write_skill_file(
    skill_paths: &[PathBuf],
    skill_name: &str,
    description: &str,
    body: &str,
) -> anyhow::Result<()> {
    if skill_name.contains('/') || skill_name.contains('\\') || skill_name.contains("..") {
        anyhow::bail!("invalid skill name: {skill_name}");
    }
    for base in skill_paths {
        let skill_dir = base.join(skill_name);
        let skill_file = skill_dir.join("SKILL.md");
        if skill_file.exists() {
            let content =
                format!("---\nname: {skill_name}\ndescription: {description}\n---\n{body}\n");
            tokio::fs::write(&skill_file, content).await?;
            return Ok(());
        }
    }
    anyhow::bail!("skill directory not found for {skill_name}")
}

/// Naive parser for `SQLite` datetime strings (e.g. "2024-01-15 10:30:00") to Unix seconds.
#[cfg(feature = "self-learning")]
fn chrono_parse_sqlite(s: &str) -> Result<u64, ()> {
    // Format: "YYYY-MM-DD HH:MM:SS"
    let parts: Vec<&str> = s.split(&['-', ' ', ':'][..]).collect();
    if parts.len() < 6 {
        return Err(());
    }
    let year: u64 = parts[0].parse().map_err(|_| ())?;
    let month: u64 = parts[1].parse().map_err(|_| ())?;
    let day: u64 = parts[2].parse().map_err(|_| ())?;
    let hour: u64 = parts[3].parse().map_err(|_| ())?;
    let min: u64 = parts[4].parse().map_err(|_| ())?;
    let sec: u64 = parts[5].parse().map_err(|_| ())?;

    // Rough approximation (sufficient for cooldown comparison)
    let days_approx = (year - 1970) * 365 + (month - 1) * 30 + (day - 1);
    Ok(days_approx * 86400 + hour * 3600 + min * 60 + sec)
}

async fn shutdown_signal(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn recv_skill_event(rx: &mut Option<mpsc::Receiver<SkillEvent>>) -> Option<SkillEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
#[cfg(feature = "self-learning")]
mod tests {
    use super::*;

    #[test]
    fn chrono_parse_valid_datetime() {
        let secs = chrono_parse_sqlite("2024-01-15 10:30:00").unwrap();
        assert!(secs > 0);
    }

    #[test]
    fn chrono_parse_ordering_preserved() {
        let earlier = chrono_parse_sqlite("2024-01-15 10:00:00").unwrap();
        let later = chrono_parse_sqlite("2024-01-15 11:00:00").unwrap();
        assert!(later > earlier);
    }

    #[test]
    fn chrono_parse_different_days() {
        let day1 = chrono_parse_sqlite("2024-06-01 00:00:00").unwrap();
        let day2 = chrono_parse_sqlite("2024-06-02 00:00:00").unwrap();
        assert_eq!(day2 - day1, 86400);
    }

    #[test]
    fn chrono_parse_invalid_format() {
        assert!(chrono_parse_sqlite("not-a-date").is_err());
        assert!(chrono_parse_sqlite("").is_err());
        assert!(chrono_parse_sqlite("2024-01").is_err());
    }

    #[tokio::test]
    async fn write_skill_file_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = write_skill_file(
            &[dir.path().to_path_buf()],
            "nonexistent-skill",
            "desc",
            "body",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn write_skill_file_updates_existing() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "old content").unwrap();

        write_skill_file(
            &[dir.path().to_path_buf()],
            "test-skill",
            "new desc",
            "new body",
        )
        .await
        .unwrap();

        let content = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(content.contains("new body"));
        assert!(content.contains("new desc"));
    }

    #[tokio::test]
    async fn write_skill_file_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            write_skill_file(&[dir.path().to_path_buf()], "../evil", "d", "b")
                .await
                .is_err()
        );
        assert!(
            write_skill_file(&[dir.path().to_path_buf()], "a/b", "d", "b")
                .await
                .is_err()
        );
        assert!(
            write_skill_file(&[dir.path().to_path_buf()], "a\\b", "d", "b")
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod agent_tests {
    use super::*;
    use crate::channel::ChannelMessage;
    use std::sync::{Arc, Mutex};
    use zeph_llm::provider::ChatStream;

    #[derive(Clone)]
    struct MockProvider {
        responses: Arc<Mutex<Vec<String>>>,
        streaming: bool,
        embeddings: bool,
    }

    impl MockProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
                streaming: false,
                embeddings: false,
            }
        }

        fn with_streaming(mut self) -> Self {
            self.streaming = true;
            self
        }
    }

    impl LlmProvider for MockProvider {
        async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok("default response".to_string())
            } else {
                Ok(responses.remove(0))
            }
        }

        async fn chat_stream(&self, messages: &[Message]) -> anyhow::Result<ChatStream> {
            let response = self.chat(messages).await?;
            let chunks: Vec<_> = response.chars().map(|c| c.to_string()).map(Ok).collect();
            Ok(Box::pin(tokio_stream::iter(chunks)))
        }

        fn supports_streaming(&self) -> bool {
            self.streaming
        }

        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            if self.embeddings {
                Ok(vec![0.1, 0.2, 0.3])
            } else {
                anyhow::bail!("embeddings not supported")
            }
        }

        fn supports_embeddings(&self) -> bool {
            self.embeddings
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }

    struct MockChannel {
        messages: Arc<Mutex<Vec<String>>>,
        sent: Arc<Mutex<Vec<String>>>,
        chunks: Arc<Mutex<Vec<String>>>,
        confirmations: Arc<Mutex<Vec<bool>>>,
    }

    impl MockChannel {
        fn new(messages: Vec<String>) -> Self {
            Self {
                messages: Arc::new(Mutex::new(messages)),
                sent: Arc::new(Mutex::new(Vec::new())),
                chunks: Arc::new(Mutex::new(Vec::new())),
                confirmations: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_confirmations(mut self, confirmations: Vec<bool>) -> Self {
            self.confirmations = Arc::new(Mutex::new(confirmations));
            self
        }
    }

    impl Channel for MockChannel {
        async fn recv(&mut self) -> anyhow::Result<Option<ChannelMessage>> {
            let mut msgs = self.messages.lock().unwrap();
            if msgs.is_empty() {
                Ok(None)
            } else {
                Ok(Some(ChannelMessage {
                    text: msgs.remove(0),
                }))
            }
        }

        async fn send(&mut self, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }

        async fn send_chunk(&mut self, chunk: &str) -> anyhow::Result<()> {
            self.chunks.lock().unwrap().push(chunk.to_string());
            Ok(())
        }

        async fn flush_chunks(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn confirm(&mut self, _prompt: &str) -> anyhow::Result<bool> {
            let mut confs = self.confirmations.lock().unwrap();
            Ok(if confs.is_empty() {
                true
            } else {
                confs.remove(0)
            })
        }
    }

    struct MockToolExecutor {
        outputs: Arc<Mutex<Vec<Result<Option<ToolOutput>, ToolError>>>>,
    }

    impl MockToolExecutor {
        fn new(outputs: Vec<Result<Option<ToolOutput>, ToolError>>) -> Self {
            Self {
                outputs: Arc::new(Mutex::new(outputs)),
            }
        }

        fn no_tools() -> Self {
            Self::new(vec![Ok(None)])
        }
    }

    impl ToolExecutor for MockToolExecutor {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            let mut outputs = self.outputs.lock().unwrap();
            if outputs.is_empty() {
                Ok(None)
            } else {
                outputs.remove(0)
            }
        }
    }

    fn create_test_registry() -> SkillRegistry {
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join("test-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: test-skill\ndescription: A test skill\n---\nTest skill body",
        )
        .unwrap();
        SkillRegistry::load(&[temp_dir.path().to_path_buf()])
    }

    #[tokio::test]
    async fn agent_new_initializes_with_system_prompt() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        assert_eq!(agent.messages.len(), 1);
        assert_eq!(agent.messages[0].role, Role::System);
        assert!(!agent.messages[0].content.is_empty());
    }

    #[tokio::test]
    async fn agent_with_embedding_model_sets_model() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_embedding_model("test-embed-model".to_string());

        assert_eq!(agent.embedding_model, "test-embed-model");
    }

    #[tokio::test]
    async fn agent_with_shutdown_sets_receiver() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (_tx, rx) = watch::channel(false);

        let _agent = Agent::new(provider, channel, registry, None, 5, executor).with_shutdown(rx);
    }

    #[tokio::test]
    async fn agent_with_security_sets_config() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let security = SecurityConfig {
            redact_secrets: true,
            ..Default::default()
        };
        let timeouts = TimeoutConfig {
            llm_seconds: 60,
            ..Default::default()
        };

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_security(security, timeouts);

        assert!(agent.security.redact_secrets);
        assert_eq!(agent.timeouts.llm_seconds, 60);
    }

    #[tokio::test]
    async fn agent_run_handles_empty_channel() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn agent_run_processes_user_message() {
        let provider = MockProvider::new(vec!["test response".to_string()]);
        let channel = MockChannel::new(vec!["hello".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
        assert_eq!(agent.messages.len(), 3);
        assert_eq!(agent.messages[1].role, Role::User);
        assert_eq!(agent.messages[1].content, "hello");
        assert_eq!(agent.messages[2].role, Role::Assistant);
    }

    #[tokio::test]
    async fn agent_run_handles_shutdown_signal() {
        let provider = MockProvider::new(vec![]);
        let (tx, rx) = watch::channel(false);
        let channel = MockChannel::new(vec!["should not process".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent =
            Agent::new(provider, channel, registry, None, 5, executor).with_shutdown(rx);

        tx.send(true).unwrap();

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn agent_handles_skills_command() {
        let provider = MockProvider::new(vec![]);
        let _channel = MockChannel::new(vec!["/skills".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent_channel = MockChannel::new(vec!["/skills".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(!sent_msgs.is_empty());
        assert!(sent_msgs[0].contains("Available skills"));
    }

    #[tokio::test]
    async fn agent_process_response_handles_empty_response() {
        let provider = MockProvider::new(vec!["".to_string()]);
        let _channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent_channel = MockChannel::new(vec!["test".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(sent_msgs.iter().any(|m| m.contains("empty response")));
    }

    #[tokio::test]
    async fn agent_handles_tool_execution_success() {
        let provider = MockProvider::new(vec!["response with tool".to_string()]);
        let _channel = MockChannel::new(vec!["execute tool".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
            summary: "tool executed successfully".to_string(),
            blocks_executed: 1,
        }))]);

        let agent_channel = MockChannel::new(vec!["execute tool".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(
            sent_msgs
                .iter()
                .any(|m| m.contains("tool executed successfully"))
        );
    }

    #[tokio::test]
    async fn agent_handles_tool_blocked_error() {
        let provider = MockProvider::new(vec!["run blocked command".to_string()]);
        let _channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::Blocked {
            command: "rm -rf /".to_string(),
        })]);

        let agent_channel = MockChannel::new(vec!["test".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(
            sent_msgs
                .iter()
                .any(|m| m.contains("blocked by security policy"))
        );
    }

    #[tokio::test]
    async fn agent_handles_tool_sandbox_violation() {
        let provider = MockProvider::new(vec!["access forbidden path".to_string()]);
        let _channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::SandboxViolation {
            path: "/etc/passwd".to_string(),
        })]);

        let agent_channel = MockChannel::new(vec!["test".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(sent_msgs.iter().any(|m| m.contains("outside the sandbox")));
    }

    #[tokio::test]
    async fn agent_handles_tool_confirmation_approved() {
        let provider = MockProvider::new(vec!["needs confirmation".to_string()]);
        let _channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::ConfirmationRequired {
            command: "dangerous command".to_string(),
        })]);

        let agent_channel =
            MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![true]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(!sent_msgs.is_empty());
    }

    #[tokio::test]
    async fn agent_handles_tool_confirmation_denied() {
        let provider = MockProvider::new(vec!["needs confirmation".to_string()]);
        let _channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::ConfirmationRequired {
            command: "dangerous command".to_string(),
        })]);

        let agent_channel =
            MockChannel::new(vec!["test".to_string()]).with_confirmations(vec![false]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(sent_msgs.iter().any(|m| m.contains("Command cancelled")));
    }

    #[tokio::test]
    async fn agent_handles_streaming_response() {
        let provider = MockProvider::new(vec!["streaming response".to_string()]).with_streaming();
        let _channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent_channel = MockChannel::new(vec!["test".to_string()]);
        let chunks = agent_channel.chunks.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_chunks = chunks.lock().unwrap();
        assert!(!sent_chunks.is_empty());
    }

    #[tokio::test]
    async fn agent_maybe_redact_enabled() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let security = SecurityConfig {
            redact_secrets: true,
            ..Default::default()
        };

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_security(security, TimeoutConfig::default());

        let text = "token: sk-abc123secret";
        let redacted = agent.maybe_redact(text);
        assert_ne!(AsRef::<str>::as_ref(&redacted), text);
    }

    #[tokio::test]
    async fn agent_maybe_redact_disabled() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let security = SecurityConfig {
            redact_secrets: false,
            ..Default::default()
        };

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_security(security, TimeoutConfig::default());

        let text = "password=secret123";
        let redacted = agent.maybe_redact(text);
        assert_eq!(AsRef::<str>::as_ref(&redacted), text);
    }

    #[tokio::test]
    async fn agent_handles_multiple_messages() {
        let provider = MockProvider::new(vec![
            "first response".to_string(),
            "second response".to_string(),
        ]);
        let channel = MockChannel::new(vec!["first".to_string(), "second".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(None), Ok(None)]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
        assert_eq!(agent.messages.len(), 5);
        assert_eq!(agent.messages[1].content, "first");
        assert_eq!(agent.messages[3].content, "second");
    }

    #[tokio::test]
    async fn agent_handles_tool_output_with_error_marker() {
        let provider = MockProvider::new(vec!["response".to_string(), "retry".to_string()]);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![
            Ok(Some(ToolOutput {
                summary: "[error] command failed [exit code 1]".to_string(),
                blocks_executed: 1,
            })),
            Ok(None),
        ]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn agent_handles_empty_tool_output() {
        let provider = MockProvider::new(vec!["response".to_string()]);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
            summary: "   ".to_string(),
            blocks_executed: 1,
        }))]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn shutdown_signal_helper_returns_on_true() {
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut rx_clone = rx;
            shutdown_signal(&mut rx_clone).await;
        });

        tx.send(true).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), handle).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn recv_skill_event_returns_none_when_no_receiver() {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            recv_skill_event(&mut None),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recv_skill_event_receives_from_channel() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(SkillEvent::Changed).await.unwrap();

        let result = recv_skill_event(&mut Some(rx)).await;
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn agent_with_skill_reload_sets_paths() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (_tx, rx) = mpsc::channel(1);

        let paths = vec![std::path::PathBuf::from("/test/path")];
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_skill_reload(paths.clone(), rx);

        assert_eq!(agent.skill_paths, paths);
    }

    #[tokio::test]
    async fn agent_handles_tool_execution_error() {
        let provider = MockProvider::new(vec!["response".to_string()]);
        let _channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Err(ToolError::Timeout { timeout_secs: 30 })]);

        let agent_channel = MockChannel::new(vec!["test".to_string()]);
        let sent = agent_channel.sent.clone();

        let mut agent = Agent::new(provider, agent_channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent_msgs = sent.lock().unwrap();
        assert!(
            sent_msgs
                .iter()
                .any(|m| m.contains("Tool execution failed"))
        );
    }

    #[tokio::test]
    async fn agent_processes_multi_turn_tool_execution() {
        let provider = MockProvider::new(vec![
            "first response".to_string(),
            "second response".to_string(),
        ]);
        let channel = MockChannel::new(vec!["start task".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![
            Ok(Some(ToolOutput {
                summary: "step 1 complete".to_string(),
                blocks_executed: 1,
            })),
            Ok(None),
        ]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
        assert!(agent.messages.len() > 3);
    }

    #[tokio::test]
    async fn agent_respects_max_shell_iterations() {
        let mut responses = vec![];
        for _ in 0..10 {
            responses.push("response".to_string());
        }
        let provider = MockProvider::new(responses);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();

        let mut outputs = vec![];
        for _ in 0..10 {
            outputs.push(Ok(Some(ToolOutput {
                summary: "continuing".to_string(),
                blocks_executed: 1,
            })));
        }
        let executor = MockToolExecutor::new(outputs);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
        let assistant_count = agent
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        assert!(assistant_count <= MAX_SHELL_ITERATIONS);
    }

    #[test]
    fn security_config_default() {
        let config = SecurityConfig::default();
        let _ = format!("{config:?}");
    }

    #[test]
    fn timeout_config_default() {
        let config = TimeoutConfig::default();
        let _ = format!("{config:?}");
    }
}
