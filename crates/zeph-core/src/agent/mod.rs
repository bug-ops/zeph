// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod builder;
mod context;
pub(crate) mod context_manager;
pub mod error;
pub(super) mod feedback_detector;
#[cfg(feature = "index")]
mod index;
mod learning;
pub(crate) mod learning_engine;
mod mcp;
mod message_queue;
mod persistence;
mod skill_management;
mod tool_execution;
pub(crate) mod tool_orchestrator;
mod trust_commands;
mod utils;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use std::sync::Arc;

use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role};
use zeph_llm::stt::SpeechToText;

use crate::metrics::MetricsSnapshot;
use std::collections::HashMap;
use zeph_memory::TokenCounter;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::loader::Skill;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::prompt::format_skills_prompt;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::SkillEvent;
use zeph_tools::executor::{ErasedToolExecutor, ToolExecutor};

use crate::channel::Channel;
use crate::config::Config;
use crate::config::{SecurityConfig, SkillPromptMode, TimeoutConfig};
use crate::config_watcher::ConfigEvent;
use crate::context::{ContextBudget, EnvironmentContext, build_system_prompt};
use crate::cost::CostTracker;
use crate::vault::Secret;

use message_queue::{MAX_AUDIO_BYTES, MAX_IMAGE_BYTES, QueuedMessage, detect_image_mime};

pub(crate) const DOOM_LOOP_WINDOW: usize = 3;
pub(crate) const DOCUMENT_RAG_PREFIX: &str = "## Relevant documents\n";
const TOOL_LOOP_KEEP_RECENT: usize = 4;
pub(crate) const RECALL_PREFIX: &str = "[semantic recall]\n";
pub(crate) const CODE_CONTEXT_PREFIX: &str = "[code context]\n";
pub(crate) const SUMMARY_PREFIX: &str = "[conversation summaries]\n";
pub(crate) const CROSS_SESSION_PREFIX: &str = "[cross-session context]\n";
pub(crate) const CORRECTIONS_PREFIX: &str = "[past corrections]\n";
pub(crate) const TOOL_OUTPUT_SUFFIX: &str = "\n```";

pub(crate) fn format_tool_output(tool_name: &str, body: &str) -> String {
    use std::fmt::Write;
    let capacity = "[tool output: ".len()
        + tool_name.len()
        + "]\n```\n".len()
        + body.len()
        + TOOL_OUTPUT_SUFFIX.len();
    let mut buf = String::with_capacity(capacity);
    let _ = write!(
        buf,
        "[tool output: {tool_name}]\n```\n{body}{TOOL_OUTPUT_SUFFIX}"
    );
    buf
}

pub(super) struct MemoryState {
    pub(super) memory: Option<Arc<SemanticMemory>>,
    pub(super) conversation_id: Option<zeph_memory::ConversationId>,
    pub(super) history_limit: u32,
    pub(super) recall_limit: usize,
    pub(super) summarization_threshold: usize,
    pub(super) cross_session_score_threshold: f32,
    pub(super) autosave_assistant: bool,
    pub(super) autosave_min_length: usize,
    pub(super) tool_call_cutoff: usize,
    pub(super) unsummarized_count: usize,
    pub(super) document_config: crate::config::DocumentConfig,
}

pub(super) struct SkillState {
    pub(super) registry: SkillRegistry,
    pub(super) skill_paths: Vec<PathBuf>,
    pub(super) managed_dir: Option<PathBuf>,
    pub(super) matcher: Option<SkillMatcherBackend>,
    pub(super) max_active_skills: usize,
    pub(super) disambiguation_threshold: f32,
    pub(super) embedding_model: String,
    pub(super) skill_reload_rx: Option<mpsc::Receiver<SkillEvent>>,
    pub(super) active_skill_names: Vec<String>,
    pub(super) last_skills_prompt: String,
    pub(super) prompt_mode: SkillPromptMode,
    /// Custom secrets available at runtime: key=hyphenated name, value=secret.
    pub(super) available_custom_secrets: HashMap<String, Secret>,
    pub(super) cosine_weight: f32,
    pub(super) hybrid_search: bool,
    pub(super) bm25_index: Option<zeph_skills::bm25::Bm25Index>,
}

pub(super) struct McpState {
    pub(super) tools: Vec<zeph_mcp::McpTool>,
    pub(super) registry: Option<zeph_mcp::McpToolRegistry>,
    pub(super) manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
    pub(super) allowed_commands: Vec<String>,
    pub(super) max_dynamic: usize,
    /// Shared with `McpToolExecutor` so native `tool_use` sees the current tool list.
    pub(super) shared_tools: Option<std::sync::Arc<std::sync::RwLock<Vec<zeph_mcp::McpTool>>>>,
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
    pub(super) permission_policy: zeph_tools::PermissionPolicy,
    pub(super) redact_credentials: bool,
}

pub struct Agent<C: Channel> {
    provider: AnyProvider,
    channel: C,
    pub(crate) tool_executor: Arc<dyn ErasedToolExecutor>,
    messages: Vec<Message>,
    pub(super) memory_state: MemoryState,
    pub(super) skill_state: SkillState,
    pub(super) context_manager: context_manager::ContextManager,
    pub(super) tool_orchestrator: tool_orchestrator::ToolOrchestrator,
    pub(super) learning_engine: learning_engine::LearningEngine,
    pub(super) feedback_detector: feedback_detector::FeedbackDetector,
    config_path: Option<PathBuf>,
    config_reload_rx: Option<mpsc::Receiver<ConfigEvent>>,
    shutdown: watch::Receiver<bool>,
    metrics_tx: Option<watch::Sender<MetricsSnapshot>>,
    pub(super) runtime: RuntimeConfig,
    pub(super) mcp: McpState,
    #[cfg(feature = "index")]
    pub(super) index: IndexState,
    cancel_signal: Arc<Notify>,
    cancel_token: CancellationToken,
    start_time: Instant,
    message_queue: VecDeque<QueuedMessage>,
    summary_provider: Option<AnyProvider>,
    /// Shared slot for runtime model switching; set by external caller (e.g. ACP).
    provider_override: Option<Arc<std::sync::RwLock<Option<AnyProvider>>>>,
    warmup_ready: Option<watch::Receiver<bool>>,
    cost_tracker: Option<CostTracker>,
    cached_prompt_tokens: u64,
    env_context: EnvironmentContext,
    pub(crate) token_counter: Arc<TokenCounter>,
    stt: Option<Box<dyn SpeechToText>>,
    update_notify_rx: Option<mpsc::Receiver<String>>,
    /// Manages spawned sub-agents. Wired up during construction but not yet
    /// dispatched to in the current agent loop iteration; retained for
    /// forward-compatible multi-agent orchestration.
    pub(crate) subagent_manager: Option<crate::subagent::SubAgentManager>,
    pub(super) response_cache: Option<std::sync::Arc<zeph_memory::ResponseCache>>,
    /// Parent tool call ID when this agent runs as a subagent inside another agent session.
    /// Propagated into every `LoopbackEvent::ToolStart` / `ToolOutput` so the IDE can build
    /// a subagent hierarchy.
    pub(crate) parent_tool_use_id: Option<String>,
    pub(super) anomaly_detector: Option<zeph_tools::AnomalyDetector>,
}

