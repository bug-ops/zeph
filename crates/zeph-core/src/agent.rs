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
use zeph_tools::executor::{ToolError, ToolExecutor};

use crate::channel::Channel;
#[cfg(feature = "self-learning")]
use crate::config::LearningConfig;
use crate::context::build_system_prompt;

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
    #[cfg(feature = "self-learning")]
    learning_config: Option<LearningConfig>,
    #[cfg(feature = "self-learning")]
    reflection_used: bool,
    #[cfg(feature = "mcp")]
    mcp_tools: Vec<zeph_mcp::McpTool>,
    #[cfg(feature = "mcp")]
    mcp_registry: Option<zeph_mcp::McpToolRegistry>,
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
        let skills_prompt = format_skills_prompt(&all_skills);
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
            #[cfg(feature = "self-learning")]
            learning_config: None,
            #[cfg(feature = "self-learning")]
            reflection_used: false,
            #[cfg(feature = "mcp")]
            mcp_tools: Vec::new(),
            #[cfg(feature = "mcp")]
            mcp_registry: None,
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
    ) -> Self {
        self.mcp_tools = tools;
        self.mcp_registry = registry;
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

    async fn reload_skills(&mut self) {
        self.registry = SkillRegistry::load(&self.skill_paths);

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
        let skills_prompt = format_skills_prompt(&all_skills);
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

        let skills_prompt = format_skills_prompt(&active_skills);
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

    async fn process_response(&mut self) -> anyhow::Result<()> {
        for _ in 0..MAX_SHELL_ITERATIONS {
            self.channel.send_typing().await?;

            let response = if self.provider.supports_streaming() {
                self.process_response_streaming().await?
            } else {
                let resp = self.provider.chat(&self.messages).await?;
                self.channel.send(&resp).await?;
                resp
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

            match self.tool_executor.execute(&response).await {
                Ok(Some(output)) => {
                    if output.summary.trim().is_empty() {
                        tracing::warn!("tool execution returned empty output");
                        self.record_skill_outcomes("success", None).await;
                        return Ok(());
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
                            return Ok(());
                        }
                    } else {
                        self.record_skill_outcomes("success", None).await;
                    }

                    let formatted_output = format!("[tool output]\n```\n{output}\n```");
                    self.channel.send(&formatted_output).await?;

                    self.messages.push(Message {
                        role: Role::User,
                        content: formatted_output.clone(),
                    });
                    self.persist_message(Role::User, &formatted_output).await;
                }
                Ok(None) => {
                    self.record_skill_outcomes("success", None).await;
                    return Ok(());
                }
                Err(ToolError::Blocked { command }) => {
                    tracing::warn!("blocked command: {command}");
                    let error_msg = "This command is blocked by security policy.".to_string();
                    self.channel.send(&error_msg).await?;
                    return Ok(());
                }
                Err(e) => {
                    let err_str = format!("{e:#}");
                    tracing::error!("tool execution error: {err_str}");
                    self.record_skill_outcomes("tool_failure", Some(&err_str))
                        .await;

                    #[cfg(feature = "self-learning")]
                    if !self.reflection_used && self.attempt_self_reflection(&err_str, "").await? {
                        return Ok(());
                    }

                    self.channel
                        .send("Tool execution failed. Please try a different approach.")
                        .await?;
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    async fn process_response_streaming(&mut self) -> anyhow::Result<String> {
        let mut stream = self.provider.chat_stream(&self.messages).await?;
        let mut response = String::with_capacity(2048);

        while let Some(chunk_result) = stream.next().await {
            let chunk: String = chunk_result?;
            response.push_str(&chunk);
            self.channel.send_chunk(&chunk).await?;
        }

        self.channel.flush_chunks().await?;
        Ok(response)
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

        let skill = match self.registry.get_skill(&name) {
            Ok(s) => s,
            Err(_) => return Ok(false),
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
