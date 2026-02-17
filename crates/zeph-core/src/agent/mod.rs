mod context;
pub mod error;
#[cfg(feature = "index")]
mod index;
mod learning;
mod mcp;
mod persistence;
mod streaming;
mod trust_commands;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, Role};

use crate::metrics::MetricsSnapshot;
use std::collections::HashMap;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::loader::Skill;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::prompt::format_skills_prompt;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::SkillEvent;
use zeph_tools::executor::ToolExecutor;

use crate::channel::Channel;
use crate::config::Config;
use crate::config::LearningConfig;
use crate::config::{SecurityConfig, TimeoutConfig};
use crate::config_watcher::ConfigEvent;
use crate::context::{ContextBudget, EnvironmentContext, build_system_prompt};
use crate::cost::CostTracker;

const DOOM_LOOP_WINDOW: usize = 3;
const TOOL_LOOP_KEEP_RECENT: usize = 4;
const MAX_QUEUE_SIZE: usize = 10;
const MESSAGE_MERGE_WINDOW: Duration = Duration::from_millis(500);
const RECALL_PREFIX: &str = "[semantic recall]\n";
const CODE_CONTEXT_PREFIX: &str = "[code context]\n";
const SUMMARY_PREFIX: &str = "[conversation summaries]\n";
const CROSS_SESSION_PREFIX: &str = "[cross-session context]\n";
const TOOL_OUTPUT_SUFFIX: &str = "\n```";

fn format_tool_output(tool_name: &str, body: &str) -> String {
    format!("[tool output: {tool_name}]\n```\n{body}{TOOL_OUTPUT_SUFFIX}")
}

struct QueuedMessage {
    text: String,
    received_at: Instant,
}

pub(super) struct MemoryState {
    pub(super) memory: Option<SemanticMemory>,
    pub(super) conversation_id: Option<zeph_memory::ConversationId>,
    pub(super) history_limit: u32,
    pub(super) recall_limit: usize,
    pub(super) summarization_threshold: usize,
    pub(super) cross_session_score_threshold: f32,
}

pub(super) struct SkillState {
    pub(super) registry: SkillRegistry,
    pub(super) skill_paths: Vec<PathBuf>,
    pub(super) matcher: Option<SkillMatcherBackend>,
    pub(super) max_active_skills: usize,
    pub(super) embedding_model: String,
    pub(super) skill_reload_rx: Option<mpsc::Receiver<SkillEvent>>,
    pub(super) active_skill_names: Vec<String>,
    pub(super) last_skills_prompt: String,
}

pub(super) struct ContextState {
    pub(super) budget: Option<ContextBudget>,
    pub(super) compaction_threshold: f32,
    pub(super) compaction_preserve_tail: usize,
    pub(super) prune_protect_tokens: usize,
}

pub(super) struct McpState {
    pub(super) tools: Vec<zeph_mcp::McpTool>,
    pub(super) registry: Option<zeph_mcp::McpToolRegistry>,
    pub(super) manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
    pub(super) allowed_commands: Vec<String>,
    pub(super) max_dynamic: usize,
}

#[cfg(feature = "index")]
pub(super) struct IndexState {
    pub(super) retriever: Option<std::sync::Arc<zeph_index::retriever::CodeRetriever>>,
    pub(super) repo_map_tokens: usize,
    pub(super) cached_repo_map: Option<(String, std::time::Instant)>,
    pub(super) repo_map_ttl: std::time::Duration,
}

pub(super) struct RuntimeConfig {
    pub(super) security: SecurityConfig,
    pub(super) timeouts: TimeoutConfig,
    pub(super) model_name: String,
    pub(super) max_tool_iterations: usize,
    pub(super) summarize_tool_output_enabled: bool,
    pub(super) permission_policy: zeph_tools::PermissionPolicy,
}

pub struct Agent<C: Channel, T: ToolExecutor> {
    provider: AnyProvider,
    channel: C,
    tool_executor: T,
    messages: Vec<Message>,
    pub(super) memory_state: MemoryState,
    pub(super) skill_state: SkillState,
    pub(super) context_state: ContextState,
    config_path: Option<PathBuf>,
    config_reload_rx: Option<mpsc::Receiver<ConfigEvent>>,
    shutdown: watch::Receiver<bool>,
    metrics_tx: Option<watch::Sender<MetricsSnapshot>>,
    pub(super) runtime: RuntimeConfig,
    learning_config: Option<LearningConfig>,
    reflection_used: bool,
    pub(super) mcp: McpState,
    #[cfg(feature = "index")]
    pub(super) index: IndexState,
    start_time: Instant,
    message_queue: VecDeque<QueuedMessage>,
    summary_provider: Option<AnyProvider>,
    warmup_ready: Option<watch::Receiver<bool>>,
    doom_loop_history: Vec<String>,
    cost_tracker: Option<CostTracker>,
    cached_prompt_tokens: u64,
}