impl<C: Channel> Agent<C> {
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new(
        provider: AnyProvider,
        channel: C,
        registry: SkillRegistry,
        matcher: Option<SkillMatcherBackend>,
        max_active_skills: usize,
        tool_executor: impl ToolExecutor + 'static,
    ) -> Self {
        let all_skills: Vec<Skill> = registry
            .all_meta()
            .iter()
            .filter_map(|m| registry.get_skill(&m.name).ok())
            .collect();
        let empty_trust = HashMap::new();
        let empty_health: HashMap<String, (f64, u32)> = HashMap::new();
        let skills_prompt = format_skills_prompt(&all_skills, &empty_trust, &empty_health);
        let system_prompt = build_system_prompt(&skills_prompt, None, None, false);
        tracing::debug!(len = system_prompt.len(), "initial system prompt built");
        tracing::trace!(prompt = %system_prompt, "full system prompt");

        let initial_prompt_tokens = u64::try_from(system_prompt.len()).unwrap_or(0) / 4;
        let (_tx, rx) = watch::channel(false);
        let token_counter = Arc::new(TokenCounter::new());
        Self {
            provider,
            channel,
            tool_executor: Arc::new(tool_executor),
            messages: vec![Message {
                role: Role::System,
                content: system_prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }],
            memory_state: MemoryState {
                memory: None,
                conversation_id: None,
                history_limit: 50,
                recall_limit: 5,
                summarization_threshold: 50,
                cross_session_score_threshold: 0.35,
                autosave_assistant: false,
                autosave_min_length: 20,
                tool_call_cutoff: 6,
                unsummarized_count: 0,
                document_config: crate::config::DocumentConfig::default(),
            },
            skill_state: SkillState {
                registry,
                skill_paths: Vec::new(),
                managed_dir: None,
                matcher,
                max_active_skills,
                disambiguation_threshold: 0.05,
                embedding_model: String::new(),
                skill_reload_rx: None,
                active_skill_names: Vec::new(),
                last_skills_prompt: skills_prompt,
                prompt_mode: SkillPromptMode::Auto,
                available_custom_secrets: HashMap::new(),
                cosine_weight: 0.7,
                hybrid_search: false,
                bm25_index: None,
            },
            context_manager: context_manager::ContextManager::new(),
            tool_orchestrator: tool_orchestrator::ToolOrchestrator::new(),
            learning_engine: learning_engine::LearningEngine::new(),
            feedback_detector: feedback_detector::FeedbackDetector::new(0.6),
            config_path: None,
            config_reload_rx: None,
            shutdown: rx,
            metrics_tx: None,
            runtime: RuntimeConfig {
                security: SecurityConfig::default(),
                timeouts: TimeoutConfig::default(),
                model_name: String::new(),
                permission_policy: zeph_tools::PermissionPolicy::default(),
                redact_credentials: true,
            },
            mcp: McpState {
                tools: Vec::new(),
                registry: None,
                manager: None,
                allowed_commands: Vec::new(),
                max_dynamic: 10,
                shared_tools: None,
            },
            #[cfg(feature = "index")]
            index: IndexState {
                retriever: None,
                repo_map_tokens: 0,
                cached_repo_map: None,
                repo_map_ttl: std::time::Duration::from_secs(300),
            },
            cancel_signal: Arc::new(Notify::new()),
            cancel_token: CancellationToken::new(),
            start_time: Instant::now(),
            message_queue: VecDeque::new(),
            summary_provider: None,
            provider_override: None,
            warmup_ready: None,
            cost_tracker: None,
            cached_prompt_tokens: initial_prompt_tokens,
            env_context: EnvironmentContext::gather(""),
            token_counter,
            stt: None,
            update_notify_rx: None,
            subagent_manager: None,
            response_cache: None,
            parent_tool_use_id: None,
            anomaly_detector: None,
        }
    }

    /// Poll all active sub-agents for completed/failed/canceled results.
    ///
    /// Non-blocking: returns immediately with a list of `(task_id, result)` pairs
    /// for agents that have finished. Each completed agent is removed from the manager.
    pub async fn poll_subagents(&mut self) -> Vec<(String, String)> {
        let Some(mgr) = &mut self.subagent_manager else {
            return vec![];
        };

        let finished: Vec<String> = mgr
            .statuses()
            .into_iter()
            .filter_map(|(id, status)| {
                if matches!(
                    status.state,
                    crate::subagent::SubAgentState::Completed
                        | crate::subagent::SubAgentState::Failed
                        | crate::subagent::SubAgentState::Canceled
                ) {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();

        let mut results = vec![];
        for task_id in finished {
            match mgr.collect(&task_id).await {
                Ok(result) => results.push((task_id, result)),
                Err(e) => {
                    tracing::warn!(task_id, error = %e, "failed to collect sub-agent result");
                }
            }
        }
        results
    }

    pub async fn shutdown(&mut self) {
        self.channel.send("Shutting down...").await.ok();

        if let Some(ref mut mgr) = self.subagent_manager {
            mgr.shutdown_all();
        }

        if let Some(ref manager) = self.mcp.manager {
            manager.shutdown_all_shared().await;
        }

        if let Some(ref tx) = self.metrics_tx {
            let m = tx.borrow();
            if m.filter_applications > 0 {
                #[allow(clippy::cast_precision_loss)]
                let pct = if m.filter_raw_tokens > 0 {
                    m.filter_saved_tokens as f64 / m.filter_raw_tokens as f64 * 100.0
                } else {
                    0.0
                };
                tracing::info!(
                    raw_tokens = m.filter_raw_tokens,
                    saved_tokens = m.filter_saved_tokens,
                    applications = m.filter_applications,
                    "tool output filtering saved ~{} tokens ({pct:.0}%)",
                    m.filter_saved_tokens,
                );
            }
        }
        tracing::info!("agent shutdown complete");
    }

    /// Run the chat loop, receiving messages via the channel until EOF or shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if channel I/O or LLM communication fails.
    #[allow(clippy::too_many_lines)]
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
            // Apply any pending provider override (from ACP set_session_config_option).
            if let Some(ref slot) = self.provider_override
                && let Some(new_provider) = slot
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            {
                tracing::debug!(provider = new_provider.name(), "ACP model override applied");
                self.provider = new_provider;
            }

            // Refresh sub-agent status in metrics before polling.
            if let Some(ref mgr) = self.subagent_manager {
                let sub_agent_metrics: Vec<crate::metrics::SubAgentMetrics> = mgr
                    .statuses()
                    .into_iter()
                    .map(|(id, s)| {
                        let def = mgr.agents_def(&id);
                        crate::metrics::SubAgentMetrics {
                            name: def.map_or_else(
                                || id[..8.min(id.len())].to_owned(),
                                |d| d.name.clone(),
                            ),
                            id: id.clone(),
                            state: format!("{:?}", s.state).to_lowercase(),
                            turns_used: s.turns_used,
                            max_turns: def.map_or(20, |d| d.permissions.max_turns),
                            background: def.is_some_and(|d| d.permissions.background),
                            elapsed_secs: s.started_at.elapsed().as_secs(),
                        }
                    })
                    .collect();
                self.update_metrics(|m| m.sub_agents = sub_agent_metrics);
            }

            // Non-blocking poll: notify user when background sub-agents complete.
            let completed = self.poll_subagents().await;
            for (task_id, result) in completed {
                let notice = if result.is_empty() {
                    format!("[sub-agent {id}] completed (no output)", id = &task_id[..8])
                } else {
                    format!("[sub-agent {id}] completed:\n{result}", id = &task_id[..8])
                };
                if let Err(e) = self.channel.send(&notice).await {
                    tracing::warn!(error = %e, "failed to send sub-agent completion notice");
                }
            }

            self.drain_channel();

            let (text, image_parts) = if let Some(queued) = self.message_queue.pop_front() {
                self.notify_queue_count().await;
                if queued.raw_attachments.is_empty() {
                    (queued.text, queued.image_parts)
                } else {
                    let msg = crate::channel::ChannelMessage {
                        text: queued.text,
                        attachments: queued.raw_attachments,
                    };
                    self.resolve_message(msg).await
                }
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
                    Some(msg) = recv_optional(&mut self.update_notify_rx) => {
                        if let Err(e) = self.channel.send(&msg).await {
                            tracing::warn!("failed to send update notification: {e}");
                        }
                        continue;
                    }
                };
                let Some(msg) = incoming else { break };
                self.drain_channel();
                self.resolve_message(msg).await
            };

            let trimmed = text.trim();

            if trimmed == "/clear-queue" {
                let n = self.clear_queue();
                self.notify_queue_count().await;
                self.channel
                    .send(&format!("Cleared {n} queued messages."))
                    .await?;
                let _ = self.channel.flush_chunks().await;
                continue;
            }

            if trimmed == "/compact" {
                if self.messages.len() > self.context_manager.compaction_preserve_tail + 1 {
                    match self.compact_context().await {
                        Ok(()) => {
                            let _ = self.channel.send("Context compacted successfully.").await;
                        }
                        Err(e) => {
                            let _ = self.channel.send(&format!("Compaction failed: {e}")).await;
                        }
                    }
                } else {
                    let _ = self.channel.send("Nothing to compact.").await;
                }
                let _ = self.channel.flush_chunks().await;
                continue;
            }

            if trimmed == "/clear" {
                self.clear_history();
                let _ = self.channel.flush_chunks().await;
                continue;
            }

            if trimmed == "/model" || trimmed.starts_with("/model ") {
                self.handle_model_command(trimmed).await;
                let _ = self.channel.flush_chunks().await;
                continue;
            }

            self.process_user_message(text, image_parts).await?;
        }

        Ok(())
    }

