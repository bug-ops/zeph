mod context;
#[cfg(feature = "index")]
mod index;
mod learning;
#[cfg(feature = "mcp")]
mod mcp;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio_stream::StreamExt;
use zeph_llm::provider::{LlmProvider, Message, MessagePart, Role};

use crate::metrics::MetricsSnapshot;
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::sqlite::role_str;
use zeph_skills::loader::Skill;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::prompt::{format_skills_catalog, format_skills_prompt};
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::SkillEvent;
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

use crate::channel::Channel;
use crate::config::Config;
#[cfg(feature = "self-learning")]
use crate::config::LearningConfig;
use crate::config::{SecurityConfig, TimeoutConfig};
use crate::config_watcher::ConfigEvent;
use crate::context::{ContextBudget, EnvironmentContext, build_system_prompt};
use crate::redact::redact_secrets;
use zeph_memory::semantic::estimate_tokens;

const DOOM_LOOP_WINDOW: usize = 3;
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
    config_path: Option<PathBuf>,
    config_reload_rx: Option<mpsc::Receiver<ConfigEvent>>,
    memory: Option<SemanticMemory<P>>,
    conversation_id: Option<zeph_memory::ConversationId>,
    history_limit: u32,
    recall_limit: usize,
    summarization_threshold: usize,
    shutdown: watch::Receiver<bool>,
    active_skill_names: Vec<String>,
    metrics_tx: Option<watch::Sender<MetricsSnapshot>>,
    security: SecurityConfig,
    timeouts: TimeoutConfig,
    context_budget: Option<ContextBudget>,
    compaction_threshold: f32,
    compaction_preserve_tail: usize,
    last_skills_prompt: String,
    model_name: String,
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
    start_time: Instant,
    message_queue: VecDeque<QueuedMessage>,
    prune_protect_tokens: usize,
    cross_session_score_threshold: f32,
    summarize_tool_output_enabled: bool,
    permission_policy: zeph_tools::PermissionPolicy,
    #[cfg(feature = "mcp")]
    mcp_allowed_commands: Vec<String>,
    #[cfg(feature = "mcp")]
    mcp_max_dynamic: usize,
    #[cfg(feature = "index")]
    code_retriever: Option<std::sync::Arc<zeph_index::retriever::CodeRetriever<P>>>,
    #[cfg(feature = "index")]
    repo_map_tokens: usize,
    #[cfg(feature = "index")]
    cached_repo_map: Option<(String, std::time::Instant)>,
    #[cfg(feature = "index")]
    repo_map_ttl: std::time::Duration,
    warmup_ready: Option<watch::Receiver<bool>>,
    max_tool_iterations: usize,
    doom_loop_history: Vec<String>,
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
        let system_prompt = build_system_prompt(&skills_prompt, None, None);
        tracing::debug!(len = system_prompt.len(), "initial system prompt built");
        tracing::trace!(prompt = %system_prompt, "full system prompt");

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
            registry,
            skill_paths: Vec::new(),
            matcher,
            max_active_skills,
            embedding_model: String::new(),
            skill_reload_rx: None,
            config_path: None,
            config_reload_rx: None,
            memory: None,
            conversation_id: None,
            history_limit: 50,
            recall_limit: 5,
            summarization_threshold: 50,
            shutdown: rx,
            active_skill_names: Vec::new(),
            metrics_tx: None,
            security: SecurityConfig::default(),
            timeouts: TimeoutConfig::default(),
            context_budget: None,
            compaction_threshold: 0.80,
            compaction_preserve_tail: 6,
            last_skills_prompt: skills_prompt,
            model_name: String::new(),
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
            start_time: Instant::now(),
            message_queue: VecDeque::new(),
            prune_protect_tokens: 40_000,
            cross_session_score_threshold: 0.35,
            summarize_tool_output_enabled: false,
            permission_policy: zeph_tools::PermissionPolicy::default(),
            #[cfg(feature = "mcp")]
            mcp_allowed_commands: Vec::new(),
            #[cfg(feature = "mcp")]
            mcp_max_dynamic: 10,
            #[cfg(feature = "index")]
            code_retriever: None,
            #[cfg(feature = "index")]
            repo_map_tokens: 0,
            #[cfg(feature = "index")]
            cached_repo_map: None,
            #[cfg(feature = "index")]
            repo_map_ttl: std::time::Duration::from_secs(300),
            warmup_ready: None,
            max_tool_iterations: 10,
            doom_loop_history: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_max_tool_iterations(mut self, max: usize) -> Self {
        self.max_tool_iterations = max;
        self
    }

    #[must_use]
    pub fn with_memory(
        mut self,
        memory: SemanticMemory<P>,
        conversation_id: zeph_memory::ConversationId,
        history_limit: u32,
        recall_limit: usize,
        summarization_threshold: usize,
    ) -> Self {
        let has_qdrant = memory.has_qdrant();
        self.memory = Some(memory);
        self.conversation_id = Some(conversation_id);
        self.history_limit = history_limit;
        self.recall_limit = recall_limit;
        self.summarization_threshold = summarization_threshold;
        self.update_metrics(|m| {
            m.qdrant_available = has_qdrant;
            m.sqlite_conversation_id = Some(conversation_id);
        });
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

    #[must_use]
    pub fn with_config_reload(mut self, path: PathBuf, rx: mpsc::Receiver<ConfigEvent>) -> Self {
        self.config_path = Some(path);
        self.config_reload_rx = Some(rx);
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

    #[must_use]
    pub fn with_tool_summarization(mut self, enabled: bool) -> Self {
        self.summarize_tool_output_enabled = enabled;
        self
    }

    #[must_use]
    pub fn with_permission_policy(mut self, policy: zeph_tools::PermissionPolicy) -> Self {
        self.permission_policy = policy;
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
            self.context_budget = Some(ContextBudget::new(budget_tokens, reserve_ratio));
        }
        self.compaction_threshold = compaction_threshold;
        self.compaction_preserve_tail = compaction_preserve_tail;
        self.prune_protect_tokens = prune_protect_tokens;
        self
    }

    #[must_use]
    pub fn with_model_name(mut self, name: impl Into<String>) -> Self {
        self.model_name = name.into();
        self
    }

    #[must_use]
    pub fn with_warmup_ready(mut self, rx: watch::Receiver<bool>) -> Self {
        self.warmup_ready = Some(rx);
        self
    }

    #[cfg(feature = "index")]
    #[must_use]
    pub fn with_code_retriever(
        mut self,
        retriever: std::sync::Arc<zeph_index::retriever::CodeRetriever<P>>,
        repo_map_tokens: usize,
        repo_map_ttl_secs: u64,
    ) -> Self {
        self.code_retriever = Some(retriever);
        self.repo_map_tokens = repo_map_tokens;
        self.repo_map_ttl = std::time::Duration::from_secs(repo_map_ttl_secs);
        self
    }

    #[must_use]
    pub fn with_metrics(mut self, tx: watch::Sender<MetricsSnapshot>) -> Self {
        let provider_name = self.provider.name().to_string();
        let model_name = self.model_name.clone();
        let total_skills = self.registry.all_meta().len();
        let qdrant_available = self
            .memory
            .as_ref()
            .is_some_and(zeph_memory::semantic::SemanticMemory::has_qdrant);
        let conversation_id = self.conversation_id;
        let prompt_estimate = self
            .messages
            .first()
            .map_or(0, |m| u64::try_from(m.content.len()).unwrap_or(0) / 4);
        #[cfg(feature = "mcp")]
        let mcp_tool_count = self.mcp_tools.len();
        #[cfg(feature = "mcp")]
        let mcp_server_count = self
            .mcp_tools
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
            #[cfg(feature = "mcp")]
            {
                m.mcp_tool_count = mcp_tool_count;
                m.mcp_server_count = mcp_server_count;
            }
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
                vec![MessagePart::CodeContext { text: content }],
            ),
        );
    }

    #[must_use]
    pub fn context_messages(&self) -> &[Message] {
        &self.messages
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

        if let Ok(count) = memory.message_count(cid).await {
            let count_u64 = u64::try_from(count).unwrap_or(0);
            self.update_metrics(|m| {
                m.sqlite_message_count = count_u64;
            });
        }

        Ok(())
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
                    Some(_) = recv_skill_event(&mut self.skill_reload_rx) => {
                        self.reload_skills().await;
                        continue;
                    }
                    Some(_) = recv_config_event(&mut self.config_reload_rx) => {
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

    async fn process_user_message(&mut self, text: String) -> anyhow::Result<()> {
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

        #[cfg(feature = "mcp")]
        if trimmed == "/mcp" || trimmed.starts_with("/mcp ") {
            let args = trimmed.strip_prefix("/mcp").unwrap_or("").trim();
            self.handle_mcp_command(args).await?;
            return Ok(());
        }

        self.rebuild_system_prompt(&text).await;

        if let Err(e) = self.maybe_compact().await {
            tracing::warn!("context compaction failed: {e:#}");
        }

        if let Err(e) = self.prepare_context(trimmed).await {
            tracing::warn!("context preparation failed: {e:#}");
        }

        #[cfg(feature = "self-learning")]
        {
            self.reflection_used = false;
        }

        self.messages.push(Message {
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
        self.last_skills_prompt.clone_from(&skills_prompt);
        let system_prompt = build_system_prompt(&skills_prompt, None, None);
        if let Some(msg) = self.messages.first_mut() {
            msg.content = system_prompt;
        }

        tracing::info!("reloaded {} skill(s)", self.registry.all_meta().len());
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

        self.security = config.security;
        self.timeouts = config.timeouts;
        self.history_limit = config.memory.history_limit;
        self.recall_limit = config.memory.semantic.recall_limit;
        self.summarization_threshold = config.memory.summarization_threshold;
        self.max_active_skills = config.skills.max_active_skills;

        if config.memory.context_budget_tokens > 0 {
            self.context_budget = Some(ContextBudget::new(
                config.memory.context_budget_tokens,
                0.20,
            ));
        } else {
            self.context_budget = None;
        }
        self.compaction_threshold = config.memory.compaction_threshold;
        self.compaction_preserve_tail = config.memory.compaction_preserve_tail;
        self.prune_protect_tokens = config.memory.prune_protect_tokens;
        self.cross_session_score_threshold = config.memory.cross_session_score_threshold;

        #[cfg(feature = "index")]
        {
            self.repo_map_ttl = std::time::Duration::from_secs(config.index.repo_map_ttl_secs);
        }

        tracing::info!("config reloaded");
    }

    async fn process_response(&mut self) -> anyhow::Result<()> {
        self.doom_loop_history.clear();

        for iteration in 0..self.max_tool_iterations {
            self.channel.send_typing().await?;

            // Context budget check at 80% threshold
            if let Some(ref budget) = self.context_budget {
                let used: usize = self
                    .messages
                    .iter()
                    .map(|m| estimate_tokens(&m.content))
                    .sum();
                let threshold = budget.max_tokens() * 4 / 5;
                if used >= threshold {
                    tracing::warn!(
                        iteration,
                        used,
                        threshold,
                        "stopping tool loop: context budget nearing limit"
                    );
                    self.channel
                        .send("Stopping: context window is nearly full.")
                        .await?;
                    break;
                }
            }

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
                parts: vec![],
            });
            self.persist_message(Role::Assistant, &response).await;

            let result = self.tool_executor.execute(&response).await;
            if !self.handle_tool_result(&response, result).await? {
                return Ok(());
            }

            // Doom-loop detection: compare last N outputs by string equality
            if let Some(last_msg) = self.messages.last() {
                self.doom_loop_history.push(last_msg.content.clone());
                if self.doom_loop_history.len() >= DOOM_LOOP_WINDOW {
                    let recent =
                        &self.doom_loop_history[self.doom_loop_history.len() - DOOM_LOOP_WINDOW..];
                    if recent.windows(2).all(|w| w[0] == w[1]) {
                        tracing::warn!(
                            iteration,
                            "doom-loop detected: {DOOM_LOOP_WINDOW} consecutive identical outputs"
                        );
                        self.channel
                            .send("Stopping: detected repeated identical tool outputs.")
                            .await?;
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    async fn call_llm_with_timeout(&mut self) -> anyhow::Result<Option<String>> {
        let llm_timeout = std::time::Duration::from_secs(self.timeouts.llm_seconds);
        let start = std::time::Instant::now();
        let prompt_estimate: u64 = self
            .messages
            .iter()
            .map(|m| u64::try_from(m.content.len()).unwrap_or(0) / 4)
            .sum();

        if self.provider.supports_streaming() {
            if let Ok(r) =
                tokio::time::timeout(llm_timeout, self.process_response_streaming()).await
            {
                let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                self.update_metrics(|m| {
                    m.api_calls += 1;
                    m.last_llm_latency_ms = latency;
                    m.context_tokens = prompt_estimate;
                    m.prompt_tokens += prompt_estimate;
                    m.total_tokens = m.prompt_tokens + m.completion_tokens;
                });
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
                    let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let completion_estimate = u64::try_from(resp.len()).unwrap_or(0) / 4;
                    self.update_metrics(|m| {
                        m.api_calls += 1;
                        m.last_llm_latency_ms = latency;
                        m.context_tokens = prompt_estimate;
                        m.prompt_tokens += prompt_estimate;
                        m.completion_tokens += completion_estimate;
                        m.total_tokens = m.prompt_tokens + m.completion_tokens;
                    });
                    let display = self.maybe_redact(&resp);
                    self.channel.send(&display).await?;
                    Ok(Some(resp))
                }
                Ok(Err(e)) => Err(e.into()),
                Err(_) => {
                    self.channel
                        .send("LLM request timed out. Please try again.")
                        .await?;
                    Ok(None)
                }
            }
        }
    }

    fn last_user_query(&self) -> &str {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User && !m.content.starts_with("[tool output"))
            .map_or("", |m| m.content.as_str())
    }

    async fn summarize_tool_output(&self, output: &str) -> String {
        let truncated = zeph_tools::truncate_tool_output(output);
        let query = self.last_user_query();
        let prompt = format!(
            "The user asked: {query}\n\n\
             A tool produced output ({len} chars, truncated to fit).\n\
             Summarize the key information relevant to the user's question.\n\
             Preserve exact: file paths, error messages, numeric values, exit codes.\n\n\
             {truncated}",
            len = output.len(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
        }];

        match self.provider.chat(&messages).await {
            Ok(summary) => format!("[tool output summary]\n```\n{summary}\n```"),
            Err(e) => {
                tracing::warn!(
                    "tool output summarization failed, falling back to truncation: {e:#}"
                );
                truncated
            }
        }
    }

    async fn maybe_summarize_tool_output(&self, output: &str) -> String {
        if output.len() <= zeph_tools::MAX_TOOL_OUTPUT_CHARS {
            return output.to_string();
        }
        let overflow_notice = if let Some(path) = zeph_tools::save_overflow(output) {
            format!(
                "\n[full output saved to {}, use read tool to access]",
                path.display()
            )
        } else {
            String::new()
        };
        let truncated = if self.summarize_tool_output_enabled {
            self.summarize_tool_output(output).await
        } else {
            zeph_tools::truncate_tool_output(output)
        };
        format!("{truncated}{overflow_notice}")
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

                let processed = self.maybe_summarize_tool_output(&output.summary).await;
                let formatted_output = format_tool_output(&output.tool_name, &processed);
                let display = self.maybe_redact(&formatted_output);
                self.channel.send(&display).await?;

                self.messages.push(Message::from_parts(
                    Role::User,
                    vec![MessagePart::ToolOutput {
                        tool_name: output.tool_name.clone(),
                        body: processed,
                        compacted_at: None,
                    }],
                ));
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
                        let processed = self.maybe_summarize_tool_output(&out.summary).await;
                        let formatted = format_tool_output(&out.tool_name, &processed);
                        let display = self.maybe_redact(&formatted);
                        self.channel.send(&display).await?;
                        self.messages.push(Message::from_parts(
                            Role::User,
                            vec![MessagePart::ToolOutput {
                                tool_name: out.tool_name.clone(),
                                body: processed,
                                compacted_at: None,
                            }],
                        ));
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

        let completion_estimate = u64::try_from(response.len()).unwrap_or(0) / 4;
        self.update_metrics(|m| {
            m.completion_tokens += completion_estimate;
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
        });

        Ok(response)
    }

    fn maybe_redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.security.redact_secrets {
            redact_secrets(text)
        } else {
            std::borrow::Cow::Borrowed(text)
        }
    }

    async fn persist_message(&mut self, role: Role, content: &str) {
        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return;
        };

        let parts_json = self
            .messages
            .last()
            .filter(|m| !m.parts.is_empty())
            .and_then(|m| serde_json::to_string(&m.parts).ok())
            .unwrap_or_else(|| "[]".to_string());

        let (_message_id, embedding_stored) = match memory
            .remember_with_parts(cid, role_str(role), content, &parts_json)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                tracing::error!("failed to persist message: {e:#}");
                return;
            }
        };

        self.update_metrics(|m| {
            m.sqlite_message_count += 1;
            if embedding_stored {
                m.embeddings_generated += 1;
            }
        });

        self.check_summarization().await;
    }

    async fn check_summarization(&mut self) {
        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return;
        };

        let count = match memory.unsummarized_message_count(cid).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to get unsummarized message count: {e:#}");
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
            let _ = self.channel.send_status("summarizing...").await;
            let batch_size = self.summarization_threshold / 2;
            match memory.summarize(cid, batch_size).await {
                Ok(Some(summary_id)) => {
                    tracing::info!("created summary {summary_id} for conversation {cid}");
                    self.update_metrics(|m| {
                        m.summaries_count += 1;
                    });
                }
                Ok(None) => {
                    tracing::debug!("no summarization needed");
                }
                Err(e) => {
                    tracing::error!("summarization failed: {e:#}");
                }
            }
            let _ = self.channel.send_status("").await;
        }
    }
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