impl<C: Channel, T: ToolExecutor> Agent<C, T> {
    #[must_use]
    pub fn new(
        provider: AnyProvider,
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
        let empty_trust = HashMap::new();
        let skills_prompt = format_skills_prompt(&all_skills, std::env::consts::OS, &empty_trust);
        let system_prompt = build_system_prompt(&skills_prompt, None, None, false);
        tracing::debug!(len = system_prompt.len(), "initial system prompt built");
        tracing::trace!(prompt = %system_prompt, "full system prompt");

        let initial_prompt_tokens = u64::try_from(system_prompt.len()).unwrap_or(0) / 4;
        let (_tx, rx) = watch::channel(false);
        Self {
            provider,
            channel,
            tool_executor,
            messages: vec![Message {
                role: Role::System,
                content: system_prompt,
                parts: vec![],
            }],
            memory_state: MemoryState {
                memory: None,
                conversation_id: None,
                history_limit: 50,
                recall_limit: 5,
                summarization_threshold: 50,
                cross_session_score_threshold: 0.35,
            },
            skill_state: SkillState {
                registry,
                skill_paths: Vec::new(),
                matcher,
                max_active_skills,
                embedding_model: String::new(),
                skill_reload_rx: None,
                active_skill_names: Vec::new(),
                last_skills_prompt: skills_prompt,
            },
            context_state: ContextState {
                budget: None,
                compaction_threshold: 0.80,
                compaction_preserve_tail: 6,
                prune_protect_tokens: 40_000,
            },
            config_path: None,
            config_reload_rx: None,
            shutdown: rx,
            metrics_tx: None,
            runtime: RuntimeConfig {
                security: SecurityConfig::default(),
                timeouts: TimeoutConfig::default(),
                model_name: String::new(),
                max_tool_iterations: 10,
                summarize_tool_output_enabled: false,
                permission_policy: zeph_tools::PermissionPolicy::default(),
            },
            learning_config: None,
            reflection_used: false,
            mcp: McpState {
                tools: Vec::new(),
                registry: None,
                manager: None,
                allowed_commands: Vec::new(),
                max_dynamic: 10,
            },
            #[cfg(feature = "index")]
            index: IndexState {
                retriever: None,
                repo_map_tokens: 0,
                cached_repo_map: None,
                repo_map_ttl: std::time::Duration::from_secs(300),
            },
            start_time: Instant::now(),
            message_queue: VecDeque::new(),
            summary_provider: None,
            warmup_ready: None,
            doom_loop_history: Vec::new(),
            cost_tracker: None,
            cached_prompt_tokens: initial_prompt_tokens,
        }
    }

    #[must_use]
    pub fn with_max_tool_iterations(mut self, max: usize) -> Self {
        self.runtime.max_tool_iterations = max;
        self
    }

    #[must_use]
    pub fn with_memory(
        mut self,
        memory: SemanticMemory,
        conversation_id: zeph_memory::ConversationId,
        history_limit: u32,
        recall_limit: usize,
        summarization_threshold: usize,
    ) -> Self {
        let has_qdrant = memory.has_qdrant();
        self.memory_state.memory = Some(memory);
        self.memory_state.conversation_id = Some(conversation_id);
        self.memory_state.history_limit = history_limit;
        self.memory_state.recall_limit = recall_limit;
        self.memory_state.summarization_threshold = summarization_threshold;
        self.update_metrics(|m| {
            m.qdrant_available = has_qdrant;
            m.sqlite_conversation_id = Some(conversation_id);
        });
        self
    }

    #[must_use]
    pub fn with_embedding_model(mut self, model: String) -> Self {
        self.skill_state.embedding_model = model;
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
        self.skill_state.skill_paths = paths;
        self.skill_state.skill_reload_rx = Some(rx);
        self
    }

    #[must_use]
    pub fn with_config_reload(mut self, path: PathBuf, rx: mpsc::Receiver<ConfigEvent>) -> Self {
        self.config_path = Some(path);
        self.config_reload_rx = Some(rx);
        self
    }

    #[must_use]
    pub fn with_learning(mut self, config: LearningConfig) -> Self {
        self.learning_config = Some(config);
        self
    }

    #[must_use]
    pub fn with_mcp(
        mut self,
        tools: Vec<zeph_mcp::McpTool>,
        registry: Option<zeph_mcp::McpToolRegistry>,
        manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
        mcp_config: &crate::config::McpConfig,
    ) -> Self {
        self.mcp.tools = tools;
        self.mcp.registry = registry;
        self.mcp.manager = manager;
        self.mcp
            .allowed_commands
            .clone_from(&mcp_config.allowed_commands);
        self.mcp.max_dynamic = mcp_config.max_dynamic_servers;
        self
    }

    #[must_use]
    pub fn with_security(mut self, security: SecurityConfig, timeouts: TimeoutConfig) -> Self {
        self.runtime.security = security;
        self.runtime.timeouts = timeouts;
        self
    }

    #[must_use]
    pub fn with_tool_summarization(mut self, enabled: bool) -> Self {
        self.runtime.summarize_tool_output_enabled = enabled;
        self
    }

    #[must_use]
    pub fn with_summary_provider(mut self, provider: AnyProvider) -> Self {
        self.summary_provider = Some(provider);
        self
    }

