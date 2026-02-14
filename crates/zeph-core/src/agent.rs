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

// TODO(M14): Make configurable via AgentConfig (currently hardcoded for MVP)
const MAX_SHELL_ITERATIONS: usize = 3;
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
    conversation_id: Option<i64>,
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
    message_queue: VecDeque<QueuedMessage>,
    prune_protect_tokens: usize,
    summarize_tool_output_enabled: bool,
    #[cfg(feature = "mcp")]
    mcp_allowed_commands: Vec<String>,
    #[cfg(feature = "mcp")]
    mcp_max_dynamic: usize,
    #[cfg(feature = "index")]
    code_retriever: Option<std::sync::Arc<zeph_index::retriever::CodeRetriever<P>>>,
    #[cfg(feature = "index")]
    repo_map_tokens: usize,
    warmup_ready: Option<watch::Receiver<bool>>,
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
        let system_prompt = build_system_prompt(&skills_prompt, None);
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
            message_queue: VecDeque::new(),
            prune_protect_tokens: 40_000,
            summarize_tool_output_enabled: false,
            #[cfg(feature = "mcp")]
            mcp_allowed_commands: Vec::new(),
            #[cfg(feature = "mcp")]
            mcp_max_dynamic: 10,
            #[cfg(feature = "index")]
            code_retriever: None,
            #[cfg(feature = "index")]
            repo_map_tokens: 0,
            warmup_ready: None,
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
    ) -> Self {
        self.code_retriever = Some(retriever);
        self.repo_map_tokens = repo_map_tokens;
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
            tx.send_modify(f);
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn should_compact(&self) -> bool {
        let Some(ref budget) = self.context_budget else {
            return false;
        };
        let total_tokens: usize = self
            .messages
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum();
        let threshold = (budget.max_tokens() as f32 * self.compaction_threshold) as usize;
        let should = total_tokens > threshold;
        tracing::debug!(
            total_tokens,
            threshold,
            message_count = self.messages.len(),
            should_compact = should,
            "context budget check"
        );
        should
    }

    async fn compact_context(&mut self) -> anyhow::Result<()> {
        let preserve_tail = self.compaction_preserve_tail;

        if self.messages.len() <= preserve_tail + 1 {
            return Ok(());
        }

        let compact_end = self.messages.len() - preserve_tail;
        let to_compact = &self.messages[1..compact_end];
        if to_compact.is_empty() {
            return Ok(());
        }

        let history_text: String = to_compact
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => "system",
                };
                format!("[{role}]: {}", m.content)
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let compaction_prompt = format!(
            "Summarize this conversation excerpt into a structured continuation note. \
             Include:\n\
             1. Task overview\n\
             2. Current state\n\
             3. Key discoveries (file paths, errors, decisions)\n\
             4. Next steps\n\
             5. Critical context (variable names, config values)\n\
             \n\
             Keep it concise but preserve all actionable details.\n\
             \n\
             Conversation:\n{history_text}"
        );

        let summary = self
            .provider
            .chat(&[Message {
                role: Role::User,
                content: compaction_prompt,
                parts: vec![],
            }])
            .await?;

        let compacted_count = to_compact.len();
        self.messages.drain(1..compact_end);
        self.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!(
                    "[conversation summary — {compacted_count} messages compacted]\n{summary}"
                ),
                parts: vec![],
            },
        );

        tracing::info!(
            compacted_count,
            summary_tokens = estimate_tokens(&summary),
            "compacted context"
        );

        self.update_metrics(|m| {
            m.context_compactions += 1;
        });

        if let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id)
            && let Err(e) = memory.store_session_summary(cid, &summary).await
        {
            tracing::warn!("failed to store session summary: {e:#}");
        }

        Ok(())
    }

    /// Prune tool output bodies outside the protection zone, oldest first.
    /// Returns the number of tokens freed.
    #[allow(clippy::cast_precision_loss)]
    fn prune_tool_outputs(&mut self, min_to_free: usize) -> usize {
        let protect = self.prune_protect_tokens;
        let mut tail_tokens = 0usize;
        let mut protection_boundary = self.messages.len();
        if protect > 0 {
            for (i, msg) in self.messages.iter().enumerate().rev() {
                tail_tokens += estimate_tokens(&msg.content);
                if tail_tokens >= protect {
                    protection_boundary = i;
                    break;
                }
                if i == 0 {
                    protection_boundary = 0;
                }
            }
        }

        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        for msg in &mut self.messages[..protection_boundary] {
            if freed >= min_to_free {
                break;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let &mut MessagePart::ToolOutput {
                    ref body,
                    ref mut compacted_at,
                    ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                {
                    freed += estimate_tokens(body);
                    *compacted_at = Some(now);
                    modified = true;
                }
            }
            if modified {
                msg.rebuild_content();
            }
        }

        if freed > 0 {
            self.update_metrics(|m| m.tool_output_prunes += 1);
            tracing::info!(freed, protection_boundary, "pruned tool outputs");
        }
        freed
    }

    /// Two-tier compaction: Tier 1 prunes tool outputs, Tier 2 falls back to full LLM compaction.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    async fn maybe_compact(&mut self) -> anyhow::Result<()> {
        if !self.should_compact() {
            return Ok(());
        }

        let budget = self
            .context_budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        let total_tokens: usize = self
            .messages
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum();
        let threshold = (budget as f32 * self.compaction_threshold) as usize;
        let min_to_free = total_tokens.saturating_sub(threshold);

        let freed = self.prune_tool_outputs(min_to_free);
        if freed >= min_to_free {
            tracing::info!(freed, "tier-1 pruning sufficient");
            return Ok(());
        }

        tracing::info!(
            freed,
            min_to_free,
            "tier-1 insufficient, falling back to tier-2 compaction"
        );
        let _ = self.channel.send_status("compacting context...").await;
        let result = self.compact_context().await;
        let _ = self.channel.send_status("").await;
        result
    }

    fn remove_recall_messages(&mut self) {
        self.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::Recall { .. }))
            {
                return false;
            }
            !m.content.starts_with(RECALL_PREFIX)
        });
    }

    async fn inject_semantic_recall(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> anyhow::Result<()> {
        self.remove_recall_messages();

        let Some(memory) = &self.memory else {
            return Ok(());
        };
        if self.recall_limit == 0 || token_budget == 0 {
            return Ok(());
        }

        let recalled = memory.recall(query, self.recall_limit, None).await?;
        if recalled.is_empty() {
            return Ok(());
        }

        let mut recall_text = String::from(RECALL_PREFIX);
        let mut tokens_used = estimate_tokens(&recall_text);

        for item in &recalled {
            let role_label = match item.message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };
            let entry = format!("- [{}] {}\n", role_label, item.message.content);
            let entry_tokens = estimate_tokens(&entry);
            if tokens_used + entry_tokens > token_budget {
                break;
            }
            recall_text.push_str(&entry);
            tokens_used += entry_tokens;
        }

        if tokens_used > estimate_tokens(RECALL_PREFIX) && self.messages.len() > 1 {
            self.messages.insert(
                1,
                Message::from_parts(
                    Role::System,
                    vec![MessagePart::Recall { text: recall_text }],
                ),
            );
        }

        Ok(())
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

    #[cfg(feature = "index")]
    async fn inject_code_rag(&mut self, query: &str, token_budget: usize) -> anyhow::Result<()> {
        let Some(retriever) = &self.code_retriever else {
            return Ok(());
        };
        if token_budget == 0 {
            return Ok(());
        }

        let result = retriever.retrieve(query, token_budget).await?;
        let context_text = zeph_index::retriever::format_as_context(&result);

        if !context_text.is_empty() {
            self.inject_code_context(&context_text);
            tracing::debug!(
                strategy = ?result.strategy,
                chunks = result.chunks.len(),
                tokens = result.total_tokens,
                "code context injected"
            );
        }

        Ok(())
    }

    fn remove_code_context_messages(&mut self) {
        self.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::CodeContext { .. }))
            {
                return false;
            }
            !m.content.starts_with(CODE_CONTEXT_PREFIX)
        });
    }

    fn remove_summary_messages(&mut self) {
        self.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::Summary { .. }))
            {
                return false;
            }
            !m.content.starts_with(SUMMARY_PREFIX)
        });
    }

    fn remove_cross_session_messages(&mut self) {
        self.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::CrossSession { .. }))
            {
                return false;
            }
            !m.content.starts_with(CROSS_SESSION_PREFIX)
        });
    }

    async fn inject_cross_session_context(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> anyhow::Result<()> {
        self.remove_cross_session_messages();

        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return Ok(());
        };
        if token_budget == 0 {
            return Ok(());
        }

        let results = memory.search_session_summaries(query, 5, Some(cid)).await?;
        if results.is_empty() {
            return Ok(());
        }

        let mut text = String::from(CROSS_SESSION_PREFIX);
        let mut tokens_used = estimate_tokens(&text);

        for item in &results {
            let entry = format!("- {}\n", item.summary_text);
            let cost = estimate_tokens(&entry);
            if tokens_used + cost > token_budget {
                break;
            }
            text.push_str(&entry);
            tokens_used += cost;
        }

        if tokens_used > estimate_tokens(CROSS_SESSION_PREFIX) && self.messages.len() > 1 {
            self.messages.insert(
                1,
                Message::from_parts(Role::System, vec![MessagePart::CrossSession { text }]),
            );
            tracing::debug!(tokens_used, "injected cross-session context");
        }

        Ok(())
    }

    async fn inject_summaries(&mut self, token_budget: usize) -> anyhow::Result<()> {
        self.remove_summary_messages();

        let (Some(memory), Some(cid)) = (&self.memory, self.conversation_id) else {
            return Ok(());
        };
        if token_budget == 0 {
            return Ok(());
        }

        let summaries = memory.load_summaries(cid).await?;
        if summaries.is_empty() {
            return Ok(());
        }

        let mut summary_text = String::from(SUMMARY_PREFIX);
        let mut tokens_used = estimate_tokens(&summary_text);

        for summary in summaries.iter().rev() {
            let entry = format!(
                "- Messages {}-{}: {}\n",
                summary.first_message_id, summary.last_message_id, summary.content
            );
            let cost = estimate_tokens(&entry);
            if tokens_used + cost > token_budget {
                break;
            }
            summary_text.push_str(&entry);
            tokens_used += cost;
        }

        if tokens_used > estimate_tokens(SUMMARY_PREFIX) && self.messages.len() > 1 {
            self.messages.insert(
                1,
                Message::from_parts(
                    Role::System,
                    vec![MessagePart::Summary { text: summary_text }],
                ),
            );
            tracing::debug!(tokens_used, "injected summaries into context");
        }

        Ok(())
    }

    fn trim_messages_to_budget(&mut self, token_budget: usize) {
        if token_budget == 0 {
            return;
        }

        let history_start = self
            .messages
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(self.messages.len());

        if history_start >= self.messages.len() {
            return;
        }

        let mut total = 0usize;
        let mut keep_from = self.messages.len();

        for i in (history_start..self.messages.len()).rev() {
            let msg_tokens = estimate_tokens(&self.messages[i].content);
            if total + msg_tokens > token_budget {
                break;
            }
            total += msg_tokens;
            keep_from = i;
        }

        if keep_from > history_start {
            let removed = keep_from - history_start;
            self.messages.drain(history_start..keep_from);
            tracing::info!(
                removed,
                token_budget,
                "trimmed messages to fit context budget"
            );
        }
    }

    async fn prepare_context(&mut self, query: &str) -> anyhow::Result<()> {
        let Some(ref budget) = self.context_budget else {
            return Ok(());
        };

        let system_prompt = self.messages.first().map_or("", |m| m.content.as_str());
        let alloc = budget.allocate(system_prompt, &self.last_skills_prompt);

        self.inject_summaries(alloc.summaries).await?;

        self.inject_cross_session_context(query, alloc.cross_session)
            .await?;

        self.inject_semantic_recall(query, alloc.semantic_recall)
            .await?;

        #[cfg(feature = "index")]
        self.inject_code_rag(query, alloc.code_context).await?;

        self.trim_messages_to_budget(alloc.recent_history);

        Ok(())
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
                let mcp_total = self.mcp_tools.len();
                let mcp_servers = self
                    .mcp_tools
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
                let mcp_total = self.mcp_tools.len();
                let mcp_servers = self
                    .mcp_tools
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
        self.last_skills_prompt.clone_from(&skills_prompt);
        let system_prompt = build_system_prompt(&skills_prompt, None);
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

        tracing::info!("config reloaded");
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

        let skill_names = self.active_skill_names.clone();
        let total = all_meta.len();
        self.update_metrics(|m| {
            m.active_skills = skill_names;
            m.total_skills = total;
        });

        if !self.active_skill_names.is_empty()
            && let Some(memory) = &self.memory
        {
            let names: Vec<&str> = self.active_skill_names.iter().map(String::as_str).collect();
            if let Err(e) = memory.sqlite().record_skill_usage(&names).await {
                tracing::warn!("failed to record skill usage: {e:#}");
            }
        }

        let all_skills: Vec<Skill> = self
            .registry
            .all_meta()
            .iter()
            .filter_map(|m| self.registry.get_skill(&m.name).ok())
            .collect();
        let active_skills: Vec<Skill> = self
            .active_skill_names
            .iter()
            .filter_map(|name| self.registry.get_skill(name).ok())
            .collect();
        let remaining_skills: Vec<Skill> = all_skills
            .iter()
            .filter(|s| !self.active_skill_names.contains(&s.name().to_string()))
            .cloned()
            .collect();

        let skills_prompt = format_skills_prompt(&active_skills, std::env::consts::OS);
        let catalog_prompt = format_skills_catalog(&remaining_skills);
        self.last_skills_prompt.clone_from(&skills_prompt);
        let env = EnvironmentContext::gather(&self.model_name);
        #[allow(unused_mut)]
        let mut system_prompt = build_system_prompt(&skills_prompt, Some(&env));

        if !catalog_prompt.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&catalog_prompt);
        }

        #[cfg(feature = "mcp")]
        self.append_mcp_prompt(query, &mut system_prompt).await;

        let cwd = std::env::current_dir().unwrap_or_default();
        let project_configs = crate::project::discover_project_configs(&cwd);
        let project_context = crate::project::load_project_context(&project_configs);
        if !project_context.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&project_context);
        }

        #[cfg(feature = "index")]
        if self.code_retriever.is_some() && self.repo_map_tokens > 0 {
            match zeph_index::repo_map::generate_repo_map(&cwd, self.repo_map_tokens) {
                Ok(map) if !map.is_empty() => {
                    system_prompt.push_str("\n\n");
                    system_prompt.push_str(&map);
                }
                Ok(_) => {}
                Err(e) => tracing::debug!("repo map generation failed: {e:#}"),
            }
        }

        tracing::debug!(
            len = system_prompt.len(),
            skills = ?self.active_skill_names,
            "system prompt rebuilt"
        );
        tracing::trace!(prompt = %system_prompt, "full system prompt");

        if let Some(msg) = self.messages.first_mut() {
            msg.content = system_prompt;
        }
    }

    #[cfg(feature = "mcp")]
    async fn append_mcp_prompt(&mut self, query: &str, system_prompt: &mut String) {
        let matched_tools = self.match_mcp_tools(query).await;
        let active_mcp: Vec<String> = matched_tools
            .iter()
            .map(zeph_mcp::McpTool::qualified_name)
            .collect();
        let mcp_total = self.mcp_tools.len();
        let mcp_servers = self
            .mcp_tools
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
                parts: vec![],
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
        if self.summarize_tool_output_enabled {
            return self.summarize_tool_output(output).await;
        }
        zeph_tools::truncate_tool_output(output)
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
        if let Err(e) = memory.remember(cid, role_str(role), content).await {
            tracing::error!("failed to persist message: {e:#}");
            return;
        }

        self.update_metrics(|m| {
            m.sqlite_message_count += 1;
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
            parts: vec![],
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
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
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

async fn recv_config_event(rx: &mut Option<mpsc::Receiver<ConfigEvent>>) -> Option<ConfigEvent> {
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
        fail_chat: bool,
    }

    impl MockProvider {
        fn new(responses: Vec<String>) -> Self {
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

        fn failing() -> Self {
            Self {
                responses: Arc::new(Mutex::new(Vec::new())),
                streaming: false,
                embeddings: false,
                fail_chat: true,
            }
        }
    }

    impl LlmProvider for MockProvider {
        async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
            if self.fail_chat {
                anyhow::bail!("mock LLM error");
            }
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
    fn should_compact_disabled_without_budget() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        for i in 0..20 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i} with some content to add tokens"),
                parts: vec![],
            });
        }
        assert!(!agent.should_compact());
    }

    #[test]
    fn should_compact_below_threshold() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(1000, 0.20, 0.75, 4, 0);
        assert!(!agent.should_compact());
    }

    #[test]
    fn should_compact_above_threshold() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 4, 0);

        for i in 0..20 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message number {i} with enough content to push over budget"),
                parts: vec![],
            });
        }
        assert!(agent.should_compact());
    }

    #[tokio::test]
    async fn compact_context_preserves_system_and_tail() {
        let provider = MockProvider::new(vec!["compacted summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 2, 0);

        let system_content = agent.messages[0].content.clone();

        for i in 0..8 {
            agent.messages.push(Message {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: format!("message {i}"),
                parts: vec![],
            });
        }

        agent.compact_context().await.unwrap();

        assert_eq!(agent.messages[0].role, Role::System);
        assert_eq!(agent.messages[0].content, system_content);

        assert_eq!(agent.messages[1].role, Role::System);
        assert!(agent.messages[1].content.contains("[conversation summary"));

        let tail = &agent.messages[2..];
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].content, "message 6");
        assert_eq!(tail[1].content, "message 7");
    }

    #[tokio::test]
    async fn compact_context_too_few_messages() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 4, 0);

        agent.messages.push(Message {
            role: Role::User,
            content: "msg1".to_string(),
            parts: vec![],
        });
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "msg2".to_string(),
            parts: vec![],
        });

        let len_before = agent.messages.len();
        agent.compact_context().await.unwrap();
        assert_eq!(agent.messages.len(), len_before);
    }

    #[test]
    fn with_context_budget_zero_disables() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(0, 0.20, 0.75, 4, 0);
        assert!(agent.context_budget.is_none());
    }

    #[test]
    fn with_context_budget_nonzero_enables() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(4096, 0.20, 0.80, 6, 0);

        assert!(agent.context_budget.is_some());
        assert_eq!(agent.context_budget.as_ref().unwrap().max_tokens(), 4096);
        assert!((agent.compaction_threshold - 0.80).abs() < f32::EPSILON);
        assert_eq!(agent.compaction_preserve_tail, 6);
    }

    #[tokio::test]
    async fn compact_context_increments_metric() {
        let provider = MockProvider::new(vec!["summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 2, 0)
            .with_metrics(tx);

        for i in 0..8 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
            });
        }

        agent.compact_context().await.unwrap();
        assert_eq!(rx.borrow().context_compactions, 1);
    }

    #[tokio::test]
    async fn test_prepare_context_no_budget_is_noop() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent.prepare_context("test query").await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_recall_injection_removed_between_turns() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{RECALL_PREFIX}old recall data"),
                parts: vec![],
            },
        );
        assert_eq!(agent.messages.len(), 2);

        agent.remove_recall_messages();
        assert_eq!(agent.messages.len(), 1);
        assert!(!agent.messages[0].content.starts_with(RECALL_PREFIX));
    }

    #[tokio::test]
    async fn test_recall_without_qdrant_returns_empty() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent.inject_semantic_recall("test", 1000).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_trim_messages_preserves_system() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
            });
        }
        assert_eq!(agent.messages.len(), 11);

        agent.trim_messages_to_budget(5);

        assert_eq!(agent.messages[0].role, Role::System);
        assert!(agent.messages.len() < 11);
    }

    #[tokio::test]
    async fn test_trim_messages_keeps_recent() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("msg {i}"),
                parts: vec![],
            });
        }

        agent.trim_messages_to_budget(5);

        let last = agent.messages.last().unwrap();
        assert_eq!(last.content, "msg 9");
    }

    #[tokio::test]
    async fn test_trim_zero_budget_is_noop() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..5 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
            });
        }
        let msg_count = agent.messages.len();

        agent.trim_messages_to_budget(0);
        assert_eq!(agent.messages.len(), msg_count);
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

    async fn create_memory_with_summaries(
        provider: MockProvider,
        summaries: &[&str],
    ) -> (SemanticMemory<MockProvider>, i64) {
        let memory = SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test")
            .await
            .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        for content in summaries {
            let m1 = memory
                .sqlite()
                .save_message(cid, "user", "q")
                .await
                .unwrap();
            let m2 = memory
                .sqlite()
                .save_message(cid, "assistant", "a")
                .await
                .unwrap();
            memory
                .sqlite()
                .save_summary(cid, content, m1, m2, estimate_tokens(content) as i64)
                .await
                .unwrap();
        }
        (memory, cid)
    }

    #[tokio::test]
    async fn test_inject_summaries_no_memory_noop() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent.inject_summaries(1000).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_summaries_zero_budget_noop() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) = create_memory_with_summaries(provider.clone(), &["summary text"]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);
        let msg_count = agent.messages.len();

        agent.inject_summaries(0).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_summaries_empty_summaries_noop() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) = create_memory_with_summaries(provider.clone(), &[]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);
        let msg_count = agent.messages.len();

        agent.inject_summaries(1000).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_summaries_inserts_at_position_1() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) =
            create_memory_with_summaries(provider.clone(), &["User asked about Rust ownership"])
                .await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);

        agent.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        });

        agent.inject_summaries(1000).await.unwrap();

        assert_eq!(agent.messages[0].role, Role::System);
        assert!(agent.messages[1].content.starts_with(SUMMARY_PREFIX));
        assert_eq!(agent.messages[1].role, Role::System);
        assert!(
            agent.messages[1]
                .content
                .contains("User asked about Rust ownership")
        );
        assert_eq!(agent.messages[2].content, "hello");
    }

    #[tokio::test]
    async fn test_inject_summaries_removes_old_before_inject() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) =
            create_memory_with_summaries(provider.clone(), &["new summary data"]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{SUMMARY_PREFIX}old summary data"),
                parts: vec![],
            },
        );
        agent.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        });
        assert_eq!(agent.messages.len(), 3);

        agent.inject_summaries(1000).await.unwrap();

        let summary_msgs: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.content.starts_with(SUMMARY_PREFIX))
            .collect();
        assert_eq!(summary_msgs.len(), 1);
        assert!(summary_msgs[0].content.contains("new summary data"));
        assert!(!summary_msgs[0].content.contains("old summary data"));
    }

    #[tokio::test]
    async fn test_inject_summaries_respects_token_budget() {
        let provider = MockProvider::new(vec![]);
        // Each summary entry is "- Messages X-Y: <content>\n" (~prefix overhead + content)
        let (memory, cid) = create_memory_with_summaries(
            provider.clone(),
            &[
                "short",
                "this is a much longer summary that should consume more tokens",
            ],
        )
        .await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);

        agent.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        });

        // Use a very small budget: only the prefix + maybe one short entry
        let prefix_cost = estimate_tokens(SUMMARY_PREFIX);
        agent.inject_summaries(prefix_cost + 10).await.unwrap();

        let summary_msg = agent
            .messages
            .iter()
            .find(|m| m.content.starts_with(SUMMARY_PREFIX));

        if let Some(msg) = summary_msg {
            let token_count = estimate_tokens(&msg.content);
            assert!(token_count <= prefix_cost + 10);
        }
    }

    #[tokio::test]
    async fn test_remove_summary_messages_preserves_other_system() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{SUMMARY_PREFIX}old summary"),
                parts: vec![],
            },
        );
        agent.messages.insert(
            2,
            Message {
                role: Role::System,
                content: format!("{RECALL_PREFIX}recall data"),
                parts: vec![],
            },
        );
        assert_eq!(agent.messages.len(), 3);

        agent.remove_summary_messages();
        assert_eq!(agent.messages.len(), 2);
        assert!(agent.messages[1].content.starts_with(RECALL_PREFIX));
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
    fn test_prune_frees_tokens() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(1000, 0.20, 0.75, 4, 0)
            .with_metrics(tx);

        let big_body = "x".repeat(500);
        agent.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: big_body,
                compacted_at: None,
            }],
        ));

        let freed = agent.prune_tool_outputs(10);
        assert!(freed > 0);
        assert_eq!(rx.borrow().tool_output_prunes, 1);

        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.messages[1].parts[0] {
            assert!(compacted_at.is_some());
        } else {
            panic!("expected ToolOutput");
        }
    }

    #[test]
    fn test_prune_respects_protection_zone() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(10000, 0.20, 0.75, 4, 999_999);

        let big_body = "x".repeat(500);
        agent.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: big_body,
                compacted_at: None,
            }],
        ));

        let freed = agent.prune_tool_outputs(10);
        assert_eq!(freed, 0);

        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.messages[1].parts[0] {
            assert!(compacted_at.is_none());
        } else {
            panic!("expected ToolOutput");
        }
    }

    #[tokio::test]
    async fn test_tier2_after_insufficient_prune() {
        let provider = MockProvider::new(vec!["summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 2, 0)
            .with_metrics(tx);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i} with enough content to push over budget threshold"),
                parts: vec![],
            });
        }

        agent.maybe_compact().await.unwrap();
        assert_eq!(rx.borrow().context_compactions, 1);
    }

    #[tokio::test]
    async fn test_inject_cross_session_no_memory_noop() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent
            .inject_cross_session_context("test", 1000)
            .await
            .unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_cross_session_zero_budget_noop() {
        let provider = MockProvider::new(vec![]);
        let (memory, cid) = create_memory_with_summaries(provider.clone(), &["summary"]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50);
        let msg_count = agent.messages.len();

        agent.inject_cross_session_context("test", 0).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_remove_cross_session_messages() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message::from_parts(
                Role::System,
                vec![MessagePart::CrossSession {
                    text: "old cross-session".into(),
                }],
            ),
        );
        assert_eq!(agent.messages.len(), 2);

        agent.remove_cross_session_messages();
        assert_eq!(agent.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_remove_cross_session_preserves_other_system() {
        let provider = MockProvider::new(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message::from_parts(
                Role::System,
                vec![MessagePart::Summary {
                    text: "keep this summary".into(),
                }],
            ),
        );
        agent.messages.insert(
            2,
            Message::from_parts(
                Role::System,
                vec![MessagePart::CrossSession {
                    text: "remove this".into(),
                }],
            ),
        );
        assert_eq!(agent.messages.len(), 3);

        agent.remove_cross_session_messages();
        assert_eq!(agent.messages.len(), 2);
        assert!(agent.messages[1].content.contains("keep this summary"));
    }

    #[tokio::test]
    async fn test_store_session_summary_on_compaction() {
        let provider = MockProvider::new(vec!["compacted summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (memory, cid) = create_memory_with_summaries(provider.clone(), &[]).await;

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 50)
            .with_context_budget(10000, 0.20, 0.80, 2, 0);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
            });
        }

        // compact_context should succeed (non-fatal store)
        agent.compact_context().await.unwrap();
        assert!(agent.messages[1].content.contains("compacted summary"));
    }

    #[test]
    fn test_budget_allocation_cross_session() {
        let budget = crate::context::ContextBudget::new(1000, 0.20);
        let alloc = budget.allocate("", "");

        assert!(alloc.cross_session > 0);
        assert!(alloc.summaries > 0);
        assert!(alloc.semantic_recall > 0);
        // cross_session should be smaller than summaries
        assert!(alloc.cross_session < alloc.summaries);
    }
}