    /// Switch the active provider to one serving `model_id`.
    ///
    /// Looks up the model in the provider's remote model list (or cache).
    ///
    /// # Errors
    ///
    /// Returns `Err` if the model is not found.
    pub fn set_model(&mut self, model_id: &str) -> Result<(), String> {
        if model_id.is_empty() {
            return Err("model id must not be empty".to_string());
        }
        if model_id.len() > 256 {
            return Err("model id exceeds maximum length of 256 characters".to_string());
        }
        if !model_id
            .chars()
            .all(|c| c.is_ascii() && !c.is_ascii_control())
        {
            return Err("model id must contain only printable ASCII characters".to_string());
        }
        self.runtime.model_name = model_id.to_string();
        tracing::info!(model = model_id, "set_model called");
        Ok(())
    }

    /// Handle `/model`, `/model <id>`, and `/model refresh` commands.
    async fn handle_model_command(&mut self, trimmed: &str) {
        let arg = trimmed.strip_prefix("/model").map_or("", str::trim);

        if arg == "refresh" {
            // Invalidate all model cache files in the cache directory.
            if let Some(cache_dir) = dirs::cache_dir() {
                let models_dir = cache_dir.join("zeph").join("models");
                if let Ok(entries) = std::fs::read_dir(&models_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|e| e.to_str()) == Some("json") {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
            match self.provider.list_models_remote().await {
                Ok(models) => {
                    let _ = self
                        .channel
                        .send(&format!("Fetched {} models.", models.len()))
                        .await;
                }
                Err(e) => {
                    let _ = self
                        .channel
                        .send(&format!("Error fetching models: {e}"))
                        .await;
                }
            }
            return;
        }

        if arg.is_empty() {
            // List models: try cache first, then remote.
            let cache = zeph_llm::model_cache::ModelCache::for_slug(self.provider.name());
            let models = if cache.is_stale() {
                None
            } else {
                cache.load().unwrap_or(None)
            };
            let models = if let Some(m) = models {
                m
            } else {
                match self.provider.list_models_remote().await {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = self
                            .channel
                            .send(&format!("Error fetching models: {e}"))
                            .await;
                        return;
                    }
                }
            };

            if models.is_empty() {
                let _ = self.channel.send("No models available.").await;
                return;
            }
            let mut lines = vec!["Available models:".to_string()];
            for (i, m) in models.iter().enumerate() {
                lines.push(format!("  {}. {} ({})", i + 1, m.display_name, m.id));
            }
            let _ = self.channel.send(&lines.join("\n")).await;
            return;
        }

        // `/model <id>` — switch model
        let model_id = arg;
        match self.set_model(model_id) {
            Ok(()) => {
                let _ = self
                    .channel
                    .send(&format!("Switched to model: {model_id}"))
                    .await;
            }
            Err(e) => {
                let _ = self.channel.send(&format!("Error: {e}")).await;
            }
        }
    }

    async fn resolve_message(
        &self,
        msg: crate::channel::ChannelMessage,
    ) -> (String, Vec<zeph_llm::provider::MessagePart>) {
        use crate::channel::{Attachment, AttachmentKind};
        use zeph_llm::provider::{ImageData, MessagePart};

        let text_base = msg.text.clone();

        let (audio_attachments, image_attachments): (Vec<Attachment>, Vec<Attachment>) = msg
            .attachments
            .into_iter()
            .partition(|a| a.kind == AttachmentKind::Audio);

        tracing::debug!(
            audio = audio_attachments.len(),
            has_stt = self.stt.is_some(),
            "resolve_message attachments"
        );

        let text = if !audio_attachments.is_empty()
            && let Some(stt) = self.stt.as_ref()
        {
            let mut transcribed_parts = Vec::new();
            for attachment in &audio_attachments {
                if attachment.data.len() > MAX_AUDIO_BYTES {
                    tracing::warn!(
                        size = attachment.data.len(),
                        max = MAX_AUDIO_BYTES,
                        "audio attachment exceeds size limit, skipping"
                    );
                    continue;
                }
                match stt
                    .transcribe(&attachment.data, attachment.filename.as_deref())
                    .await
                {
                    Ok(result) => {
                        tracing::info!(
                            len = result.text.len(),
                            language = ?result.language,
                            "audio transcribed"
                        );
                        transcribed_parts.push(result.text);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "audio transcription failed");
                    }
                }
            }
            if transcribed_parts.is_empty() {
                text_base
            } else {
                let transcribed = transcribed_parts.join("\n");
                if text_base.is_empty() {
                    transcribed
                } else {
                    format!("[transcribed audio]\n{transcribed}\n\n{text_base}")
                }
            }
        } else {
            if !audio_attachments.is_empty() {
                tracing::warn!(
                    count = audio_attachments.len(),
                    "audio attachments received but no STT provider configured, dropping"
                );
            }
            text_base
        };

        let mut image_parts = Vec::new();
        for attachment in image_attachments {
            if attachment.data.len() > MAX_IMAGE_BYTES {
                tracing::warn!(
                    size = attachment.data.len(),
                    max = MAX_IMAGE_BYTES,
                    "image attachment exceeds size limit, skipping"
                );
                continue;
            }
            let mime_type = detect_image_mime(attachment.filename.as_deref()).to_string();
            image_parts.push(MessagePart::Image(Box::new(ImageData {
                data: attachment.data,
                mime_type,
            })));
        }

        (text, image_parts)
    }