    fn summary_or_primary_provider(&self) -> &AnyProvider {
        self.summary_provider.as_ref().unwrap_or(&self.provider)
    }

    #[must_use]
    pub fn with_permission_policy(mut self, policy: zeph_tools::PermissionPolicy) -> Self {
        self.runtime.permission_policy = policy;
        self
    }

    #[must_use]
    pub fn with_context_budget(
        mut self,
        budget_tokens: usize,
        reserve_ratio: f32,
        compaction_threshold: f32,
        compaction_preserve_tail: usize,
        prune_protect_tokens: usize,
    ) -> Self {
        if budget_tokens > 0 {
            self.context_state.budget = Some(ContextBudget::new(budget_tokens, reserve_ratio));
        }
        self.context_state.compaction_threshold = compaction_threshold;
        self.context_state.compaction_preserve_tail = compaction_preserve_tail;
        self.context_state.prune_protect_tokens = prune_protect_tokens;
        self
    }

    #[must_use]
    pub fn with_model_name(mut self, name: impl Into<String>) -> Self {
        self.runtime.model_name = name.into();
        self
    }

    #[must_use]
    pub fn with_warmup_ready(mut self, rx: watch::Receiver<bool>) -> Self {
        self.warmup_ready = Some(rx);
        self
    }

    #[must_use]
    pub fn with_cost_tracker(mut self, tracker: CostTracker) -> Self {
        self.cost_tracker = Some(tracker);
        self
    }

    #[cfg(feature = "index")]
    #[must_use]
    pub fn with_code_retriever(
        mut self,
        retriever: std::sync::Arc<zeph_index::retriever::CodeRetriever>,
        repo_map_tokens: usize,
        repo_map_ttl_secs: u64,
    ) -> Self {
        self.index.retriever = Some(retriever);
        self.index.repo_map_tokens = repo_map_tokens;
        self.index.repo_map_ttl = std::time::Duration::from_secs(repo_map_ttl_secs);
        self
    }