async fn recv_config_event(rx: &mut Option<mpsc::Receiver<ConfigEvent>>) -> Option<ConfigEvent> {
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
    use zeph_llm::provider::ChatStream;

    #[derive(Clone)]
    pub(super) struct MockProvider {
        responses: Arc<Mutex<Vec<String>>>,
        streaming: bool,
        embeddings: bool,
        fail_chat: bool,
    }

    impl MockProvider {
        pub(super) fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
                streaming: false,
                embeddings: false,
                fail_chat: false,
            }
        }

        fn with_streaming(mut self) -> Self {
            self.streaming = true;
            self
        }

        pub(super) fn failing() -> Self {
            Self {
                responses: Arc::new(Mutex::new(Vec::new())),
                streaming: false,
                embeddings: false,
                fail_chat: true,
            }
        }
    }

    impl LlmProvider for MockProvider {
        async fn chat(&self, _messages: &[Message]) -> Result<String, zeph_llm::LlmError> {
            if self.fail_chat {
                return Err(zeph_llm::LlmError::Other("mock LLM error".into()));
            }
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok("default response".to_string())
            } else {
                Ok(responses.remove(0))
            }
        }

        async fn chat_stream(
            &self,
            messages: &[Message],
        ) -> Result<ChatStream, zeph_llm::LlmError> {
            let response = self.chat(messages).await?;
            let chunks: Vec<_> = response.chars().map(|c| c.to_string()).map(Ok).collect();
            Ok(Box::pin(tokio_stream::iter(chunks)))
        }

        fn supports_streaming(&self) -> bool {
            self.streaming
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, zeph_llm::LlmError> {
            if self.embeddings {
                Ok(vec![0.1, 0.2, 0.3])
            } else {
                Err(zeph_llm::LlmError::EmbedUnsupported { provider: "mock" })
            }
        }

        fn supports_embeddings(&self) -> bool {
            self.embeddings
        }

        fn name(&self) -> &'static str {
            "mock"
        }
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
        let provider = MockProvider::new(vec!["response".to_string(), "retry".to_string()]);
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
        let provider = MockProvider::new(vec!["response".to_string()]);
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
        let provider = MockProvider::new(responses);
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
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec!["response".to_string()]);
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
        let provider = MockProvider::new(vec!["streaming response".to_string()]).with_streaming();
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
        let provider = MockProvider::new(vec!["response".to_string()]);
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
        let provider = MockProvider::new(vec!["response".to_string()]);
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
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.update_metrics(|m| m.api_calls = 999);
    }

    #[test]
    fn update_metrics_sets_uptime_seconds() {
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);
        assert_eq!(agent.last_user_query(), "");
    }

    #[tokio::test]
    async fn test_maybe_summarize_short_output_passthrough() {
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec!["summary text".to_string()]);
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
        let provider = MockProvider::failing();
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
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_tool_summarization(true);
        assert!(agent.summarize_tool_output_enabled);

        let provider2 = MockProvider::new(vec![]);
        let channel2 = MockChannel::new(vec![]);
        let registry2 = create_test_registry();
        let executor2 = MockToolExecutor::no_tools();

        let agent2 = Agent::new(provider2, channel2, registry2, None, 5, executor2)
            .with_tool_summarization(false);
        assert!(!agent2.summarize_tool_output_enabled);
    }

    #[test]
    fn enqueue_or_merge_adds_new_message() {
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
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
        let provider = MockProvider::new(vec![]);
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