    #[allow(clippy::too_many_lines)]
    async fn process_user_message(
        &mut self,
        text: String,
        image_parts: Vec<zeph_llm::provider::MessagePart>,
    ) -> Result<(), error::AgentError> {
        self.cancel_token = CancellationToken::new();
        let signal = Arc::clone(&self.cancel_signal);
        let token = self.cancel_token.clone();
        tokio::spawn(async move {
            signal.notified().await;
            token.cancel();
        });
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

        if let Some(path) = trimmed.strip_prefix("/image ") {
            return self
                .handle_image_command(path.trim(), &mut image_parts.into_iter().collect())
                .await;
        }

        if trimmed.starts_with("/agent") || trimmed.starts_with('@') {
            let known: Vec<String> = self
                .subagent_manager
                .as_ref()
                .map(|m| m.definitions().iter().map(|d| d.name.clone()).collect())
                .unwrap_or_default();
            match crate::subagent::AgentCommand::parse(trimmed, &known) {
                Ok(cmd) => {
                    if let Some(msg) = self.handle_agent_command(cmd).await {
                        self.channel.send(&msg).await?;
                    }
                    return Ok(());
                }
                Err(e) if trimmed.starts_with('@') => {
                    // Unknown @token — fall through to normal LLM processing
                    tracing::debug!("@mention not matched as agent: {e}");
                }
                Err(e) => {
                    self.channel.send(&e.to_string()).await?;
                    return Ok(());
                }
            }
        }

        self.check_pending_rollbacks().await;
        // Extract before rebuild_system_prompt so the value is not tainted
        // by the secrets-bearing system prompt (ConversationId is just an i64).
        let conv_id = self.memory_state.conversation_id;
        self.rebuild_system_prompt(&text).await;

        let correction_detection_enabled = self
            .learning_engine
            .config
            .as_ref()
            .is_none_or(|c| c.correction_detection);
        if self.is_learning_enabled() && correction_detection_enabled {
            let previous_user_messages: Vec<&str> = self
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .map(|m| m.content.as_str())
                .collect();
            if let Some(signal) = self
                .feedback_detector
                .detect(trimmed, &previous_user_messages)
            {
                tracing::info!(
                    kind = signal.kind.as_str(),
                    confidence = signal.confidence,
                    "implicit correction detected"
                );
                // REV-PH2-002 + SEC-PH2-002: cap feedback_text to 500 chars (UTF-8 safe)
                let feedback_text = context::truncate_chars(&signal.feedback_text, 500);
                self.record_skill_outcomes(
                    "user_rejection",
                    Some(&feedback_text),
                    Some(signal.kind.as_str()),
                )
                .await;
                if let Some(memory) = &self.memory_state.memory {
                    // Use `trimmed` (raw user input, untainted by secrets) instead of
                    // `feedback_text` (derived from previous_user_messages → self.messages)
                    // to avoid the CodeQL cleartext-logging taint path.
                    let correction_text = context::truncate_chars(trimmed, 500);
                    match memory
                        .sqlite()
                        .store_user_correction(
                            conv_id.map(|c| c.0),
                            "",
                            &correction_text,
                            self.skill_state
                                .active_skill_names
                                .first()
                                .map(String::as_str),
                            signal.kind.as_str(),
                        )
                        .await
                    {
                        Ok(correction_id) => {
                            if let Err(e) = memory
                                .store_correction_embedding(correction_id, &correction_text)
                                .await
                            {
                                tracing::warn!("failed to store correction embedding: {e:#}");
                            }
                        }
                        Err(e) => tracing::warn!("failed to store user correction: {e:#}"),
                    }
                }
            }
        }

        if let Err(e) = self.maybe_compact().await {
            tracing::warn!("context compaction failed: {e:#}");
        }

        if let Err(e) = Box::pin(self.prepare_context(trimmed)).await {
            tracing::warn!("context preparation failed: {e:#}");
        }

        self.learning_engine.reset_reflection();

        let user_msg = if !image_parts.is_empty() && self.provider.supports_vision() {
            let mut parts = vec![zeph_llm::provider::MessagePart::Text { text: text.clone() }];
            parts.extend(image_parts);
            Message::from_parts(Role::User, parts)
        } else {
            if !image_parts.is_empty() {
                tracing::warn!(
                    count = image_parts.len(),
                    "image attachments dropped: provider does not support vision"
                );
            }
            Message {
                role: Role::User,
                content: text.clone(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            }
        };
        self.push_message(user_msg);
        self.persist_message(Role::User, &text).await;

        if let Err(e) = self.process_response().await {
            tracing::error!("Response processing failed: {e:#}");
            let user_msg = format!("Error: {e:#}");
            self.channel.send(&user_msg).await?;
            self.messages.pop();
            self.recompute_prompt_tokens();
            self.channel.flush_chunks().await?;
        }

        Ok(())
    }

    async fn handle_image_command(
        &mut self,
        path: &str,
        extra_parts: &mut Vec<zeph_llm::provider::MessagePart>,
    ) -> Result<(), error::AgentError> {
        use std::path::Component;
        use zeph_llm::provider::{ImageData, MessagePart};

        // Reject paths that traverse outside the current directory.
        let has_parent_dir = std::path::Path::new(path)
            .components()
            .any(|c| c == Component::ParentDir);
        if has_parent_dir {
            self.channel
                .send("Invalid image path: path traversal not allowed")
                .await?;
            return Ok(());
        }

        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                self.channel
                    .send(&format!("Cannot read image {path}: {e}"))
                    .await?;
                return Ok(());
            }
        };
        if data.len() > MAX_IMAGE_BYTES {
            self.channel
                .send(&format!(
                    "Image {path} exceeds size limit ({} MB), skipping",
                    MAX_IMAGE_BYTES / 1024 / 1024
                ))
                .await?;
            return Ok(());
        }
        let mime_type = detect_image_mime(Some(path)).to_string();
        extra_parts.push(MessagePart::Image(Box::new(ImageData { data, mime_type })));
        self.channel
            .send(&format!("Image loaded: {path}. Send your message."))
            .await?;
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
                None,
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