    #[must_use]
    pub fn with_metrics(mut self, tx: watch::Sender<MetricsSnapshot>) -> Self {
        let provider_name = self.provider.name().to_string();
        let model_name = self.runtime.model_name.clone();
        let total_skills = self.skill_state.registry.all_meta().len();
        let qdrant_available = self
            .memory_state
            .memory
            .as_ref()
            .is_some_and(zeph_memory::semantic::SemanticMemory::has_qdrant);
        let conversation_id = self.memory_state.conversation_id;
        let prompt_estimate = self
            .messages
            .first()
            .map_or(0, |m| u64::try_from(m.content.len()).unwrap_or(0) / 4);
        let mcp_tool_count = self.mcp.tools.len();
        let mcp_server_count = self
            .mcp
            .tools
            .iter()
            .map(|t| &t.server_id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        tx.send_modify(|m| {
            m.provider_name = provider_name;
            m.model_name = model_name;
            m.total_skills = total_skills;
            m.qdrant_available = qdrant_available;
            m.sqlite_conversation_id = conversation_id;
            m.context_tokens = prompt_estimate;
            m.prompt_tokens = prompt_estimate;
            m.total_tokens = prompt_estimate;
            m.mcp_tool_count = mcp_tool_count;
            m.mcp_server_count = mcp_server_count;
        });
        self.metrics_tx = Some(tx);
        self
    }

    fn update_metrics(&self, f: impl FnOnce(&mut MetricsSnapshot)) {
        if let Some(ref tx) = self.metrics_tx {
            let elapsed = self.start_time.elapsed().as_secs();
            tx.send_modify(|m| {
                m.uptime_seconds = elapsed;
                f(m);
            });
        }
    }

    fn estimate_tokens(content: &str) -> u64 {
        u64::try_from(content.len()).unwrap_or(0) / 4
    }

    pub(super) fn recompute_prompt_tokens(&mut self) {
        self.cached_prompt_tokens = self
            .messages
            .iter()
            .map(|m| Self::estimate_tokens(&m.content))
            .sum();
    }

    pub(super) fn push_message(&mut self, msg: Message) {
        self.cached_prompt_tokens += Self::estimate_tokens(&msg.content);
        self.messages.push(msg);
    }

    pub(crate) fn record_cost(&self, prompt_tokens: u64, completion_tokens: u64) {
        if let Some(ref tracker) = self.cost_tracker {
            tracker.record_usage(&self.runtime.model_name, prompt_tokens, completion_tokens);
            self.update_metrics(|m| {
                m.cost_spent_cents = tracker.current_spend();
            });
        }
    }

    pub(crate) fn record_cache_usage(&self) {
        if let Some((creation, read)) = self.provider.last_cache_usage() {
            self.update_metrics(|m| {
                m.cache_creation_tokens += creation;
                m.cache_read_tokens += read;
            });
        }
    }

    /// Inject pre-formatted code context into the message list.
    /// The caller is responsible for retrieving and formatting the text.
    pub fn inject_code_context(&mut self, text: &str) {
        self.remove_code_context_messages();
        if text.is_empty() || self.messages.len() <= 1 {
            return;
        }
        let content = format!("{CODE_CONTEXT_PREFIX}{text}");
        self.messages.insert(
            1,
            Message::from_parts(
                Role::System,
                vec![zeph_llm::provider::MessagePart::CodeContext { text: content }],
            ),
        );
    }

    #[must_use]
    pub fn context_messages(&self) -> &[Message] {
        &self.messages
    }

    fn drain_channel(&mut self) {
        while self.message_queue.len() < MAX_QUEUE_SIZE {
            let Some(msg) = self.channel.try_recv() else {
                break;
            };
            self.enqueue_or_merge(msg.text);
        }
    }

    fn enqueue_or_merge(&mut self, text: String) {
        let now = Instant::now();
        if let Some(last) = self.message_queue.back_mut()
            && now.duration_since(last.received_at) < MESSAGE_MERGE_WINDOW
        {
            last.text.push('\n');
            last.text.push_str(&text);
            return;
        }
        if self.message_queue.len() < MAX_QUEUE_SIZE {
            self.message_queue.push_back(QueuedMessage {
                text,
                received_at: now,
            });
        } else {
            tracing::warn!("message queue full, dropping message");
        }
    }

    async fn notify_queue_count(&mut self) {
        let count = self.message_queue.len();
        let _ = self.channel.send_queue_count(count).await;
    }

    fn clear_queue(&mut self) -> usize {
        let count = self.message_queue.len();
        self.message_queue.clear();
        count
    }

    pub async fn shutdown(&mut self) {
        self.channel.send("Shutting down...").await.ok();

        if let Some(ref manager) = self.mcp.manager {
            manager.shutdown_all_shared().await;
        }

        tracing::info!("agent shutdown complete");
    }

    /// Run the chat loop, receiving messages via the channel until EOF or shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if channel I/O or LLM communication fails.
    pub async fn run(&mut self) -> anyhow::Result<()> {
        if let Some(mut rx) = self.warmup_ready.take()
            && !*rx.borrow()
        {
            let _ = rx.changed().await;
            if !*rx.borrow() {
                tracing::warn!("model warmup did not complete successfully");
            }
        }

        loop {
            self.drain_channel();

            let text = if let Some(queued) = self.message_queue.pop_front() {
                self.notify_queue_count().await;
                queued.text
            } else {
                let incoming = tokio::select! {
                    result = self.channel.recv() => result?,
                    () = shutdown_signal(&mut self.shutdown) => {
                        tracing::info!("shutting down");
                        break;
                    }
                    Some(_) = recv_optional(&mut self.skill_state.skill_reload_rx) => {
                        self.reload_skills().await;
                        continue;
                    }
                    Some(_) = recv_optional(&mut self.config_reload_rx) => {
                        self.reload_config();
                        continue;
                    }
                };
                let Some(msg) = incoming else { break };
                self.drain_channel();
                msg.text
            };

            let trimmed = text.trim();

            if trimmed == "/clear-queue" {
                let n = self.clear_queue();
                self.notify_queue_count().await;
                self.channel
                    .send(&format!("Cleared {n} queued messages."))
                    .await?;
                continue;
            }

            self.process_user_message(text).await?;
        }

        Ok(())
    }

    async fn process_user_message(&mut self, text: String) -> Result<(), error::AgentError> {
        let trimmed = text.trim();

        if trimmed == "/skills" {
            self.handle_skills_command().await?;
            return Ok(());
        }

        if let Some(rest) = trimmed.strip_prefix("/skill ") {
            self.handle_skill_command(rest).await?;
            return Ok(());
        }

        if let Some(rest) = trimmed.strip_prefix("/feedback ") {
            self.handle_feedback(rest).await?;
            return Ok(());
        }

        if trimmed == "/mcp" || trimmed.starts_with("/mcp ") {
            let args = trimmed.strip_prefix("/mcp").unwrap_or("").trim();
            self.handle_mcp_command(args).await?;
            return Ok(());
        }

        self.rebuild_system_prompt(&text).await;

        if let Err(e) = self.maybe_compact().await {
            tracing::warn!("context compaction failed: {e:#}");
        }

        if let Err(e) = Box::pin(self.prepare_context(trimmed)).await {
            tracing::warn!("context preparation failed: {e:#}");
        }

        self.reflection_used = false;

        self.push_message(Message {
            role: Role::User,
            content: text.clone(),
            parts: vec![],
        });
        self.persist_message(Role::User, &text).await;

        if let Err(e) = self.process_response().await {
            tracing::error!("Response processing failed: {e:#}");
            let user_msg = format!("Error: {e:#}");
            self.channel.send(&user_msg).await?;
            self.messages.pop();
            self.recompute_prompt_tokens();
        }

        Ok(())
    }

    async fn handle_skills_command(&mut self) -> Result<(), error::AgentError> {
        use std::fmt::Write;

        let mut output = String::from("Available skills:\n\n");

        for meta in self.skill_state.registry.all_meta() {
            let trust_info = if let Some(memory) = &self.memory_state.memory {
                memory
                    .sqlite()
                    .load_skill_trust(&meta.name)
                    .await
                    .ok()
                    .flatten()
                    .map_or_else(String::new, |r| format!(" [{}]", r.trust_level))
            } else {
                String::new()
            };
            let _ = writeln!(output, "- {} — {}{trust_info}", meta.name, meta.description);
        }

        if let Some(memory) = &self.memory_state.memory {
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

        self.channel.send(&output).await?;
        Ok(())
    }

    async fn handle_feedback(&mut self, input: &str) -> Result<(), error::AgentError> {
        let Some((name, rest)) = input.split_once(' ') else {
            self.channel
                .send("Usage: /feedback <skill_name> <message>")
                .await?;
            return Ok(());
        };
        let (skill_name, feedback) = (name.trim(), rest.trim().trim_matches('"'));

        if feedback.is_empty() {
            self.channel
                .send("Usage: /feedback <skill_name> <message>")
                .await?;
            return Ok(());
        }

        let Some(memory) = &self.memory_state.memory else {
            self.channel.send("Memory not available.").await?;
            return Ok(());
        };

        memory
            .sqlite()
            .record_skill_outcome(
                skill_name,
                None,
                self.memory_state.conversation_id,
                "user_rejection",
                Some(feedback),
            )
            .await?;

        if self.is_learning_enabled() {
            self.generate_improved_skill(skill_name, feedback, "", Some(feedback))
                .await
                .ok();
        }

        self.channel
            .send(&format!("Feedback recorded for \"{skill_name}\"."))
            .await?;
        Ok(())
    }

    async fn reload_skills(&mut self) {
        let new_registry = SkillRegistry::load(&self.skill_state.skill_paths);
        if new_registry.fingerprint() == self.skill_state.registry.fingerprint() {
            return;
        }
        self.skill_state.registry = new_registry;

        let all_meta = self.skill_state.registry.all_meta();
        let provider = self.provider.clone();
        let embed_fn = |text: &str| -> zeph_skills::matcher::EmbedFuture {
            let owned = text.to_owned();
            let p = provider.clone();
            Box::pin(async move { p.embed(&owned).await })
        };

        let needs_inmemory_rebuild = !self
            .skill_state
            .matcher
            .as_ref()
            .is_some_and(SkillMatcherBackend::is_qdrant);

        if needs_inmemory_rebuild {
            self.skill_state.matcher = SkillMatcher::new(&all_meta, embed_fn)
                .await
                .map(SkillMatcherBackend::InMemory);
        } else if let Some(ref mut backend) = self.skill_state.matcher
            && let Err(e) = backend
                .sync(&all_meta, &self.skill_state.embedding_model, embed_fn)
                .await
        {
            tracing::warn!("failed to sync skill embeddings: {e:#}");
        }

        let all_skills: Vec<Skill> = self
            .skill_state
            .registry
            .all_meta()
            .iter()
            .filter_map(|m| self.skill_state.registry.get_skill(&m.name).ok())
            .collect();
        let trust_map = self.build_skill_trust_map().await;
        let skills_prompt = format_skills_prompt(&all_skills, std::env::consts::OS, &trust_map);
        self.skill_state
            .last_skills_prompt
            .clone_from(&skills_prompt);
        let system_prompt = build_system_prompt(&skills_prompt, None, None, false);
        if let Some(msg) = self.messages.first_mut() {
            msg.content = system_prompt;
        }

        tracing::info!(
            "reloaded {} skill(s)",
            self.skill_state.registry.all_meta().len()
        );
    }

    fn reload_config(&mut self) {
        let Some(ref path) = self.config_path else {
            return;
        };
        let config = match Config::load(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config reload failed: {e:#}");
                return;
            }
        };

        self.runtime.security = config.security;
        self.runtime.timeouts = config.timeouts;
        self.memory_state.history_limit = config.memory.history_limit;
        self.memory_state.recall_limit = config.memory.semantic.recall_limit;
        self.memory_state.summarization_threshold = config.memory.summarization_threshold;
        self.skill_state.max_active_skills = config.skills.max_active_skills;

        if config.memory.context_budget_tokens > 0 {
            self.context_state.budget = Some(ContextBudget::new(
                config.memory.context_budget_tokens,
                0.20,
            ));
        } else {
            self.context_state.budget = None;
        }
        self.context_state.compaction_threshold = config.memory.compaction_threshold;
        self.context_state.compaction_preserve_tail = config.memory.compaction_preserve_tail;
        self.context_state.prune_protect_tokens = config.memory.prune_protect_tokens;
        self.memory_state.cross_session_score_threshold =
            config.memory.cross_session_score_threshold;

        #[cfg(feature = "index")]
        {
            self.index.repo_map_ttl =
                std::time::Duration::from_secs(config.index.repo_map_ttl_secs);
        }

        tracing::info!("config reloaded");
    }
}

async fn shutdown_signal(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn recv_optional<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
pub(super) mod agent_tests {
    pub(crate) use super::*;
    use crate::channel::ChannelMessage;
    use std::sync::{Arc, Mutex};
    use zeph_llm::mock::MockProvider;
    use zeph_tools::executor::{ToolError, ToolOutput};

    pub(super) fn mock_provider(responses: Vec<String>) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(responses))
    }

    pub(super) fn mock_provider_streaming(responses: Vec<String>) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(responses).with_streaming())
    }

    pub(super) fn mock_provider_failing() -> AnyProvider {
        AnyProvider::Mock(MockProvider::failing())
    }

    pub(super) struct MockChannel {
        messages: Arc<Mutex<Vec<String>>>,
        sent: Arc<Mutex<Vec<String>>>,
        chunks: Arc<Mutex<Vec<String>>>,
        confirmations: Arc<Mutex<Vec<bool>>>,
    }

    impl MockChannel {
        pub(super) fn new(messages: Vec<String>) -> Self {
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
        async fn recv(&mut self) -> Result<Option<ChannelMessage>, crate::channel::ChannelError> {
            let mut msgs = self.messages.lock().unwrap();
            if msgs.is_empty() {
                Ok(None)
            } else {
                Ok(Some(ChannelMessage {
                    text: msgs.remove(0),
                }))
            }
        }

        fn try_recv(&mut self) -> Option<ChannelMessage> {
            let mut msgs = self.messages.lock().unwrap();
            if msgs.is_empty() {
                None
            } else {
                Some(ChannelMessage {
                    text: msgs.remove(0),
                })
            }
        }

        async fn send(&mut self, text: &str) -> Result<(), crate::channel::ChannelError> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }

        async fn send_chunk(&mut self, chunk: &str) -> Result<(), crate::channel::ChannelError> {
            self.chunks.lock().unwrap().push(chunk.to_string());
            Ok(())
        }

        async fn flush_chunks(&mut self) -> Result<(), crate::channel::ChannelError> {
            Ok(())
        }

        async fn confirm(&mut self, _prompt: &str) -> Result<bool, crate::channel::ChannelError> {
            let mut confs = self.confirmations.lock().unwrap();
            Ok(if confs.is_empty() {
                true
            } else {
                confs.remove(0)
            })
        }
    }

    pub(super) struct MockToolExecutor {
        outputs: Arc<Mutex<Vec<Result<Option<ToolOutput>, ToolError>>>>,
    }

    impl MockToolExecutor {
        pub(super) fn new(outputs: Vec<Result<Option<ToolOutput>, ToolError>>) -> Self {
            Self {
                outputs: Arc::new(Mutex::new(outputs)),
            }
        }

        pub(super) fn no_tools() -> Self {
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

    pub(super) fn create_test_registry() -> SkillRegistry {
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
        let provider = mock_provider(vec![]);
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
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_embedding_model("test-embed-model".to_string());

        assert_eq!(agent.skill_state.embedding_model, "test-embed-model");
    }

    #[tokio::test]
    async fn agent_with_shutdown_sets_receiver() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (_tx, rx) = watch::channel(false);

        let _agent = Agent::new(provider, channel, registry, None, 5, executor).with_shutdown(rx);
    }

    #[tokio::test]
    async fn agent_with_security_sets_config() {
        let provider = mock_provider(vec![]);
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

        assert!(agent.runtime.security.redact_secrets);
        assert_eq!(agent.runtime.timeouts.llm_seconds, 60);
    }

    #[tokio::test]
    async fn agent_run_handles_empty_channel() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn agent_run_processes_user_message() {
        let provider = mock_provider(vec!["test response".to_string()]);
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
        let provider = mock_provider(vec![]);
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
        let provider = mock_provider(vec![]);
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
        let provider = mock_provider(vec!["".to_string()]);
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
        let provider = mock_provider(vec!["response with tool".to_string()]);
        let _channel = MockChannel::new(vec!["execute tool".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
            tool_name: "bash".to_string(),
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
        let provider = mock_provider(vec!["run blocked command".to_string()]);
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
        let provider = mock_provider(vec!["access forbidden path".to_string()]);
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
        let provider = mock_provider(vec!["needs confirmation".to_string()]);
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
        let provider = mock_provider(vec!["needs confirmation".to_string()]);
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
        let provider = mock_provider_streaming(vec!["streaming response".to_string()]);
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
        let provider = mock_provider(vec![]);
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
        let provider = mock_provider(vec![]);
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
        let provider = mock_provider(vec![
            "first response".to_string(),
            "second response".to_string(),
        ]);
        // Both messages arrive simultaneously via try_recv(), so they merge
        // within the 500ms window into a single "first\nsecond" message.
        let channel = MockChannel::new(vec!["first".to_string(), "second".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(None), Ok(None)]);

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());
        assert_eq!(agent.messages.len(), 3);
        assert_eq!(agent.messages[1].content, "first\nsecond");
    }

    #[tokio::test]
    async fn agent_handles_tool_output_with_error_marker() {
        let provider = mock_provider(vec!["response".to_string(), "retry".to_string()]);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![
            Ok(Some(ToolOutput {
                tool_name: "bash".to_string(),
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
        let provider = mock_provider(vec!["response".to_string()]);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![Ok(Some(ToolOutput {
            tool_name: "bash".to_string(),
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
    async fn recv_optional_returns_pending_when_no_receiver() {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            recv_optional::<SkillEvent>(&mut None),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recv_optional_receives_from_channel() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(SkillEvent::Changed).await.unwrap();

        let result = recv_optional(&mut Some(rx)).await;
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn agent_with_skill_reload_sets_paths() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (_tx, rx) = mpsc::channel(1);

        let paths = vec![std::path::PathBuf::from("/test/path")];
        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_skill_reload(paths.clone(), rx);

        assert_eq!(agent.skill_state.skill_paths, paths);
    }

    #[tokio::test]
    async fn agent_handles_tool_execution_error() {
        let provider = mock_provider(vec!["response".to_string()]);
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
        let provider = mock_provider(vec![
            "first response".to_string(),
            "second response".to_string(),
        ]);
        let channel = MockChannel::new(vec!["start task".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::new(vec![
            Ok(Some(ToolOutput {
                tool_name: "bash".to_string(),
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
        let provider = mock_provider(responses);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();

        let mut outputs = vec![];
        for _ in 0..10 {
            outputs.push(Ok(Some(ToolOutput {
                tool_name: "bash".to_string(),
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
        assert!(assistant_count <= 10);
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

    #[tokio::test]
    async fn agent_with_metrics_sets_initial_values() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let _agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_model_name("test-model")
            .with_metrics(tx);

        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.provider_name, "mock");
        assert_eq!(snapshot.model_name, "test-model");
        assert_eq!(snapshot.total_skills, 1);
        assert!(
            snapshot.prompt_tokens > 0,
            "initial prompt estimate should be non-zero"
        );
        assert_eq!(snapshot.total_tokens, snapshot.prompt_tokens);
    }

    #[tokio::test]
    async fn agent_metrics_update_on_llm_call() {
        let provider = mock_provider(vec!["response".to_string()]);
        let channel = MockChannel::new(vec!["hello".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.run().await.unwrap();

        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.api_calls, 1);
        assert!(snapshot.total_tokens > 0);
    }

    #[tokio::test]
    async fn agent_metrics_streaming_updates_completion_tokens() {
        let provider = mock_provider_streaming(vec!["streaming response".to_string()]);
        let channel = MockChannel::new(vec!["test".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.run().await.unwrap();

        let snapshot = rx.borrow().clone();
        assert!(snapshot.completion_tokens > 0);
        assert_eq!(snapshot.api_calls, 1);
    }

    #[tokio::test]
    async fn agent_metrics_persist_increments_count() {
        let provider = mock_provider(vec!["response".to_string()]);
        let channel = MockChannel::new(vec!["hello".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.run().await.unwrap();

        let snapshot = rx.borrow().clone();
        assert!(snapshot.sqlite_message_count == 0, "no memory = no persist");
    }

    #[tokio::test]
    async fn agent_metrics_skills_updated_on_prompt_rebuild() {
        let provider = mock_provider(vec!["response".to_string()]);
        let channel = MockChannel::new(vec!["hello".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.run().await.unwrap();

        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot.total_skills, 1);
        assert!(!snapshot.active_skills.is_empty());
    }

    #[test]
    fn update_metrics_noop_when_none() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.update_metrics(|m| m.api_calls = 999);
    }

    #[test]
    fn update_metrics_sets_uptime_seconds() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, rx) = tokio::sync::watch::channel(MetricsSnapshot::default());
        let agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        agent.update_metrics(|m| m.api_calls = 1);

        let snapshot = rx.borrow();
        assert!(snapshot.uptime_seconds < 2);
        assert_eq!(snapshot.api_calls, 1);
    }

    #[test]
    fn test_last_user_query_finds_original() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.messages.push(Message {
            role: Role::User,
            content: "hello".to_string(),
            parts: vec![],
        });
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "cmd".to_string(),
            parts: vec![],
        });
        agent.messages.push(Message {
            role: Role::User,
            content: "[tool output: bash]\nsome output".to_string(),
            parts: vec![],
        });

        assert_eq!(agent.last_user_query(), "hello");
    }

    #[test]
    fn test_last_user_query_empty_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        assert_eq!(agent.last_user_query(), "");
    }

    #[tokio::test]
    async fn test_maybe_summarize_short_output_passthrough() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_tool_summarization(true);

        let short = "short output";
        let result = agent.maybe_summarize_tool_output(short).await;
        assert_eq!(result, short);
    }

    #[tokio::test]
    async fn test_maybe_summarize_long_output_disabled_truncates() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_tool_summarization(false);

        let long = "x".repeat(zeph_tools::MAX_TOOL_OUTPUT_CHARS + 1000);
        let result = agent.maybe_summarize_tool_output(&long).await;
        assert!(result.contains("truncated"));
    }

    #[tokio::test]
    async fn test_maybe_summarize_long_output_enabled_calls_llm() {
        let provider = mock_provider(vec!["summary text".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_tool_summarization(true);

        let long = "x".repeat(zeph_tools::MAX_TOOL_OUTPUT_CHARS + 1000);
        let result = agent.maybe_summarize_tool_output(&long).await;
        assert!(result.contains("summary text"));
        assert!(result.contains("[tool output summary]"));
        assert!(!result.contains("truncated"));
    }

    #[tokio::test]
    async fn test_summarize_tool_output_llm_failure_fallback() {
        let provider = mock_provider_failing();
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_tool_summarization(true);

        let long = "x".repeat(zeph_tools::MAX_TOOL_OUTPUT_CHARS + 1000);
        let result = agent.maybe_summarize_tool_output(&long).await;
        assert!(result.contains("truncated"));
    }

    #[test]
    fn with_tool_summarization_sets_flag() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_tool_summarization(true);
        assert!(agent.runtime.summarize_tool_output_enabled);

        let provider2 = mock_provider(vec![]);
        let channel2 = MockChannel::new(vec![]);
        let registry2 = create_test_registry();
        let executor2 = MockToolExecutor::no_tools();

        let agent2 = Agent::new(provider2, channel2, registry2, None, 5, executor2)
            .with_tool_summarization(false);
        assert!(!agent2.runtime.summarize_tool_output_enabled);
    }

    #[test]
    fn enqueue_or_merge_adds_new_message() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.enqueue_or_merge("hello".into());
        assert_eq!(agent.message_queue.len(), 1);
        assert_eq!(agent.message_queue[0].text, "hello");
    }

    #[test]
    fn enqueue_or_merge_merges_within_window() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.enqueue_or_merge("first".into());
        agent.enqueue_or_merge("second".into());
        assert_eq!(agent.message_queue.len(), 1);
        assert_eq!(agent.message_queue[0].text, "first\nsecond");
    }

    #[test]
    fn enqueue_or_merge_no_merge_after_window() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.message_queue.push_back(QueuedMessage {
            text: "old".into(),
            received_at: Instant::now() - Duration::from_secs(2),
        });
        agent.enqueue_or_merge("new".into());
        assert_eq!(agent.message_queue.len(), 2);
        assert_eq!(agent.message_queue[0].text, "old");
        assert_eq!(agent.message_queue[1].text, "new");
    }

    #[test]
    fn enqueue_or_merge_respects_max_queue_size() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..MAX_QUEUE_SIZE {
            agent.message_queue.push_back(QueuedMessage {
                text: format!("msg{i}"),
                received_at: Instant::now() - Duration::from_secs(2),
            });
        }
        agent.enqueue_or_merge("overflow".into());
        assert_eq!(agent.message_queue.len(), MAX_QUEUE_SIZE);
    }

    #[test]
    fn clear_queue_returns_count_and_empties() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.enqueue_or_merge("a".into());
        // Wait past merge window
        agent.message_queue.back_mut().unwrap().received_at =
            Instant::now() - Duration::from_secs(1);
        agent.enqueue_or_merge("b".into());
        assert_eq!(agent.message_queue.len(), 2);

        let count = agent.clear_queue();
        assert_eq!(count, 2);
        assert!(agent.message_queue.is_empty());
    }

    #[test]
    fn drain_channel_fills_queue() {
        let messages: Vec<String> = (0..5).map(|i| format!("msg{i}")).collect();
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(messages);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.drain_channel();
        // All 5 messages arrive within the merge window, so they merge into 1
        assert_eq!(agent.message_queue.len(), 1);
        assert!(agent.message_queue[0].text.contains("msg0"));
        assert!(agent.message_queue[0].text.contains("msg4"));
    }

    #[test]
    fn drain_channel_stops_at_max_queue_size() {
        let messages: Vec<String> = (0..15).map(|i| format!("msg{i}")).collect();
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(messages);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Pre-fill queue to near capacity with old timestamps (outside merge window)
        for i in 0..MAX_QUEUE_SIZE - 1 {
            agent.message_queue.push_back(QueuedMessage {
                text: format!("pre{i}"),
                received_at: Instant::now() - Duration::from_secs(2),
            });
        }
        agent.drain_channel();
        // One more slot was available; all 15 messages merge into it
        assert_eq!(agent.message_queue.len(), MAX_QUEUE_SIZE);
    }

    #[test]
    fn queue_fifo_order() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..3 {
            agent.message_queue.push_back(QueuedMessage {
                text: format!("msg{i}"),
                received_at: Instant::now() - Duration::from_secs(2),
            });
        }

        assert_eq!(agent.message_queue.pop_front().unwrap().text, "msg0");
        assert_eq!(agent.message_queue.pop_front().unwrap().text, "msg1");
        assert_eq!(agent.message_queue.pop_front().unwrap().text, "msg2");
    }

    #[test]
    fn doom_loop_detection_triggers_on_identical_outputs() {
        let s = "same output".to_owned();
        let history = vec![s.clone(), s.clone(), s];
        let recent = &history[history.len() - DOOM_LOOP_WINDOW..];
        assert!(recent.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn doom_loop_detection_no_trigger_on_different_outputs() {
        let history = vec![
            "output a".to_owned(),
            "output b".to_owned(),
            "output c".to_owned(),
        ];
        let recent = &history[history.len() - DOOM_LOOP_WINDOW..];
        assert!(!recent.windows(2).all(|w| w[0] == w[1]));
    }
}
