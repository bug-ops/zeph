use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Notify, mpsc, watch};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;

use crate::channel::Channel;
use crate::config::{LearningConfig, SecurityConfig, TimeoutConfig};
use crate::config_watcher::ConfigEvent;
use crate::context::ContextBudget;
use crate::cost::CostTracker;
use crate::metrics::MetricsSnapshot;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::watcher::SkillEvent;
use zeph_tools::executor::ToolExecutor;

use super::Agent;

impl<C: Channel, T: ToolExecutor> Agent<C, T> {
    #[must_use]
    pub fn with_stt(mut self, stt: Box<dyn zeph_llm::stt::SpeechToText>) -> Self {
        self.stt = Some(stt);
        self
    }

    #[must_use]
    pub fn with_update_notifications(mut self, rx: mpsc::Receiver<String>) -> Self {
        self.update_notify_rx = Some(rx);
        self
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
    pub fn with_disambiguation_threshold(mut self, threshold: f32) -> Self {
        self.skill_state.disambiguation_threshold = threshold;
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

    pub(super) fn summary_or_primary_provider(&self) -> &AnyProvider {
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

    /// Returns a handle that can cancel the current in-flight operation.
    /// The returned `Notify` is stable across messages — callers invoke
    /// `notify_waiters()` to cancel whatever operation is running.
    #[must_use]
    pub fn cancel_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.cancel_signal)
    }
}