    #[allow(clippy::too_many_lines)]
    async fn handle_agent_command(&mut self, cmd: crate::subagent::AgentCommand) -> Option<String> {
        use crate::subagent::{AgentCommand, SubAgentState};
        use std::fmt::Write as _;

        match cmd {
            AgentCommand::List => {
                let mgr = self.subagent_manager.as_ref()?;
                let defs = mgr.definitions();
                if defs.is_empty() {
                    return Some("No sub-agent definitions found.".into());
                }
                let mut out = String::from("Available sub-agents:\n");
                for d in defs {
                    let _ = writeln!(out, "  {} — {}", d.name, d.description);
                }
                Some(out)
            }
            AgentCommand::Background { name, prompt } => {
                let provider = self.provider.clone();
                let tool_executor = Arc::clone(&self.tool_executor);
                let skills = self.filtered_skills_for(&name);
                let mgr = self.subagent_manager.as_mut()?;
                match mgr.spawn(&name, &prompt, provider, tool_executor, skills) {
                    Ok(id) => Some(format!(
                        "Sub-agent '{name}' started in background (id: {short})",
                        short = &id[..8.min(id.len())]
                    )),
                    Err(e) => Some(format!("Failed to spawn sub-agent: {e}")),
                }
            }
            AgentCommand::Spawn { name, prompt }
            | AgentCommand::Mention {
                agent: name,
                prompt,
            } => {
                // Foreground spawn: launch and await completion, streaming status to user.
                let provider = self.provider.clone();
                let tool_executor = Arc::clone(&self.tool_executor);
                let skills = self.filtered_skills_for(&name);
                let mgr = self.subagent_manager.as_mut()?;
                let task_id = match mgr.spawn(&name, &prompt, provider, tool_executor, skills) {
                    Ok(id) => id,
                    Err(e) => return Some(format!("Failed to spawn sub-agent: {e}")),
                };
                let short = task_id[..8.min(task_id.len())].to_owned();
                let _ = self
                    .channel
                    .send(&format!("Sub-agent '{name}' running... (id: {short})"))
                    .await;
                // Poll until the sub-agent reaches a terminal state.
                let result = loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let mgr = self.subagent_manager.as_ref()?;
                    let statuses = mgr.statuses();
                    let Some((_, status)) = statuses.iter().find(|(id, _)| id == &task_id) else {
                        break "Sub-agent completed (no status available).".to_owned();
                    };
                    match status.state {
                        SubAgentState::Completed => {
                            let msg = status.last_message.clone().unwrap_or_else(|| "done".into());
                            break format!("Sub-agent '{name}' completed: {msg}");
                        }
                        SubAgentState::Failed => {
                            let msg = status
                                .last_message
                                .clone()
                                .unwrap_or_else(|| "unknown error".into());
                            break format!("Sub-agent '{name}' failed: {msg}");
                        }
                        SubAgentState::Canceled => {
                            break format!("Sub-agent '{name}' was cancelled.");
                        }
                        _ => {
                            let _ = self
                                .channel
                                .send_status(&format!(
                                    "sub-agent '{name}': turn {}/{}",
                                    status.turns_used,
                                    self.subagent_manager
                                        .as_ref()
                                        .and_then(|m| m.agents_def(&task_id))
                                        .map_or(20, |d| d.permissions.max_turns)
                                ))
                                .await;
                        }
                    }
                };
                Some(result)
            }
            AgentCommand::Status => {
                let mgr = self.subagent_manager.as_ref()?;
                let statuses = mgr.statuses();
                if statuses.is_empty() {
                    return Some("No active sub-agents.".into());
                }
                let mut out = String::from("Active sub-agents:\n");
                for (id, s) in &statuses {
                    let state = format!("{:?}", s.state).to_lowercase();
                    let elapsed = s.started_at.elapsed().as_secs();
                    let _ = writeln!(
                        out,
                        "  [{short}] {state}  turns={t}  elapsed={elapsed}s  {msg}",
                        short = &id[..8.min(id.len())],
                        t = s.turns_used,
                        msg = s.last_message.as_deref().unwrap_or(""),
                    );
                }
                Some(out)
            }
            AgentCommand::Cancel { id } => {
                let mgr = self.subagent_manager.as_mut()?;
                // Accept prefix match on task_id.
                let ids: Vec<String> = mgr
                    .statuses()
                    .into_iter()
                    .map(|(task_id, _)| task_id)
                    .filter(|task_id| task_id.starts_with(&id))
                    .collect();
                match ids.as_slice() {
                    [] => Some(format!("No sub-agent with id prefix '{id}'")),
                    [full_id] => {
                        let full_id = full_id.clone();
                        match mgr.cancel(&full_id) {
                            Ok(()) => Some(format!("Cancelled sub-agent {full_id}.")),
                            Err(e) => Some(format!("Cancel failed: {e}")),
                        }
                    }
                    _ => Some(format!(
                        "Ambiguous id prefix '{id}': matches {} agents",
                        ids.len()
                    )),
                }
            }
            AgentCommand::Approve { id } => {
                // Look up pending secret request for the given task_id prefix.
                let mgr = self.subagent_manager.as_mut()?;
                let full_ids: Vec<String> = mgr
                    .statuses()
                    .into_iter()
                    .map(|(tid, _)| tid)
                    .filter(|tid| tid.starts_with(&id))
                    .collect();
                let full_id = match full_ids.as_slice() {
                    [] => return Some(format!("No sub-agent with id prefix '{id}'")),
                    [fid] => fid.clone(),
                    _ => {
                        return Some(format!(
                            "Ambiguous id prefix '{id}': matches {} agents",
                            full_ids.len()
                        ));
                    }
                };
                if let Some((tid, req)) = mgr.try_recv_secret_request()
                    && tid == full_id
                {
                    let key = req.secret_key.clone();
                    let ttl = std::time::Duration::from_secs(300);
                    if let Err(e) = mgr.approve_secret(&full_id, &key, ttl) {
                        return Some(format!("Approve failed: {e}"));
                    }
                    if let Err(e) = mgr.deliver_secret(&full_id, key.clone()) {
                        return Some(format!("Secret delivery failed: {e}"));
                    }
                    return Some(format!("Secret '{key}' approved for sub-agent {full_id}."));
                }
                Some(format!(
                    "No pending secret request for sub-agent '{full_id}'."
                ))
            }
            AgentCommand::Deny { id } => {
                let mgr = self.subagent_manager.as_mut()?;
                let full_ids: Vec<String> = mgr
                    .statuses()
                    .into_iter()
                    .map(|(tid, _)| tid)
                    .filter(|tid| tid.starts_with(&id))
                    .collect();
                let full_id = match full_ids.as_slice() {
                    [] => return Some(format!("No sub-agent with id prefix '{id}'")),
                    [fid] => fid.clone(),
                    _ => {
                        return Some(format!(
                            "Ambiguous id prefix '{id}': matches {} agents",
                            full_ids.len()
                        ));
                    }
                };
                match mgr.deny_secret(&full_id) {
                    Ok(()) => Some(format!("Secret request denied for sub-agent '{full_id}'.")),
                    Err(e) => Some(format!("Deny failed: {e}")),
                }
            }
        }
    }

    fn filtered_skills_for(&self, agent_name: &str) -> Option<Vec<String>> {
        let mgr = self.subagent_manager.as_ref()?;
        let def = mgr.definitions().iter().find(|d| d.name == agent_name)?;
        match crate::subagent::filter_skills(&self.skill_state.registry, &def.skills) {
            Ok(skills) => {
                let bodies: Vec<String> = skills.into_iter().map(|s| s.body.clone()).collect();
                if bodies.is_empty() {
                    None
                } else {
                    Some(bodies)
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "skill filtering failed for sub-agent");
                None
            }
        }
    }

    async fn reload_skills(&mut self) {
        let new_registry = SkillRegistry::load(&self.skill_state.skill_paths);
        if new_registry.fingerprint() == self.skill_state.registry.fingerprint() {
            return;
        }
        let _ = self.channel.send_status("reloading skills...").await;
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
        } else if let Some(ref mut backend) = self.skill_state.matcher {
            let _ = self.channel.send_status("syncing skill index...").await;
            if let Err(e) = backend
                .sync(&all_meta, &self.skill_state.embedding_model, embed_fn)
                .await
            {
                tracing::warn!("failed to sync skill embeddings: {e:#}");
            }
        }

        if self.skill_state.hybrid_search {
            let descs: Vec<&str> = all_meta.iter().map(|m| m.description.as_str()).collect();
            let _ = self.channel.send_status("rebuilding search index...").await;
            self.skill_state.bm25_index = Some(zeph_skills::bm25::Bm25Index::build(&descs));
        }

        let all_skills: Vec<Skill> = self
            .skill_state
            .registry
            .all_meta()
            .iter()
            .filter_map(|m| self.skill_state.registry.get_skill(&m.name).ok())
            .collect();
        let trust_map = self.build_skill_trust_map().await;
        let empty_health: HashMap<String, (f64, u32)> = HashMap::new();
        let skills_prompt = format_skills_prompt(&all_skills, &trust_map, &empty_health);
        self.skill_state
            .last_skills_prompt
            .clone_from(&skills_prompt);
        let system_prompt = build_system_prompt(&skills_prompt, None, None, false);
        if let Some(msg) = self.messages.first_mut() {
            msg.content = system_prompt;
        }

        let _ = self.channel.send_status("").await;
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
        self.runtime.redact_credentials = config.memory.redact_credentials;
        self.memory_state.history_limit = config.memory.history_limit;
        self.memory_state.recall_limit = config.memory.semantic.recall_limit;
        self.memory_state.summarization_threshold = config.memory.summarization_threshold;
        self.skill_state.max_active_skills = config.skills.max_active_skills;
        self.skill_state.disambiguation_threshold = config.skills.disambiguation_threshold;
        self.skill_state.cosine_weight = config.skills.cosine_weight.clamp(0.0, 1.0);
        self.skill_state.hybrid_search = config.skills.hybrid_search;

        if config.memory.context_budget_tokens > 0 {
            self.context_manager.budget = Some(ContextBudget::new(
                config.memory.context_budget_tokens,
                0.20,
            ));
        } else {
            self.context_manager.budget = None;
        }
        self.context_manager.compaction_threshold = config.memory.compaction_threshold;
        self.context_manager.compaction_preserve_tail = config.memory.compaction_preserve_tail;
        self.context_manager.prune_protect_tokens = config.memory.prune_protect_tokens;
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
pub(crate) async fn shutdown_signal(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub(crate) async fn recv_optional<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(inner) => {
            if let Some(v) = inner.recv().await {
                Some(v)
            } else {
                *rx = None;
                std::future::pending().await
            }
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
pub(super) mod agent_tests {
    use super::message_queue::{MAX_AUDIO_BYTES, MAX_IMAGE_BYTES, detect_image_mime};
    #[allow(unused_imports)]
    pub(crate) use super::{
        Agent, CODE_CONTEXT_PREFIX, CROSS_SESSION_PREFIX, DOOM_LOOP_WINDOW, RECALL_PREFIX,
        SUMMARY_PREFIX, TOOL_OUTPUT_SUFFIX, format_tool_output, recv_optional, shutdown_signal,
    };
    pub(crate) use crate::channel::Channel;
    use crate::channel::{Attachment, AttachmentKind, ChannelMessage};
    pub(crate) use crate::config::{SecurityConfig, TimeoutConfig};
    pub(crate) use crate::metrics::MetricsSnapshot;
    use std::sync::{Arc, Mutex};
    pub(crate) use tokio::sync::{Notify, mpsc, watch};
    pub(crate) use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    pub(crate) use zeph_llm::provider::{Message, MessageMetadata, Role};
    pub(crate) use zeph_memory::semantic::SemanticMemory;
    pub(crate) use zeph_skills::registry::SkillRegistry;
    pub(crate) use zeph_skills::watcher::SkillEvent;
    pub(crate) use zeph_tools::executor::ToolExecutor;
    use zeph_tools::executor::{ToolError, ToolOutput};

    pub(crate) fn mock_provider(responses: Vec<String>) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(responses))
    }

    pub(crate) fn mock_provider_streaming(responses: Vec<String>) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(responses).with_streaming())
    }

    pub(crate) fn mock_provider_failing() -> AnyProvider {
        AnyProvider::Mock(MockProvider::failing())
    }

    pub(crate) struct MockChannel {
        messages: Arc<Mutex<Vec<String>>>,
        sent: Arc<Mutex<Vec<String>>>,
        chunks: Arc<Mutex<Vec<String>>>,
        confirmations: Arc<Mutex<Vec<bool>>>,
        pub(crate) statuses: Arc<Mutex<Vec<String>>>,
    }

    impl MockChannel {
        pub(crate) fn new(messages: Vec<String>) -> Self {
            Self {
                messages: Arc::new(Mutex::new(messages)),
                sent: Arc::new(Mutex::new(Vec::new())),
                chunks: Arc::new(Mutex::new(Vec::new())),
                confirmations: Arc::new(Mutex::new(Vec::new())),
                statuses: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_confirmations(mut self, confirmations: Vec<bool>) -> Self {
            self.confirmations = Arc::new(Mutex::new(confirmations));
            self
        }

        pub(crate) fn sent_messages(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }

        pub(crate) fn sent_chunks(&self) -> Vec<String> {
            self.chunks.lock().unwrap().clone()
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
                    attachments: vec![],
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
                    attachments: vec![],
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

        async fn send_status(&mut self, text: &str) -> Result<(), crate::channel::ChannelError> {
            self.statuses.lock().unwrap().push(text.to_string());
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

    pub(crate) struct MockToolExecutor {
        outputs: Arc<Mutex<Vec<Result<Option<ToolOutput>, ToolError>>>>,
        pub(crate) captured_env: Arc<Mutex<Vec<Option<std::collections::HashMap<String, String>>>>>,
    }

    impl MockToolExecutor {
        pub(crate) fn new(outputs: Vec<Result<Option<ToolOutput>, ToolError>>) -> Self {
            Self {
                outputs: Arc::new(Mutex::new(outputs)),
                captured_env: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub(crate) fn no_tools() -> Self {
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

        fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
            self.captured_env.lock().unwrap().push(env);
        }
    }

    pub(crate) fn create_test_registry() -> SkillRegistry {
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
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
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
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
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
            filter_stats: None,
            diff: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
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
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
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
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
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
            metadata: MessageMetadata::default(),
        });
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "cmd".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.messages.push(Message {
            role: Role::User,
            content: "[tool output: bash]\nsome output".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
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
    async fn test_overflow_notice_contains_filename() {
        let dir = tempfile::tempdir().unwrap();
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_tool_summarization(false)
            .with_overflow_config(zeph_tools::OverflowConfig {
                threshold: 100,
                retention_days: 7,
                dir: Some(dir.path().to_path_buf()),
            });

        let long = "x".repeat(zeph_tools::MAX_TOOL_OUTPUT_CHARS + 1000);
        let result = agent.maybe_summarize_tool_output(&long).await;
        assert!(result.contains("full output saved to"));
        // Notice must contain only filename (UUID.txt), not a full path
        let notice_start = result.find("full output saved to").unwrap();
        let notice_part = &result[notice_start..];
        assert!(notice_part.contains(".txt"));
        assert!(!notice_part.contains('/'));
    }

    #[tokio::test]
    async fn test_maybe_summarize_long_output_disabled_truncates() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_tool_summarization(false)
            .with_overflow_config(zeph_tools::OverflowConfig {
                threshold: 1000,
                retention_days: 7,
                dir: None,
            });

        // Must exceed both overflow threshold (1000) and MAX_TOOL_OUTPUT_CHARS (30_000)
        // so that truncate_tool_output produces the "truncated" marker.
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
            .with_tool_summarization(true)
            .with_overflow_config(zeph_tools::OverflowConfig {
                threshold: 1000,
                retention_days: 7,
                dir: None,
            });

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
            .with_tool_summarization(true)
            .with_overflow_config(zeph_tools::OverflowConfig {
                threshold: 1000,
                retention_days: 7,
                dir: None,
            });

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
        assert!(agent.tool_orchestrator.summarize_tool_output_enabled);

        let provider2 = mock_provider(vec![]);
        let channel2 = MockChannel::new(vec![]);
        let registry2 = create_test_registry();
        let executor2 = MockToolExecutor::no_tools();

        let agent2 = Agent::new(provider2, channel2, registry2, None, 5, executor2)
            .with_tool_summarization(false);
        assert!(!agent2.tool_orchestrator.summarize_tool_output_enabled);
    }

    #[test]
    fn doom_loop_detection_triggers_on_identical_outputs() {
        // doom_loop_history stores u64 hashes — identical content produces equal hashes
        let h = 42u64;
        let history: Vec<u64> = vec![h, h, h];
        let recent = &history[history.len() - DOOM_LOOP_WINDOW..];
        assert!(recent.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn doom_loop_detection_no_trigger_on_different_outputs() {
        let history: Vec<u64> = vec![1, 2, 3];
        let recent = &history[history.len() - DOOM_LOOP_WINDOW..];
        assert!(!recent.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn format_tool_output_structure() {
        let out = format_tool_output("bash", "hello world");
        assert!(out.starts_with("[tool output: bash]\n```\n"));
        assert!(out.ends_with(TOOL_OUTPUT_SUFFIX));
        assert!(out.contains("hello world"));
    }

    #[test]
    fn format_tool_output_empty_body() {
        let out = format_tool_output("grep", "");
        assert_eq!(out, "[tool output: grep]\n```\n\n```");
    }

    #[tokio::test]
    async fn cancel_signal_propagates_to_fresh_token() {
        use tokio_util::sync::CancellationToken;
        let signal = Arc::new(Notify::new());

        let token = CancellationToken::new();
        let sig = Arc::clone(&signal);
        let tok = token.clone();
        tokio::spawn(async move {
            sig.notified().await;
            tok.cancel();
        });

        // Yield to let the spawned task reach notified().await
        tokio::task::yield_now().await;
        assert!(!token.is_cancelled());
        signal.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_signal_works_across_multiple_messages() {
        use tokio_util::sync::CancellationToken;
        let signal = Arc::new(Notify::new());

        // First "message"
        let token1 = CancellationToken::new();
        let sig1 = Arc::clone(&signal);
        let tok1 = token1.clone();
        tokio::spawn(async move {
            sig1.notified().await;
            tok1.cancel();
        });

        tokio::task::yield_now().await;
        signal.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(token1.is_cancelled());

        // Second "message" — same signal, new token
        let token2 = CancellationToken::new();
        let sig2 = Arc::clone(&signal);
        let tok2 = token2.clone();
        tokio::spawn(async move {
            sig2.notified().await;
            tok2.cancel();
        });

        tokio::task::yield_now().await;
        assert!(!token2.is_cancelled());
        signal.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(token2.is_cancelled());
    }

    mod resolve_message_tests {
        use super::*;
        use crate::channel::{Attachment, AttachmentKind, ChannelMessage};
        use std::future::Future;
        use std::pin::Pin;
        use zeph_llm::error::LlmError;
        use zeph_llm::stt::{SpeechToText, Transcription};

        struct MockStt {
            text: Option<String>,
        }

        impl MockStt {
            fn ok(text: &str) -> Self {
                Self {
                    text: Some(text.to_string()),
                }
            }

            fn failing() -> Self {
                Self { text: None }
            }
        }

        impl SpeechToText for MockStt {
            fn transcribe(
                &self,
                _audio: &[u8],
                _filename: Option<&str>,
            ) -> Pin<Box<dyn Future<Output = Result<Transcription, LlmError>> + Send + '_>>
            {
                let result = match &self.text {
                    Some(t) => Ok(Transcription {
                        text: t.clone(),
                        language: None,
                        duration_secs: None,
                    }),
                    None => Err(LlmError::TranscriptionFailed("mock error".into())),
                };
                Box::pin(async move { result })
            }
        }

        fn make_agent(stt: Option<Box<dyn SpeechToText>>) -> Agent<MockChannel> {
            let provider = mock_provider(vec!["ok".into()]);
            let empty: Vec<String> = vec![];
            let registry = zeph_skills::registry::SkillRegistry::load(&empty);
            let channel = MockChannel::new(vec![]);
            let executor = MockToolExecutor::no_tools();
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.stt = stt;
            agent
        }

        fn audio_attachment(data: &[u8]) -> Attachment {
            Attachment {
                kind: AttachmentKind::Audio,
                data: data.to_vec(),
                filename: Some("test.wav".into()),
            }
        }

        #[tokio::test]
        async fn no_audio_attachments_returns_text() {
            let agent = make_agent(None);
            let msg = ChannelMessage {
                text: "hello".into(),
                attachments: vec![],
            };
            assert_eq!(agent.resolve_message(msg).await.0, "hello");
        }

        #[tokio::test]
        async fn audio_without_stt_returns_original_text() {
            let agent = make_agent(None);
            let msg = ChannelMessage {
                text: "hello".into(),
                attachments: vec![audio_attachment(b"audio-data")],
            };
            assert_eq!(agent.resolve_message(msg).await.0, "hello");
        }

        #[tokio::test]
        async fn audio_with_stt_prepends_transcription() {
            let agent = make_agent(Some(Box::new(MockStt::ok("transcribed text"))));
            let msg = ChannelMessage {
                text: "original".into(),
                attachments: vec![audio_attachment(b"audio-data")],
            };
            let (result, _) = agent.resolve_message(msg).await;
            assert!(result.contains("[transcribed audio]"));
            assert!(result.contains("transcribed text"));
            assert!(result.contains("original"));
        }

        #[tokio::test]
        async fn audio_with_stt_no_original_text() {
            let agent = make_agent(Some(Box::new(MockStt::ok("transcribed text"))));
            let msg = ChannelMessage {
                text: String::new(),
                attachments: vec![audio_attachment(b"audio-data")],
            };
            let (result, _) = agent.resolve_message(msg).await;
            assert_eq!(result, "transcribed text");
        }

        #[tokio::test]
        async fn all_transcriptions_fail_returns_original() {
            let agent = make_agent(Some(Box::new(MockStt::failing())));
            let msg = ChannelMessage {
                text: "original".into(),
                attachments: vec![audio_attachment(b"audio-data")],
            };
            assert_eq!(agent.resolve_message(msg).await.0, "original");
        }

        #[tokio::test]
        async fn multiple_audio_attachments_joined() {
            let agent = make_agent(Some(Box::new(MockStt::ok("chunk"))));
            let msg = ChannelMessage {
                text: String::new(),
                attachments: vec![
                    audio_attachment(b"a1"),
                    audio_attachment(b"a2"),
                    audio_attachment(b"a3"),
                ],
            };
            let (result, _) = agent.resolve_message(msg).await;
            assert_eq!(result, "chunk\nchunk\nchunk");
        }

        #[tokio::test]
        async fn oversized_audio_skipped() {
            let agent = make_agent(Some(Box::new(MockStt::ok("should not appear"))));
            let big = vec![0u8; MAX_AUDIO_BYTES + 1];
            let msg = ChannelMessage {
                text: "original".into(),
                attachments: vec![Attachment {
                    kind: AttachmentKind::Audio,
                    data: big,
                    filename: None,
                }],
            };
            assert_eq!(agent.resolve_message(msg).await.0, "original");
        }
    }

    #[test]
    fn detect_image_mime_jpeg() {
        assert_eq!(detect_image_mime(Some("photo.jpg")), "image/jpeg");
        assert_eq!(detect_image_mime(Some("photo.jpeg")), "image/jpeg");
    }

    #[test]
    fn detect_image_mime_gif() {
        assert_eq!(detect_image_mime(Some("anim.gif")), "image/gif");
    }

    #[test]
    fn detect_image_mime_webp() {
        assert_eq!(detect_image_mime(Some("img.webp")), "image/webp");
    }

    #[test]
    fn detect_image_mime_unknown_defaults_png() {
        assert_eq!(detect_image_mime(Some("file.bmp")), "image/png");
        assert_eq!(detect_image_mime(None), "image/png");
    }

    #[tokio::test]
    async fn resolve_message_extracts_image_attachment() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let msg = ChannelMessage {
            text: "look at this".into(),
            attachments: vec![Attachment {
                kind: AttachmentKind::Image,
                data: vec![0u8; 16],
                filename: Some("test.jpg".into()),
            }],
        };
        let (text, parts) = agent.resolve_message(msg).await;
        assert_eq!(text, "look at this");
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            zeph_llm::provider::MessagePart::Image(img) => {
                assert_eq!(img.mime_type, "image/jpeg");
                assert_eq!(img.data.len(), 16);
            }
            _ => panic!("expected Image part"),
        }
    }

    #[tokio::test]
    async fn resolve_message_drops_oversized_image() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let msg = ChannelMessage {
            text: "big image".into(),
            attachments: vec![Attachment {
                kind: AttachmentKind::Image,
                data: vec![0u8; MAX_IMAGE_BYTES + 1],
                filename: Some("huge.png".into()),
            }],
        };
        let (text, parts) = agent.resolve_message(msg).await;
        assert_eq!(text, "big image");
        assert!(parts.is_empty());
    }

    #[tokio::test]
    async fn handle_image_command_rejects_path_traversal() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut parts = Vec::new();
        let result = agent
            .handle_image_command("../../etc/passwd", &mut parts)
            .await;
        assert!(result.is_ok());
        assert!(parts.is_empty());
        // Channel should have received an error message
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|m| m.contains("traversal")));
    }

    #[tokio::test]
    async fn handle_image_command_missing_file_sends_error() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut parts = Vec::new();
        let result = agent
            .handle_image_command("/nonexistent/image.png", &mut parts)
            .await;
        assert!(result.is_ok());
        assert!(parts.is_empty());
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|m| m.contains("Cannot read image")));
    }

    #[tokio::test]
    async fn handle_image_command_loads_valid_file() {
        use std::io::Write;
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Write a small temp image
        let mut tmp = tempfile::NamedTempFile::with_suffix(".jpg").unwrap();
        let data = vec![0xFFu8, 0xD8, 0xFF, 0xE0];
        tmp.write_all(&data).unwrap();
        let path = tmp.path().to_str().unwrap().to_owned();

        let mut parts = Vec::new();
        let result = agent.handle_image_command(&path, &mut parts).await;
        assert!(result.is_ok());
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            zeph_llm::provider::MessagePart::Image(img) => {
                assert_eq!(img.data, data);
                assert_eq!(img.mime_type, "image/jpeg");
            }
            _ => panic!("expected Image part"),
        }
        let sent = agent.channel.sent_messages();
        assert!(sent.iter().any(|m| m.contains("Image loaded")));
    }

    // ── handle_agent_command tests ────────────────────────────────────────────

    use crate::subagent::AgentCommand;

    fn make_agent_with_manager() -> Agent<MockChannel> {
        use crate::subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use crate::subagent::{SubAgentDef, SubAgentManager};

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "helper".into(),
            description: "A helper bot".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: "You are helpful.".into(),
        });
        agent.subagent_manager = Some(mgr);
        agent
    }

    #[tokio::test]
    async fn agent_command_no_manager_returns_none() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // no subagent_manager set — List needs manager to return Some
        assert!(
            agent
                .handle_agent_command(AgentCommand::List)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn agent_command_list_returns_definitions() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::List)
            .await
            .unwrap();
        assert!(resp.contains("helper"));
        assert!(resp.contains("A helper bot"));
    }

    #[tokio::test]
    async fn agent_command_spawn_unknown_name_returns_error() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "unknown-bot".into(),
                prompt: "do something".into(),
            })
            .await
            .unwrap();
        assert!(resp.contains("Failed to spawn"));
    }

    #[tokio::test]
    async fn agent_command_spawn_known_name_returns_started() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "helper".into(),
                prompt: "do some work".into(),
            })
            .await
            .unwrap();
        assert!(resp.contains("helper"));
        assert!(resp.contains("started"));
    }

    #[tokio::test]
    async fn agent_command_status_no_agents_returns_empty_message() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::Status)
            .await
            .unwrap();
        assert!(resp.contains("No active sub-agents"));
    }

    #[tokio::test]
    async fn agent_command_cancel_unknown_id_returns_not_found() {
        let mut agent = make_agent_with_manager();
        let resp = agent
            .handle_agent_command(AgentCommand::Cancel {
                id: "deadbeef".into(),
            })
            .await
            .unwrap();
        assert!(resp.contains("No sub-agent"));
    }

    #[tokio::test]
    async fn agent_command_cancel_valid_id_succeeds() {
        let mut agent = make_agent_with_manager();
        // spawn first so we have a task to cancel
        let spawn_resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "helper".into(),
                prompt: "cancel this".into(),
            })
            .await
            .unwrap();
        // extract short id from "started in background (id: XXXXXXXX)"
        let short_id = spawn_resp
            .split("id: ")
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .trim()
            .to_string();
        let resp = agent
            .handle_agent_command(AgentCommand::Cancel { id: short_id })
            .await
            .unwrap();
        assert!(resp.contains("Cancelled"));
    }

    #[tokio::test]
    async fn agent_command_approve_no_pending_request() {
        let mut agent = make_agent_with_manager();
        // Spawn an agent first so there's an active agent to reference
        let spawn_resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "helper".into(),
                prompt: "do work".into(),
            })
            .await
            .unwrap();
        let short_id = spawn_resp
            .split("id: ")
            .nth(1)
            .unwrap()
            .trim_end_matches(')')
            .trim()
            .to_string();
        let resp = agent
            .handle_agent_command(AgentCommand::Approve { id: short_id })
            .await
            .unwrap();
        assert!(resp.contains("No pending secret request"));
    }

    #[test]
    fn set_model_updates_model_name() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        assert!(agent.set_model("claude-opus-4-6").is_ok());
        assert_eq!(agent.runtime.model_name, "claude-opus-4-6");
    }

    #[test]
    fn set_model_overwrites_previous_value() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.set_model("model-a").unwrap();
        agent.set_model("model-b").unwrap();
        assert_eq!(agent.runtime.model_name, "model-b");
    }

    #[tokio::test]
    async fn model_command_switch_sends_confirmation() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.handle_model_command("/model my-new-model").await;
        let messages = sent.lock().unwrap();
        assert!(
            messages.iter().any(|m| m.contains("my-new-model")),
            "expected switch confirmation, got: {messages:?}"
        );
    }

    #[tokio::test]
    async fn model_command_list_no_cache_fetches_remote() {
        // With mock provider, list_models_remote returns empty vec — agent sends "No models".
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // Ensure cache is stale for mock provider slug
        zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();
        agent.handle_model_command("/model").await;
        let messages = sent.lock().unwrap();
        // Mock returns empty list → "No models available."
        assert!(
            messages.iter().any(|m| m.contains("No models")),
            "expected empty model list message, got: {messages:?}"
        );
    }

    #[tokio::test]
    async fn model_command_refresh_sends_result() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.handle_model_command("/model refresh").await;
        let messages = sent.lock().unwrap();
        assert!(
            messages.iter().any(|m| m.contains("Fetched")),
            "expected fetch confirmation, got: {messages:?}"
        );
    }
}

/// End-to-end tests for M30 resilient compaction: error detection → compact → retry → success.
#[cfg(test)]
mod compaction_e2e {
    use super::agent_tests::*;
    use zeph_llm::LlmError;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    /// Verify that the agent recovers from a `ContextLengthExceeded` error during an LLM call,
    /// compacts its context, and returns a successful response on the next attempt.
    #[tokio::test]
    async fn agent_recovers_from_context_length_exceeded_and_produces_response() {
        // Provider: first call raises ContextLengthExceeded, second call succeeds.
        let provider = AnyProvider::Mock(
            MockProvider::with_responses(vec!["final answer".into()])
                .with_errors(vec![LlmError::ContextLengthExceeded]),
        );
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = super::Agent::new(provider, channel, registry, None, 5, executor)
            // Provide a context budget so compact_context has a compaction target
            .with_context_budget(200_000, 0.20, 0.80, 4, 0);

        // Seed a user message so the agent has something to compact/retry
        agent.messages.push(Message {
            role: Role::User,
            content: "describe the architecture".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        // call_llm_with_retry is the direct entry point for the retry/compact flow
        let result = agent.call_llm_with_retry(2).await.unwrap();

        assert!(
            result.is_some(),
            "agent must produce a response after recovering from context length error"
        );
        assert_eq!(result.as_deref(), Some("final answer"));

        // Verify the channel received the recovered response
        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|m| m.contains("final answer")),
            "recovered response must be forwarded to the channel; got: {sent:?}"
        );
    }
}
