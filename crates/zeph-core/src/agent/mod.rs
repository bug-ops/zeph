// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod builder;
mod context;
pub(crate) mod context_manager;
pub mod error;
#[cfg(feature = "experiments")]
mod experiment_cmd;
pub(super) mod feedback_detector;
mod graph_commands;
#[cfg(feature = "index")]
mod index;
mod learning;
pub(crate) mod learning_engine;
mod log_commands;
#[cfg(feature = "lsp-context")]
mod lsp_commands;
mod mcp;
mod message_queue;
mod persistence;
mod skill_management;
pub mod slash_commands;
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
use crate::context::{
    ContextBudget, EnvironmentContext, build_system_prompt, build_system_prompt_with_instructions,
};
use crate::cost::CostTracker;
use crate::instructions::{InstructionBlock, InstructionEvent, InstructionReloadState};
use crate::sanitizer::ContentSanitizer;
use crate::sanitizer::quarantine::QuarantinedSummarizer;
use crate::vault::Secret;

use message_queue::{MAX_AUDIO_BYTES, MAX_IMAGE_BYTES, QueuedMessage, detect_image_mime};

pub(crate) const DOOM_LOOP_WINDOW: usize = 3;
pub(crate) const DOCUMENT_RAG_PREFIX: &str = "## Relevant documents\n";
pub(crate) const RECALL_PREFIX: &str = "[semantic recall]\n";
pub(crate) const CODE_CONTEXT_PREFIX: &str = "[code context]\n";
pub(crate) const SUMMARY_PREFIX: &str = "[conversation summaries]\n";
pub(crate) const CROSS_SESSION_PREFIX: &str = "[cross-session context]\n";
pub(crate) const CORRECTIONS_PREFIX: &str = "[past corrections]\n";
pub(crate) const GRAPH_FACTS_PREFIX: &str = "[known facts]\n";
/// Prefix used for LSP context messages (`Role::System`) injected into message history.
/// The tool-pair summarizer targets User/Assistant pairs and skips System messages,
/// so these notes are never accidentally summarized. `remove_lsp_messages` uses this
/// prefix to clear stale notes before each fresh injection.
#[cfg(feature = "lsp-context")]
pub(crate) const LSP_NOTE_PREFIX: &str = "[lsp ";
pub(crate) const TOOL_OUTPUT_SUFFIX: &str = "\n```";

fn format_plan_summary(graph: &crate::orchestration::TaskGraph) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "Plan: \"{}\"", graph.goal);
    let _ = writeln!(out, "Tasks: {}", graph.tasks.len());
    let _ = writeln!(out);
    for (i, task) in graph.tasks.iter().enumerate() {
        let deps = if task.depends_on.is_empty() {
            String::new()
        } else {
            let ids: Vec<String> = task.depends_on.iter().map(ToString::to_string).collect();
            format!(" (after: {})", ids.join(", "))
        };
        let agent = task.agent_hint.as_deref().unwrap_or("-");
        let _ = writeln!(out, "  {}. [{}] {}{}", i + 1, agent, task.title, deps);
    }
    out
}

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
    pub(super) graph_config: crate::config::GraphConfig,
}

pub(super) struct SkillState {
    pub(super) registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
    pub(super) skill_paths: Vec<PathBuf>,
    pub(super) managed_dir: Option<PathBuf>,
    pub(super) trust_config: crate::config::TrustConfig,
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
    pub(super) judge_detector: Option<feedback_detector::JudgeDetector>,
    pub(super) judge_provider: Option<AnyProvider>,
    config_path: Option<PathBuf>,
    pub(super) logging_config: crate::config::LoggingConfig,
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
    custom_task_rx: Option<mpsc::Receiver<String>>,
    /// Manages spawned sub-agents. Wired up during construction but not yet
    /// dispatched to in the current agent loop iteration; retained for
    /// forward-compatible multi-agent orchestration.
    pub(crate) subagent_manager: Option<crate::subagent::SubAgentManager>,
    pub(crate) subagent_config: crate::config::SubAgentConfig,
    pub(crate) orchestration_config: crate::config::OrchestrationConfig,
    #[cfg(feature = "experiments")]
    pub(super) experiment_config: crate::config::ExperimentConfig,
    pub(super) response_cache: Option<std::sync::Arc<zeph_memory::ResponseCache>>,
    /// Parent tool call ID when this agent runs as a subagent inside another agent session.
    /// Propagated into every `LoopbackEvent::ToolStart` / `ToolOutput` so the IDE can build
    /// a subagent hierarchy.
    pub(crate) parent_tool_use_id: Option<String>,
    pub(super) anomaly_detector: Option<zeph_tools::AnomalyDetector>,
    /// Instruction blocks loaded at startup from provider-specific and explicit files.
    pub(super) instruction_blocks: Vec<InstructionBlock>,
    pub(super) instruction_reload_rx: Option<mpsc::Receiver<InstructionEvent>>,
    pub(super) instruction_reload_state: Option<InstructionReloadState>,
    /// Sanitizes untrusted content before it enters the LLM message history.
    pub(super) sanitizer: ContentSanitizer,
    /// Optional quarantine summarizer for routing high-risk content through an isolated LLM.
    pub(super) quarantine_summarizer: Option<QuarantinedSummarizer>,
    /// Guards LLM output and tool calls against data exfiltration.
    pub(super) exfiltration_guard: crate::sanitizer::exfiltration::ExfiltrationGuard,
    /// URLs extracted from untrusted tool outputs that had injection flags.
    /// Cleared at the start of each `process_response` call (per-turn strategy — see S3).
    pub(super) flagged_urls: std::collections::HashSet<String>,
    /// Image parts staged by `/image` commands, attached to the next user message.
    pending_image_parts: Vec<zeph_llm::provider::MessagePart>,
    /// Graph waiting for `/plan confirm` before execution starts.
    pub(super) pending_graph: Option<crate::orchestration::TaskGraph>,
    /// Active debug dumper. When `Some`, every LLM request/response and raw tool output
    /// is written to files in the dump directory. Enabled via `--debug-dump` CLI flag or
    /// `[debug]` config section.
    pub(super) debug_dumper: Option<crate::debug_dump::DebugDumper>,
    /// Format used when creating a dumper via the `/debug-dump` slash command.
    pub(super) dump_format: crate::debug_dump::DumpFormat,
    /// LSP context injection hooks. Fires after native tool execution, injects
    /// diagnostics/hover notes as `Role::System` messages before the next LLM call.
    #[cfg(feature = "lsp-context")]
    pub(super) lsp_hooks: Option<crate::lsp_hooks::LspHookRunner>,
    /// Cancellation token for a running experiment session. `Some` means an experiment is active.
    #[cfg(feature = "experiments")]
    pub(super) experiment_cancel: Option<tokio_util::sync::CancellationToken>,
    /// Pre-built config snapshot used as the experiment baseline (agent path).
    /// Set via `with_experiment_baseline()`; defaults to `ConfigSnapshot::default()`.
    #[cfg(feature = "experiments")]
    pub(super) experiment_baseline: crate::experiments::ConfigSnapshot,
    /// Receives completion/error messages from the background experiment engine task.
    /// When a message arrives in the agent loop, it is forwarded to the channel and
    /// `experiment_cancel` is cleared. Always present so the select! branch compiles
    /// unconditionally; only ever receives messages when the `experiments` feature is enabled.
    pub(super) experiment_notify_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    /// Sender end paired with `experiment_notify_rx`. Cloned into the background task.
    /// Feature-gated because it is only used in `experiment_cmd.rs`.
    #[cfg(feature = "experiments")]
    pub(super) experiment_notify_tx: tokio::sync::mpsc::Sender<String>,
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
        let registry = std::sync::Arc::new(std::sync::RwLock::new(registry));
        Self::new_with_registry_arc(
            provider,
            channel,
            registry,
            matcher,
            max_active_skills,
            tool_executor,
        )
    }

    /// Create an agent from a pre-wrapped registry Arc, allowing the caller to
    /// share the same Arc with other components (e.g. [`crate::SkillLoaderExecutor`]).
    ///
    /// # Panics
    ///
    /// Panics if the registry `RwLock` is poisoned.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn new_with_registry_arc(
        provider: AnyProvider,
        channel: C,
        registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
        matcher: Option<SkillMatcherBackend>,
        max_active_skills: usize,
        tool_executor: impl ToolExecutor + 'static,
    ) -> Self {
        let all_skills: Vec<Skill> = {
            let reg = registry.read().expect("registry read lock poisoned");
            reg.all_meta()
                .iter()
                .filter_map(|m| reg.get_skill(&m.name).ok())
                .collect()
        };
        let empty_trust = HashMap::new();
        let empty_health: HashMap<String, (f64, u32)> = HashMap::new();
        let skills_prompt = format_skills_prompt(&all_skills, &empty_trust, &empty_health);
        let system_prompt = build_system_prompt(&skills_prompt, None, None, false);
        tracing::debug!(len = system_prompt.len(), "initial system prompt built");
        tracing::trace!(prompt = %system_prompt, "full system prompt");

        let initial_prompt_tokens = u64::try_from(system_prompt.len()).unwrap_or(0) / 4;
        let (_tx, rx) = watch::channel(false);
        let token_counter = Arc::new(TokenCounter::new());
        // Always create the receiver side of the experiment notification channel so the
        // select! branch in the agent loop compiles unconditionally. The sender is only
        // stored when the experiments feature is enabled (it is only used in experiment_cmd.rs).
        #[cfg(feature = "experiments")]
        let (exp_notify_tx, exp_notify_rx) = tokio::sync::mpsc::channel::<String>(4);
        #[cfg(not(feature = "experiments"))]
        let (_exp_notify_tx, exp_notify_rx) = tokio::sync::mpsc::channel::<String>(4);
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
                graph_config: crate::config::GraphConfig::default(),
            },
            skill_state: SkillState {
                registry,
                skill_paths: Vec::new(),
                managed_dir: None,
                trust_config: crate::config::TrustConfig::default(),
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
            judge_detector: None,
            judge_provider: None,
            config_path: None,
            logging_config: crate::config::LoggingConfig::default(),
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
            custom_task_rx: None,
            subagent_manager: None,
            subagent_config: crate::config::SubAgentConfig::default(),
            orchestration_config: crate::config::OrchestrationConfig::default(),
            #[cfg(feature = "experiments")]
            experiment_config: crate::config::ExperimentConfig::default(),
            #[cfg(feature = "experiments")]
            experiment_baseline: crate::experiments::ConfigSnapshot::default(),
            experiment_notify_rx: Some(exp_notify_rx),
            #[cfg(feature = "experiments")]
            experiment_notify_tx: exp_notify_tx,
            response_cache: None,
            parent_tool_use_id: None,
            anomaly_detector: None,
            instruction_blocks: Vec::new(),
            instruction_reload_rx: None,
            instruction_reload_state: None,
            sanitizer: ContentSanitizer::new(&crate::sanitizer::ContentIsolationConfig::default()),
            quarantine_summarizer: None,
            exfiltration_guard: crate::sanitizer::exfiltration::ExfiltrationGuard::new(
                crate::sanitizer::exfiltration::ExfiltrationGuardConfig::default(),
            ),
            flagged_urls: std::collections::HashSet::new(),
            pending_image_parts: Vec::new(),
            pending_graph: None,
            debug_dumper: None,
            dump_format: crate::debug_dump::DumpFormat::default(),
            #[cfg(feature = "lsp-context")]
            lsp_hooks: None,
            #[cfg(feature = "experiments")]
            experiment_cancel: None,
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

    async fn handle_plan_command(
        &mut self,
        cmd: crate::orchestration::PlanCommand,
    ) -> Result<(), error::AgentError> {
        use crate::orchestration::PlanCommand;

        if !self.config_for_orchestration().enabled {
            self.channel
                .send(
                    "Task orchestration is disabled. Set `orchestration.enabled = true` in config.",
                )
                .await?;
            return Ok(());
        }

        match cmd {
            PlanCommand::Goal(goal) => self.handle_plan_goal(&goal).await,
            PlanCommand::Confirm => self.handle_plan_confirm().await,
            PlanCommand::Status(id) => self.handle_plan_status(id.as_deref()).await,
            PlanCommand::List => self.handle_plan_list().await,
            PlanCommand::Cancel(id) => self.handle_plan_cancel(id.as_deref()).await,
            PlanCommand::Resume(id) => self.handle_plan_resume(id.as_deref()).await,
            PlanCommand::Retry(id) => self.handle_plan_retry(id.as_deref()).await,
        }
    }

    fn config_for_orchestration(&self) -> &crate::config::OrchestrationConfig {
        &self.orchestration_config
    }

    async fn handle_plan_goal(&mut self, goal: &str) -> Result<(), error::AgentError> {
        use crate::orchestration::{LlmPlanner, Planner};

        if self.pending_graph.is_some() {
            self.channel
                .send(
                    "A plan is already pending confirmation. \
                     Use /plan confirm to execute it or /plan cancel to discard.",
                )
                .await?;
            return Ok(());
        }

        self.channel.send("Planning task decomposition...").await?;

        let available_agents = self
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();

        let confirm_before_execute = self.orchestration_config.confirm_before_execute;
        let graph = LlmPlanner::new(self.provider.clone(), &self.orchestration_config)
            .plan(goal, &available_agents)
            .await
            .map_err(|e| error::AgentError::Other(e.to_string()))?;

        let task_count = graph.tasks.len() as u64;
        let snapshot = crate::metrics::TaskGraphSnapshot::from(&graph);
        self.update_metrics(|m| {
            m.orchestration.plans_total += 1;
            m.orchestration.tasks_total += task_count;
            m.orchestration_graph = Some(snapshot);
        });

        if confirm_before_execute {
            let summary = format_plan_summary(&graph);
            self.channel.send(&summary).await?;
            self.channel
                .send("Type `/plan confirm` to execute, or `/plan cancel` to abort.")
                .await?;
            self.pending_graph = Some(graph);
        } else {
            // confirm_before_execute = false: display and proceed (Phase 5 will run scheduler).
            // TODO(#1241): wire DagScheduler tick updates for Running task state
            let summary = format_plan_summary(&graph);
            self.channel.send(&summary).await?;
            self.channel
                .send("Plan ready. Full execution will be available in a future phase.")
                .await?;
            // IC1: graph was shown but never confirmed; clear snapshot so it doesn't linger.
            let now = std::time::Instant::now();
            self.update_metrics(|m| {
                if let Some(ref mut s) = m.orchestration_graph {
                    "completed".clone_into(&mut s.status);
                    s.completed_at = Some(now);
                }
            });
        }

        Ok(())
    }

    async fn handle_plan_confirm(&mut self) -> Result<(), error::AgentError> {
        use crate::orchestration::{DagScheduler, GraphStatus, RuleBasedRouter};

        let Some(graph) = self.pending_graph.take() else {
            self.channel
                .send("No pending plan to confirm. Use `/plan <goal>` to create one.")
                .await?;
            return Ok(());
        };

        // When subagent manager is not configured, restore graph and inform the user.
        if self.subagent_manager.is_none() {
            self.channel
                .send(
                    "No sub-agents configured. Add sub-agent definitions to config \
                     to enable plan execution.",
                )
                .await?;
            self.pending_graph = Some(graph);
            return Ok(());
        }

        // REV-2: pre-validate before moving graph into the constructor so we can
        // restore it to pending_graph on failure.
        if graph.tasks.is_empty() {
            self.channel.send("Plan has no tasks.").await?;
            self.pending_graph = Some(graph);
            return Ok(());
        }
        // resume_from() rejects Completed and Canceled — guard those here too.
        if matches!(graph.status, GraphStatus::Completed | GraphStatus::Canceled) {
            self.channel
                .send(&format!(
                    "Cannot re-execute a {} plan. Use `/plan <goal>` to create a new one.",
                    graph.status
                ))
                .await?;
            self.pending_graph = Some(graph);
            return Ok(());
        }

        let available_agents = self
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();

        // Use resume_from() for graphs that are no longer in Created status
        // (e.g., after /plan retry which calls reset_for_retry and sets status=Running).
        let mut scheduler = if graph.status == GraphStatus::Created {
            DagScheduler::new(
                graph,
                &self.orchestration_config,
                Box::new(RuleBasedRouter),
                available_agents,
            )
        } else {
            DagScheduler::resume_from(
                graph,
                &self.orchestration_config,
                Box::new(RuleBasedRouter),
                available_agents,
            )
        }
        .map_err(|e| error::AgentError::Other(e.to_string()))?;

        let task_count = scheduler.graph().tasks.len();
        self.channel
            .send(&format!(
                "Confirmed. Executing plan ({task_count} tasks)..."
            ))
            .await?;

        let final_status = self.run_scheduler_loop(&mut scheduler, task_count).await?;

        let completed_graph = scheduler.into_graph();

        // Final TUI snapshot update.
        let snapshot = crate::metrics::TaskGraphSnapshot::from(&completed_graph);
        self.update_metrics(|m| {
            m.orchestration_graph = Some(snapshot);
        });

        let result_label = self
            .finalize_plan_execution(completed_graph, final_status)
            .await?;

        let now = std::time::Instant::now();
        self.update_metrics(|m| {
            if let Some(ref mut s) = m.orchestration_graph {
                result_label.clone_into(&mut s.status);
                s.completed_at = Some(now);
            }
        });
        Ok(())
    }

    /// Drive the [`DagScheduler`] tick loop until it emits `SchedulerAction::Done`.
    ///
    /// # Known limitations
    ///
    /// The agent is single-threaded; this loop blocks all message processing while
    /// running. `/plan cancel` cannot interrupt an active execution. A future phase
    /// will add a `CancellationToken` field to `Agent` and wire it into this loop.
    /// (SEC-M34-001, tracked in GitHub issue.)
    async fn run_scheduler_loop(
        &mut self,
        scheduler: &mut crate::orchestration::DagScheduler,
        task_count: usize,
    ) -> Result<crate::orchestration::GraphStatus, error::AgentError> {
        use crate::orchestration::SchedulerAction;

        // Sequential spawn counter for human-readable "task N/M" progress messages.
        // task_id.index() reflects array position and can be non-contiguous for
        // parallel plans (e.g. 0, 2, 4), so we use a local counter instead.
        let mut spawn_counter: usize = 0;

        let final_status = 'tick: loop {
            let actions = scheduler.tick();

            for action in actions {
                match action {
                    SchedulerAction::Spawn {
                        task_id,
                        agent_def_name,
                        prompt,
                    } => {
                        spawn_counter += 1;
                        let task_title = scheduler
                            .graph()
                            .tasks
                            .get(task_id.index())
                            .map_or("unknown", |t| t.title.as_str());
                        let _ = self
                            .channel
                            .send_status(&format!(
                                "Executing task {spawn_counter}/{task_count}: {task_title}..."
                            ))
                            .await;

                        let provider = self.provider.clone();
                        let tool_executor = Arc::clone(&self.tool_executor);
                        let skills = self.filtered_skills_for(&agent_def_name);
                        let cfg = self.subagent_config.clone();
                        let event_tx = scheduler.event_sender();

                        let mgr = self
                            .subagent_manager
                            .as_mut()
                            .expect("subagent_manager checked above");
                        match mgr.spawn_for_task(
                            &agent_def_name,
                            &prompt,
                            provider,
                            tool_executor,
                            skills,
                            &cfg,
                            task_id,
                            event_tx,
                        ) {
                            Ok(handle_id) => {
                                scheduler.record_spawn(task_id, handle_id, agent_def_name);
                            }
                            Err(e) => {
                                tracing::error!(error = %e, %task_id, "spawn_for_task failed");
                                let extra = scheduler.record_spawn_failure(task_id, &e.to_string());
                                for a in extra {
                                    match a {
                                        SchedulerAction::Cancel { agent_handle_id } => {
                                            if let Some(m) = self.subagent_manager.as_mut() {
                                                // benign race: agent may have already finished
                                                let _ =
                                                    m.cancel(&agent_handle_id).inspect_err(|err| {
                                                        tracing::trace!(
                                                            error = %err,
                                                            "cancel after spawn failure: agent already gone"
                                                        );
                                                    });
                                            }
                                        }
                                        SchedulerAction::Done { status } => {
                                            break 'tick status;
                                        }
                                        SchedulerAction::Spawn { .. } => {}
                                    }
                                }
                            }
                        }
                    }
                    SchedulerAction::Cancel { agent_handle_id } => {
                        if let Some(mgr) = self.subagent_manager.as_mut() {
                            // benign race: agent may have already finished
                            let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                                tracing::trace!(error = %e, "cancel: agent already gone");
                            });
                        }
                    }
                    SchedulerAction::Done { status } => {
                        break 'tick status;
                    }
                }
            }

            // Drain all pending secret requests this tick (MED-2 fix).
            self.process_pending_secret_requests().await;

            // Update TUI with current graph state.
            let snapshot = crate::metrics::TaskGraphSnapshot::from(scheduler.graph());
            self.update_metrics(|m| {
                m.orchestration_graph = Some(snapshot);
            });

            scheduler.wait_event().await;
        };

        Ok(final_status)
    }

    /// Bridge pending secret requests from sub-agents to the user (non-blocking, time-bounded).
    ///
    /// SEC-P1-02: explicit user confirmation is required before granting any secret to a
    /// sub-agent. Denial is the default on timeout or channel error.
    async fn process_pending_secret_requests(&mut self) {
        loop {
            let pending = self
                .subagent_manager
                .as_mut()
                .and_then(crate::subagent::SubAgentManager::try_recv_secret_request);
            let Some((req_handle_id, req)) = pending else {
                break;
            };
            let prompt = format!(
                "Sub-agent requests secret '{}'. Allow?{}",
                req.secret_key,
                req.reason
                    .as_deref()
                    .map(|r| format!(" Reason: {r}"))
                    .unwrap_or_default()
            );
            // CRIT-1 fix: use select! to avoid blocking the tick loop forever.
            let approved = tokio::select! {
                result = self.channel.confirm(&prompt) => result.unwrap_or(false),
                () = tokio::time::sleep(std::time::Duration::from_secs(120)) => {
                    let _ = self.channel.send("Secret request timed out.").await;
                    false
                }
            };
            if let Some(mgr) = self.subagent_manager.as_mut() {
                if approved {
                    let ttl = std::time::Duration::from_secs(300);
                    let key = req.secret_key.clone();
                    if mgr.approve_secret(&req_handle_id, &key, ttl).is_ok() {
                        let _ = mgr.deliver_secret(&req_handle_id, key);
                    }
                } else {
                    let _ = mgr.deny_secret(&req_handle_id);
                }
            }
        }
    }

    /// Aggregate results or report failure after the tick loop completes.
    async fn finalize_plan_execution(
        &mut self,
        completed_graph: crate::orchestration::TaskGraph,
        final_status: crate::orchestration::GraphStatus,
    ) -> Result<&'static str, error::AgentError> {
        use std::fmt::Write;

        use crate::orchestration::{Aggregator, GraphStatus, LlmAggregator};

        let result_label = match final_status {
            GraphStatus::Completed => {
                // Update task completion counters.
                let completed_count = completed_graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == crate::orchestration::TaskStatus::Completed)
                    .count() as u64;
                self.update_metrics(|m| m.orchestration.tasks_completed += completed_count);

                let aggregator =
                    LlmAggregator::new(self.provider.clone(), &self.orchestration_config);
                match aggregator.aggregate(&completed_graph).await {
                    Ok(synthesis) => {
                        self.channel.send(&synthesis).await?;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "aggregation failed");
                        self.channel
                            .send(
                                "Plan completed but aggregation failed. \
                                 Check individual task results.",
                            )
                            .await?;
                    }
                }
                "completed"
            }
            GraphStatus::Failed => {
                let failed_tasks: Vec<_> = completed_graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == crate::orchestration::TaskStatus::Failed)
                    .collect();
                self.update_metrics(|m| {
                    m.orchestration.tasks_failed += failed_tasks.len() as u64;
                });
                let mut msg = format!(
                    "Plan failed. {}/{} tasks failed:\n",
                    failed_tasks.len(),
                    completed_graph.tasks.len()
                );
                for t in &failed_tasks {
                    // SEC-M34-002: truncate raw task output before displaying to user.
                    let err: std::borrow::Cow<str> =
                        t.result.as_ref().map_or("unknown error".into(), |r| {
                            if r.output.len() > 500 {
                                r.output.chars().take(500).collect::<String>().into()
                            } else {
                                r.output.as_str().into()
                            }
                        });
                    let _ = writeln!(msg, "  - {}: {err}", t.title);
                }
                msg.push_str("\nUse `/plan retry` to retry failed tasks.");
                self.channel.send(&msg).await?;
                // Store graph back so /plan retry and /plan resume work.
                self.pending_graph = Some(completed_graph);
                "failed"
            }
            GraphStatus::Paused => {
                self.channel
                    .send(
                        "Plan paused due to a task failure (ask strategy). \
                         Use `/plan resume` to continue or `/plan retry` to retry failed tasks.",
                    )
                    .await?;
                self.pending_graph = Some(completed_graph);
                "paused"
            }
            other => {
                tracing::warn!(%other, "unexpected graph status after Done");
                self.channel
                    .send(&format!("Plan ended with status: {other}"))
                    .await?;
                "canceled"
            }
        };
        Ok(result_label)
    }

    async fn handle_plan_status(
        &mut self,
        _graph_id: Option<&str>,
    ) -> Result<(), error::AgentError> {
        if self.pending_graph.is_some() {
            self.channel
                .send("A plan is awaiting confirmation. Type `/plan confirm` to execute or `/plan cancel` to abort.")
                .await?;
        } else {
            self.channel.send("No active plan.").await?;
        }
        Ok(())
    }

    async fn handle_plan_list(&mut self) -> Result<(), error::AgentError> {
        if let Some(ref graph) = self.pending_graph {
            let summary = format_plan_summary(graph);
            let status_label = match graph.status {
                crate::orchestration::GraphStatus::Created => "awaiting confirmation",
                crate::orchestration::GraphStatus::Running => "running",
                crate::orchestration::GraphStatus::Paused => "paused",
                crate::orchestration::GraphStatus::Failed => "failed (retryable)",
                _ => "unknown",
            };
            self.channel
                .send(&format!("{summary}\nStatus: {status_label}"))
                .await?;
        } else {
            self.channel.send("No recent plans.").await?;
        }
        Ok(())
    }

    async fn handle_plan_cancel(
        &mut self,
        _graph_id: Option<&str>,
    ) -> Result<(), error::AgentError> {
        if self.pending_graph.take().is_some() {
            let now = std::time::Instant::now();
            self.update_metrics(|m| {
                if let Some(ref mut s) = m.orchestration_graph {
                    "canceled".clone_into(&mut s.status);
                    s.completed_at = Some(now);
                }
            });
            self.channel.send("Plan canceled.").await?;
        } else {
            self.channel.send("No active plan to cancel.").await?;
        }
        Ok(())
    }

    /// Resume a paused graph (Ask failure strategy triggered a pause).
    ///
    /// Looks for a pending graph in `Paused` status. If `graph_id` is provided
    /// it must match the active graph's id (SEC-P5-03).
    async fn handle_plan_resume(
        &mut self,
        graph_id: Option<&str>,
    ) -> Result<(), error::AgentError> {
        use crate::orchestration::GraphStatus;

        let Some(ref graph) = self.pending_graph else {
            self.channel
                .send("No paused plan to resume. Use `/plan status` to check the current state.")
                .await?;
            return Ok(());
        };

        // SEC-P5-03: if a graph_id was provided, reject if it doesn't match.
        if let Some(id) = graph_id
            && graph.id.to_string() != id
        {
            self.channel
                .send(&format!(
                    "Graph id '{id}' does not match the active plan ({}). \
                     Use `/plan status` to see the active plan id.",
                    graph.id
                ))
                .await?;
            return Ok(());
        }

        if graph.status != GraphStatus::Paused {
            self.channel
                .send(&format!(
                    "The active plan is in '{}' status and cannot be resumed. \
                     Only Paused plans can be resumed.",
                    graph.status
                ))
                .await?;
            return Ok(());
        }

        let graph = self.pending_graph.take().unwrap();

        tracing::info!(
            graph_id = %graph.id,
            "resuming paused graph"
        );

        self.channel
            .send(&format!(
                "Resuming plan: {}\nUse `/plan confirm` to continue execution.",
                graph.goal
            ))
            .await?;

        // Store resumed graph back as pending. resume_from() will set status=Running in confirm.
        self.pending_graph = Some(graph);
        Ok(())
    }

    /// Retry failed tasks in a graph.
    ///
    /// Resets all `Failed` tasks to `Ready` and all `Skipped` dependents back
    /// to `Pending`, then re-stores the graph as pending for re-execution.
    /// If `graph_id` is provided it must match the active graph's id (SEC-P5-04).
    async fn handle_plan_retry(&mut self, graph_id: Option<&str>) -> Result<(), error::AgentError> {
        use crate::orchestration::{GraphStatus, dag};

        let Some(ref graph) = self.pending_graph else {
            self.channel
                .send("No active plan to retry. Use `/plan status` to check the current state.")
                .await?;
            return Ok(());
        };

        // SEC-P5-04: if a graph_id was provided, reject if it doesn't match.
        if let Some(id) = graph_id
            && graph.id.to_string() != id
        {
            self.channel
                .send(&format!(
                    "Graph id '{id}' does not match the active plan ({}). \
                     Use `/plan status` to see the active plan id.",
                    graph.id
                ))
                .await?;
            return Ok(());
        }

        if graph.status != GraphStatus::Failed && graph.status != GraphStatus::Paused {
            self.channel
                .send(&format!(
                    "The active plan is in '{}' status. Only Failed or Paused plans can be retried.",
                    graph.status
                ))
                .await?;
            return Ok(());
        }

        let mut graph = self.pending_graph.take().unwrap();

        // IC3: count before reset so the message reflects actual failed tasks, not Ready count.
        let failed_count = graph
            .tasks
            .iter()
            .filter(|t| t.status == crate::orchestration::TaskStatus::Failed)
            .count();

        dag::reset_for_retry(&mut graph).map_err(|e| error::AgentError::Other(e.to_string()))?;

        // HIGH-1 fix: reset_for_retry only resets Failed/Canceled tasks. Any tasks that were
        // in Running state at pause time are left as Running with stale assigned_agent handles
        // (those sub-agents are long dead). Reset them to Ready so resume_from() does not try
        // to wait for their events.
        for task in &mut graph.tasks {
            if task.status == crate::orchestration::TaskStatus::Running {
                task.status = crate::orchestration::TaskStatus::Ready;
                task.assigned_agent = None;
            }
        }

        tracing::info!(
            graph_id = %graph.id,
            failed_count,
            "retrying failed tasks in graph"
        );

        self.channel
            .send(&format!(
                "Retrying {failed_count} failed task(s) in plan: {}\n\
                 Use `/plan confirm` to execute.",
                graph.goal
            ))
            .await?;

        // Store retried graph back for re-execution via /plan confirm.
        self.pending_graph = Some(graph);
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        self.channel.send("Shutting down...").await.ok();

        // CRIT-1: persist Thompson state accumulated during this session.
        self.provider.save_router_state();

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
                            permission_mode: def.map_or_else(String::new, |d| {
                                use crate::subagent::def::PermissionMode;
                                match d.permissions.permission_mode {
                                    PermissionMode::Default => String::new(),
                                    PermissionMode::AcceptEdits => "accept_edits".into(),
                                    PermissionMode::DontAsk => "dont_ask".into(),
                                    PermissionMode::BypassPermissions => {
                                        "bypass_permissions".into()
                                    }
                                    PermissionMode::Plan => "plan".into(),
                                }
                            }),
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
                    Some(_) = recv_optional(&mut self.instruction_reload_rx) => {
                        self.reload_instructions();
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
                    Some(msg) = recv_optional(&mut self.experiment_notify_rx) => {
                        // Experiment engine completed (ok or err). Clear the cancel token so
                        // status reports idle and new experiments can be started.
                        #[cfg(feature = "experiments")]
                        { self.experiment_cancel = None; }
                        if let Err(e) = self.channel.send(&msg).await {
                            tracing::warn!("failed to send experiment completion: {e}");
                        }
                        continue;
                    }
                    Some(prompt) = recv_optional(&mut self.custom_task_rx) => {
                        tracing::info!("scheduler: injecting custom task as agent turn");
                        let text = format!("[Scheduled task] {prompt}");
                        Some(crate::channel::ChannelMessage { text, attachments: Vec::new() })
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

            if trimmed == "/debug-dump" || trimmed.starts_with("/debug-dump ") {
                self.handle_debug_dump_command(trimmed).await;
                let _ = self.channel.flush_chunks().await;
                continue;
            }

            if trimmed == "/exit" || trimmed == "/quit" {
                if self.channel.supports_exit() {
                    break;
                }
                let _ = self
                    .channel
                    .send("/exit is not supported in this channel.")
                    .await;
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
    #[allow(clippy::too_many_lines)]
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

        // Validate model_id against the known model list before switching.
        // Try disk cache first; fall back to a remote fetch if the cache is stale.
        let cache = zeph_llm::model_cache::ModelCache::for_slug(self.provider.name());
        let known_models: Option<Vec<zeph_llm::model_cache::RemoteModelInfo>> = if cache.is_stale()
        {
            match self.provider.list_models_remote().await {
                Ok(m) if !m.is_empty() => Some(m),
                _ => None,
            }
        } else {
            cache.load().unwrap_or(None)
        };
        if let Some(models) = known_models {
            if !models.iter().any(|m| m.id == model_id) {
                let mut lines = vec![format!("Unknown model '{model_id}'. Available models:")];
                for m in &models {
                    lines.push(format!("  • {} ({})", m.display_name, m.id));
                }
                let _ = self.channel.send(&lines.join("\n")).await;
                return;
            }
        } else {
            let _ = self
                .channel
                .send(
                    "Model list unavailable, switching anyway — verify your model name is correct.",
                )
                .await;
        }

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

    /// Handle `/debug-dump` and `/debug-dump <path>` commands.
    async fn handle_debug_dump_command(&mut self, trimmed: &str) {
        let arg = trimmed.strip_prefix("/debug-dump").map_or("", str::trim);
        if arg.is_empty() {
            match &self.debug_dumper {
                Some(d) => {
                    let _ = self
                        .channel
                        .send(&format!("Debug dump active: {}", d.dir().display()))
                        .await;
                }
                None => {
                    let _ = self
                        .channel
                        .send(
                            "Debug dump is inactive. Use `/debug-dump <path>` to enable, \
                             or start with `--debug-dump [dir]`.",
                        )
                        .await;
                }
            }
            return;
        }
        let dir = std::path::PathBuf::from(arg);
        match crate::debug_dump::DebugDumper::new(&dir, self.dump_format) {
            Ok(dumper) => {
                let path = dumper.dir().display().to_string();
                self.debug_dumper = Some(dumper);
                let _ = self
                    .channel
                    .send(&format!("Debug dump enabled: {path}"))
                    .await;
            }
            Err(e) => {
                let _ = self
                    .channel
                    .send(&format!("Failed to enable debug dump: {e}"))
                    .await;
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

        if trimmed == "/help" {
            self.handle_help_command().await?;
            return Ok(());
        }

        if trimmed == "/status" {
            self.handle_status_command().await?;
            return Ok(());
        }

        if trimmed == "/skills" {
            self.handle_skills_command().await?;
            return Ok(());
        }

        if trimmed == "/skill" || trimmed.starts_with("/skill ") {
            let rest = trimmed.strip_prefix("/skill").unwrap_or("").trim();
            self.handle_skill_command(rest).await?;
            return Ok(());
        }

        if trimmed == "/feedback" || trimmed.starts_with("/feedback ") {
            let rest = trimmed.strip_prefix("/feedback").unwrap_or("").trim();
            self.handle_feedback(rest).await?;
            return Ok(());
        }

        if trimmed == "/mcp" || trimmed.starts_with("/mcp ") {
            let args = trimmed.strip_prefix("/mcp").unwrap_or("").trim();
            self.handle_mcp_command(args).await?;
            return Ok(());
        }

        if trimmed == "/image" || trimmed.starts_with("/image ") {
            let path = trimmed.strip_prefix("/image").unwrap_or("").trim();
            if path.is_empty() {
                self.channel.send("Usage: /image <path>").await?;
                return Ok(());
            }
            return self.handle_image_command(path).await;
        }

        if trimmed == "/plan" || trimmed.starts_with("/plan ") {
            match crate::orchestration::PlanCommand::parse(trimmed) {
                Ok(cmd) => {
                    self.handle_plan_command(cmd).await?;
                    return Ok(());
                }
                Err(e) => {
                    self.channel.send(&e.to_string()).await?;
                    return Ok(());
                }
            }
        }

        if trimmed == "/graph" || trimmed.starts_with("/graph ") {
            self.handle_graph_command(trimmed).await?;
            return Ok(());
        }

        #[cfg(feature = "experiments")]
        if trimmed == "/experiment" || trimmed.starts_with("/experiment ") {
            self.handle_experiment_command(trimmed).await?;
            return Ok(());
        }

        #[cfg(feature = "lsp-context")]
        if trimmed == "/lsp" {
            self.handle_lsp_status_command().await?;
            return Ok(());
        }

        if trimmed == "/log" {
            self.handle_log_command().await?;
            return Ok(());
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
            let regex_signal = self
                .feedback_detector
                .detect(trimmed, &previous_user_messages);

            // Judge mode: invoke LLM in background if regex is borderline or missed.
            //
            // The judge call is decoupled from the response pipeline — it records the
            // correction asynchronously via tokio::spawn and returns None immediately
            // so the user response is not blocked.
            //
            // TODO(I3): JoinHandles are not tracked — outstanding tasks may be aborted
            // on runtime shutdown before store_user_correction completes. This is
            // acceptable for the learning subsystem at MVP. Future: collect handles in
            // Agent and drain on graceful shutdown.
            // Check rate limit synchronously before deciding to spawn.
            // The judge_detector is &mut self so check_rate_limit() can update call_times.
            let judge_should_run = self
                .judge_detector
                .as_ref()
                .is_some_and(|jd| jd.should_invoke(regex_signal.as_ref()))
                && self
                    .judge_detector
                    .as_mut()
                    .is_some_and(feedback_detector::JudgeDetector::check_rate_limit);

            let signal = if judge_should_run {
                let judge_provider = self
                    .judge_provider
                    .clone()
                    .unwrap_or_else(|| self.provider.clone());
                let assistant_snippet = self.last_assistant_response();
                let user_msg_owned = trimmed.to_owned();
                let memory_arc = self.memory_state.memory.clone();
                let skill_name = self
                    .skill_state
                    .active_skill_names
                    .first()
                    .cloned()
                    .unwrap_or_default();
                let conv_id_bg = conv_id;
                // Extract only the scalar config values needed by the spawned task.
                let confidence_threshold = self
                    .learning_engine
                    .config
                    .as_ref()
                    .map_or(0.6, |c| c.correction_confidence_threshold);

                tokio::spawn(async move {
                    match feedback_detector::JudgeDetector::evaluate(
                        &judge_provider,
                        &user_msg_owned,
                        &assistant_snippet,
                        confidence_threshold,
                    )
                    .await
                    {
                        Ok(verdict) => {
                            if let Some(signal) = verdict.into_signal(&user_msg_owned) {
                                // Self-corrections (user corrects their own statement) must not
                                // penalize skills. The judge path has no record_skill_outcomes()
                                // call today, but this guard mirrors the regex path to make the
                                // intent explicit and prevent future regressions if parity is added.
                                let is_self_correction = signal.kind
                                    == feedback_detector::CorrectionKind::SelfCorrection;
                                tracing::info!(
                                    kind = signal.kind.as_str(),
                                    confidence = signal.confidence,
                                    source = "judge",
                                    is_self_correction,
                                    "correction signal detected"
                                );
                                if let Some(memory) = memory_arc {
                                    let correction_text =
                                        context::truncate_chars(&user_msg_owned, 500);
                                    match memory
                                        .sqlite()
                                        .store_user_correction(
                                            conv_id_bg.map(|c| c.0),
                                            &assistant_snippet,
                                            &correction_text,
                                            if skill_name.is_empty() {
                                                None
                                            } else {
                                                Some(skill_name.as_str())
                                            },
                                            signal.kind.as_str(),
                                        )
                                        .await
                                    {
                                        Ok(correction_id) => {
                                            if let Err(e) = memory
                                                .store_correction_embedding(
                                                    correction_id,
                                                    &correction_text,
                                                )
                                                .await
                                            {
                                                tracing::warn!(
                                                    "failed to store correction embedding: {e:#}"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "failed to store judge correction: {e:#}"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("judge detector failed: {e:#}");
                        }
                    }
                });

                // Judge runs in background — return None so the response pipeline continues.
                None
            } else {
                regex_signal
            };

            if let Some(signal) = signal {
                tracing::info!(
                    kind = signal.kind.as_str(),
                    confidence = signal.confidence,
                    source = "regex",
                    "implicit correction detected"
                );
                // REV-PH2-002 + SEC-PH2-002: cap feedback_text to 500 chars (UTF-8 safe)
                let feedback_text = context::truncate_chars(&signal.feedback_text, 500);
                // Self-corrections (user corrects their own statement) must not penalize skills —
                // the agent did nothing wrong. Store for analytics but skip skill outcome recording.
                if signal.kind != feedback_detector::CorrectionKind::SelfCorrection {
                    self.record_skill_outcomes(
                        "user_rejection",
                        Some(&feedback_text),
                        Some(signal.kind.as_str()),
                    )
                    .await;
                }
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

        // Reset per-turn compaction guard at the start of context management phase.
        self.context_manager.compacted_this_turn = false;

        // Tier 0: batch-apply deferred tool summaries when approaching context limit.
        // This is a pure in-memory operation (no LLM call) — summaries were pre-computed
        // during the tool loop. Intentionally does NOT set compacted_this_turn, so
        // proactive/reactive compaction may still fire if tokens remain above their thresholds.
        self.maybe_apply_deferred_summaries();

        // Proactive compression fires first (if configured); if it runs, reactive is skipped.
        if let Err(e) = self.maybe_proactive_compress().await {
            tracing::warn!("proactive compression failed: {e:#}");
        }

        if let Err(e) = self.maybe_compact().await {
            tracing::warn!("context compaction failed: {e:#}");
        }

        if let Err(e) = Box::pin(self.prepare_context(trimmed)).await {
            tracing::warn!("context preparation failed: {e:#}");
        }

        self.learning_engine.reset_reflection();

        let mut all_image_parts = std::mem::take(&mut self.pending_image_parts);
        all_image_parts.extend(image_parts);
        let image_parts = all_image_parts;

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
        // Image parts intentionally excluded — base64 payloads too large for message history.
        self.persist_message(Role::User, &text, &[], false).await;
        self.push_message(user_msg);

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

    async fn handle_image_command(&mut self, path: &str) -> Result<(), error::AgentError> {
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
        self.pending_image_parts
            .push(MessagePart::Image(Box::new(ImageData { data, mime_type })));
        self.channel
            .send(&format!("Image loaded: {path}. Send your message."))
            .await?;
        Ok(())
    }

    async fn handle_help_command(&mut self) -> Result<(), error::AgentError> {
        use std::fmt::Write;

        let mut out = String::from("Slash commands:\n\n");

        let categories = [
            slash_commands::SlashCategory::Info,
            slash_commands::SlashCategory::Session,
            slash_commands::SlashCategory::Model,
            slash_commands::SlashCategory::Memory,
            slash_commands::SlashCategory::Tools,
            slash_commands::SlashCategory::Planning,
            slash_commands::SlashCategory::Debug,
            slash_commands::SlashCategory::Advanced,
        ];

        for cat in &categories {
            let entries: Vec<_> = slash_commands::COMMANDS
                .iter()
                .filter(|c| &c.category == cat)
                .collect();
            if entries.is_empty() {
                continue;
            }
            let _ = writeln!(out, "{}:", cat.as_str());
            for cmd in entries {
                if cmd.args.is_empty() {
                    let _ = write!(out, "  {}", cmd.name);
                } else {
                    let _ = write!(out, "  {} {}", cmd.name, cmd.args);
                }
                let _ = write!(out, "  — {}", cmd.description);
                if let Some(feat) = cmd.feature_gate {
                    let _ = write!(out, " [requires: {feat}]");
                }
                let _ = writeln!(out);
            }
            let _ = writeln!(out);
        }

        self.channel.send(out.trim_end()).await?;
        Ok(())
    }

    async fn handle_status_command(&mut self) -> Result<(), error::AgentError> {
        use std::fmt::Write;

        let uptime = self.start_time.elapsed().as_secs();
        let msg_count = self
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count();

        let (api_calls, prompt_tokens, completion_tokens, cost_cents, mcp_servers) =
            if let Some(ref tx) = self.metrics_tx {
                let m = tx.borrow();
                (
                    m.api_calls,
                    m.prompt_tokens,
                    m.completion_tokens,
                    m.cost_spent_cents,
                    m.mcp_server_count,
                )
            } else {
                (0, 0, 0, 0.0, 0)
            };

        let skill_count = self
            .skill_state
            .registry
            .read()
            .map(|r| r.all_meta().len())
            .unwrap_or(0);

        let mut out = String::from("Session status:\n\n");
        let _ = writeln!(out, "Provider:  {}", self.provider.name());
        let _ = writeln!(out, "Model:     {}", self.runtime.model_name);
        let _ = writeln!(out, "Uptime:    {uptime}s");
        let _ = writeln!(out, "Turns:     {msg_count}");
        let _ = writeln!(out, "API calls: {api_calls}");
        let _ = writeln!(
            out,
            "Tokens:    {prompt_tokens} prompt / {completion_tokens} completion"
        );
        let _ = writeln!(out, "Skills:    {skill_count}");
        let _ = writeln!(out, "MCP:       {mcp_servers} server(s)");
        if cost_cents > 0.0 {
            let _ = writeln!(out, "Cost:      ${:.4}", cost_cents / 100.0);
        }

        self.channel.send(out.trim_end()).await?;
        Ok(())
    }

    async fn handle_skills_command(&mut self) -> Result<(), error::AgentError> {
        use std::fmt::Write;

        let mut output = String::from("Available skills:\n\n");

        let all_meta: Vec<zeph_skills::loader::SkillMeta> = self
            .skill_state
            .registry
            .read()
            .expect("registry read lock")
            .all_meta()
            .into_iter()
            .cloned()
            .collect();

        for meta in &all_meta {
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
                    let memory_label = match d.memory {
                        Some(crate::subagent::MemoryScope::User) => " [memory:user]",
                        Some(crate::subagent::MemoryScope::Project) => " [memory:project]",
                        Some(crate::subagent::MemoryScope::Local) => " [memory:local]",
                        None => "",
                    };
                    if let Some(ref src) = d.source {
                        let _ = writeln!(
                            out,
                            "  {}{} — {} ({})",
                            d.name, memory_label, d.description, src
                        );
                    } else {
                        let _ = writeln!(out, "  {}{} — {}", d.name, memory_label, d.description);
                    }
                }
                Some(out)
            }
            AgentCommand::Background { name, prompt } => {
                let provider = self.provider.clone();
                let tool_executor = Arc::clone(&self.tool_executor);
                let skills = self.filtered_skills_for(&name);
                let mgr = self.subagent_manager.as_mut()?;
                let cfg = self.subagent_config.clone();
                match mgr.spawn(&name, &prompt, provider, tool_executor, skills, &cfg) {
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
                let cfg = self.subagent_config.clone();
                let task_id = match mgr.spawn(&name, &prompt, provider, tool_executor, skills, &cfg)
                {
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

                    // Bridge secret requests from sub-agent to channel.confirm().
                    // Fetch the pending request first, then release the borrow before
                    // calling channel.confirm() (which requires &mut self).
                    #[allow(clippy::redundant_closure_for_method_calls)]
                    let pending = self
                        .subagent_manager
                        .as_mut()
                        .and_then(|m| m.try_recv_secret_request());
                    if let Some((req_task_id, req)) = pending {
                        // req.secret_key is pre-validated to [a-zA-Z0-9_-] in manager.rs
                        // (SEC-P1-02), so it is safe to embed in the prompt string.
                        //
                        // confirm() timeout (30s for Telegram) is a UX timeout — how long to
                        // wait for operator input. The grant TTL (300s below) is a security
                        // bound on how long an approved secret remains usable. Both values are
                        // intentionally different: short confirm window, longer grant lifetime.
                        let prompt =
                            format!("Sub-agent requests secret '{}'. Allow?", req.secret_key);
                        let approved = self.channel.confirm(&prompt).await.unwrap_or(false);
                        if let Some(mgr) = self.subagent_manager.as_mut() {
                            if approved {
                                let ttl = std::time::Duration::from_secs(300);
                                let key = req.secret_key.clone();
                                if mgr.approve_secret(&req_task_id, &key, ttl).is_ok() {
                                    let _ = mgr.deliver_secret(&req_task_id, key);
                                }
                            } else {
                                let _ = mgr.deny_secret(&req_task_id);
                            }
                        }
                    }

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
                    // Show memory directory path for agents with memory enabled.
                    if let Some(def) = mgr.agents_def(id)
                        && let Some(scope) = def.memory
                        && let Ok(dir) =
                            crate::subagent::memory::resolve_memory_dir(scope, &def.name)
                    {
                        let _ = writeln!(out, "       memory: {}", dir.display());
                    }
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
            AgentCommand::Resume { id, prompt } => {
                let cfg = self.subagent_config.clone();
                // Resolve definition name from transcript meta before spawning so we can
                // look up skills by definition name rather than the UUID prefix (S1 fix).
                let def_name = {
                    let mgr = self.subagent_manager.as_ref()?;
                    match mgr.def_name_for_resume(&id, &cfg) {
                        Ok(name) => name,
                        Err(e) => return Some(format!("Failed to resume sub-agent: {e}")),
                    }
                };
                let skills = self.filtered_skills_for(&def_name);
                let provider = self.provider.clone();
                let tool_executor = Arc::clone(&self.tool_executor);
                let mgr = self.subagent_manager.as_mut()?;
                let (task_id, _) =
                    match mgr.resume(&id, &prompt, provider, tool_executor, skills, &cfg) {
                        Ok(pair) => pair,
                        Err(e) => return Some(format!("Failed to resume sub-agent: {e}")),
                    };
                let short = task_id[..8.min(task_id.len())].to_owned();
                let _ = self
                    .channel
                    .send(&format!("Resuming sub-agent '{id}'... (new id: {short})"))
                    .await;
                // Poll until the sub-agent reaches a terminal state (same as Spawn).
                let result = loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                    #[allow(clippy::redundant_closure_for_method_calls)]
                    let pending = self
                        .subagent_manager
                        .as_mut()
                        .and_then(|m| m.try_recv_secret_request());
                    if let Some((req_task_id, req)) = pending {
                        let confirm_prompt =
                            format!("Sub-agent requests secret '{}'. Allow?", req.secret_key);
                        let approved = self.channel.confirm(&confirm_prompt).await.unwrap_or(false);
                        if let Some(mgr) = self.subagent_manager.as_mut() {
                            if approved {
                                let ttl = std::time::Duration::from_secs(300);
                                let key = req.secret_key.clone();
                                if mgr.approve_secret(&req_task_id, &key, ttl).is_ok() {
                                    let _ = mgr.deliver_secret(&req_task_id, key);
                                }
                            } else {
                                let _ = mgr.deny_secret(&req_task_id);
                            }
                        }
                    }

                    let mgr = self.subagent_manager.as_ref()?;
                    let statuses = mgr.statuses();
                    let Some((_, status)) = statuses.iter().find(|(tid, _)| tid == &task_id) else {
                        break "Sub-agent resume completed (no status available).".to_owned();
                    };
                    match status.state {
                        SubAgentState::Completed => {
                            let msg = status.last_message.clone().unwrap_or_else(|| "done".into());
                            break format!("Resumed sub-agent completed: {msg}");
                        }
                        SubAgentState::Failed => {
                            let msg = status
                                .last_message
                                .clone()
                                .unwrap_or_else(|| "unknown error".into());
                            break format!("Resumed sub-agent failed: {msg}");
                        }
                        SubAgentState::Canceled => {
                            break "Resumed sub-agent was cancelled.".to_owned();
                        }
                        _ => {
                            let _ = self
                                .channel
                                .send_status(&format!(
                                    "resumed sub-agent: turn {}/{}",
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
        }
    }

    fn filtered_skills_for(&self, agent_name: &str) -> Option<Vec<String>> {
        let mgr = self.subagent_manager.as_ref()?;
        let def = mgr.definitions().iter().find(|d| d.name == agent_name)?;
        let reg = self
            .skill_state
            .registry
            .read()
            .expect("registry read lock");
        match crate::subagent::filter_skills(&reg, &def.skills) {
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

    #[allow(clippy::too_many_lines)]
    async fn reload_skills(&mut self) {
        let new_registry = SkillRegistry::load(&self.skill_state.skill_paths);
        if new_registry.fingerprint()
            == self
                .skill_state
                .registry
                .read()
                .expect("registry read lock")
                .fingerprint()
        {
            return;
        }
        let _ = self.channel.send_status("reloading skills...").await;
        *self
            .skill_state
            .registry
            .write()
            .expect("registry write lock") = new_registry;

        let all_meta = self
            .skill_state
            .registry
            .read()
            .expect("registry read lock")
            .all_meta()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();

        // Update trust DB records for reloaded skills.
        if let Some(ref memory) = self.memory_state.memory {
            let trust_cfg = self.skill_state.trust_config.clone();
            let managed_dir = self.skill_state.managed_dir.clone();
            for meta in &all_meta {
                let source_kind = if managed_dir
                    .as_ref()
                    .is_some_and(|d| meta.skill_dir.starts_with(d))
                {
                    zeph_memory::sqlite::SourceKind::Hub
                } else {
                    zeph_memory::sqlite::SourceKind::Local
                };
                let initial_level = if matches!(source_kind, zeph_memory::sqlite::SourceKind::Hub) {
                    &trust_cfg.default_level
                } else {
                    &trust_cfg.local_level
                };
                match zeph_skills::compute_skill_hash(&meta.skill_dir) {
                    Ok(current_hash) => {
                        let existing = memory
                            .sqlite()
                            .load_skill_trust(&meta.name)
                            .await
                            .ok()
                            .flatten();
                        let trust_level_str = if let Some(ref row) = existing {
                            if row.blake3_hash == current_hash {
                                row.trust_level.clone()
                            } else {
                                trust_cfg.hash_mismatch_level.to_string()
                            }
                        } else {
                            initial_level.to_string()
                        };
                        let source_path = meta.skill_dir.to_str();
                        if let Err(e) = memory
                            .sqlite()
                            .upsert_skill_trust(
                                &meta.name,
                                &trust_level_str,
                                source_kind,
                                None,
                                source_path,
                                &current_hash,
                            )
                            .await
                        {
                            tracing::warn!("failed to record trust for '{}': {e:#}", meta.name);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to compute hash for '{}': {e:#}", meta.name);
                    }
                }
            }
        }

        let all_meta = all_meta.iter().collect::<Vec<_>>();
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

        let all_skills: Vec<Skill> = {
            let reg = self
                .skill_state
                .registry
                .read()
                .expect("registry read lock");
            reg.all_meta()
                .iter()
                .filter_map(|m| reg.get_skill(&m.name).ok())
                .collect()
        };
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
            self.skill_state
                .registry
                .read()
                .expect("registry read lock")
                .all_meta()
                .len()
        );
    }

    fn reload_instructions(&mut self) {
        // Drain any additional queued events before reloading to avoid redundant reloads.
        if let Some(ref mut rx) = self.instruction_reload_rx {
            while rx.try_recv().is_ok() {}
        }
        let Some(ref state) = self.instruction_reload_state else {
            return;
        };
        let new_blocks = crate::instructions::load_instructions(
            &state.base_dir,
            &state.provider_kinds,
            &state.explicit_files,
            state.auto_detect,
        );
        let old_sources: std::collections::HashSet<_> =
            self.instruction_blocks.iter().map(|b| &b.source).collect();
        let new_sources: std::collections::HashSet<_> =
            new_blocks.iter().map(|b| &b.source).collect();
        for added in new_sources.difference(&old_sources) {
            tracing::info!(path = %added.display(), "instruction file added");
        }
        for removed in old_sources.difference(&new_sources) {
            tracing::info!(path = %removed.display(), "instruction file removed");
        }
        tracing::info!(
            old_count = self.instruction_blocks.len(),
            new_count = new_blocks.len(),
            "reloaded instruction files"
        );
        self.instruction_blocks = new_blocks;
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
            self.context_manager.budget = Some(
                ContextBudget::new(config.memory.context_budget_tokens, 0.20)
                    .with_graph_enabled(config.memory.graph.enabled),
            );
        } else {
            self.context_manager.budget = None;
        }

        {
            self.memory_state.graph_config = config.memory.graph.clone();
        }
        self.context_manager.compaction_threshold = config.memory.compaction_threshold;
        self.context_manager.compaction_preserve_tail = config.memory.compaction_preserve_tail;
        self.context_manager.prune_protect_tokens = config.memory.prune_protect_tokens;
        self.context_manager.compression = config.memory.compression.clone();
        self.context_manager.routing = config.memory.routing.clone();
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

    pub(crate) fn mock_provider_with_models(
        responses: Vec<String>,
        models: Vec<zeph_llm::model_cache::RemoteModelInfo>,
    ) -> AnyProvider {
        AnyProvider::Mock(MockProvider::with_responses(responses).with_models(models))
    }

    pub(crate) struct MockChannel {
        messages: Arc<Mutex<Vec<String>>>,
        sent: Arc<Mutex<Vec<String>>>,
        chunks: Arc<Mutex<Vec<String>>>,
        confirmations: Arc<Mutex<Vec<bool>>>,
        pub(crate) statuses: Arc<Mutex<Vec<String>>>,
        exit_supported: bool,
    }

    impl MockChannel {
        pub(crate) fn new(messages: Vec<String>) -> Self {
            Self {
                messages: Arc::new(Mutex::new(messages)),
                sent: Arc::new(Mutex::new(Vec::new())),
                chunks: Arc::new(Mutex::new(Vec::new())),
                confirmations: Arc::new(Mutex::new(Vec::new())),
                statuses: Arc::new(Mutex::new(Vec::new())),
                exit_supported: true,
            }
        }

        pub(crate) fn without_exit_support(mut self) -> Self {
            self.exit_supported = false;
            self
        }

        pub(crate) fn with_confirmations(mut self, confirmations: Vec<bool>) -> Self {
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

        fn supports_exit(&self) -> bool {
            self.exit_supported
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
        // Notice must contain the absolute path and byte count
        let notice_start = result.find("full output saved to").unwrap();
        let notice_part = &result[notice_start..];
        assert!(notice_part.contains(".txt"));
        assert!(notice_part.contains(std::path::MAIN_SEPARATOR));
        assert!(notice_part.contains("bytes"));
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

        // Must exceed overflow threshold (1000) so that truncate_tool_output_at produces
        // the "truncated" marker. MAX_TOOL_OUTPUT_CHARS is no longer used in this path.
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

        let result = agent.handle_image_command("../../etc/passwd").await;
        assert!(result.is_ok());
        assert!(agent.pending_image_parts.is_empty());
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

        let result = agent.handle_image_command("/nonexistent/image.png").await;
        assert!(result.is_ok());
        assert!(agent.pending_image_parts.is_empty());
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

        let result = agent.handle_image_command(&path).await;
        assert!(result.is_ok());
        assert_eq!(agent.pending_image_parts.len(), 1);
        match &agent.pending_image_parts[0] {
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
        use crate::subagent::hooks::SubagentHooks;
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
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: "You are helpful.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
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

    #[tokio::test]
    async fn model_command_valid_model_accepted() {
        // Ensure cache is stale so the handler falls back to list_models_remote().
        zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();

        let models = vec![
            zeph_llm::model_cache::RemoteModelInfo {
                id: "llama3:8b".to_string(),
                display_name: "Llama 3 8B".to_string(),
                context_window: Some(8192),
                created_at: None,
            },
            zeph_llm::model_cache::RemoteModelInfo {
                id: "qwen3:8b".to_string(),
                display_name: "Qwen3 8B".to_string(),
                context_window: Some(32768),
                created_at: None,
            },
        ];
        let provider = mock_provider_with_models(vec![], models);
        let channel = MockChannel::new(vec![]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.handle_model_command("/model llama3:8b").await;

        let messages = sent.lock().unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("Switched to model: llama3:8b")),
            "expected switch confirmation, got: {messages:?}"
        );
        assert!(
            !messages.iter().any(|m| m.contains("Unknown model")),
            "unexpected rejection for valid model, got: {messages:?}"
        );
    }

    #[tokio::test]
    async fn model_command_invalid_model_rejected() {
        // Ensure cache is stale so the handler falls back to list_models_remote().
        zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();

        let models = vec![zeph_llm::model_cache::RemoteModelInfo {
            id: "qwen3:8b".to_string(),
            display_name: "Qwen3 8B".to_string(),
            context_window: None,
            created_at: None,
        }];
        let provider = mock_provider_with_models(vec![], models);
        let channel = MockChannel::new(vec![]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.handle_model_command("/model nonexistent-model").await;

        let messages = sent.lock().unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("Unknown model") && m.contains("nonexistent-model")),
            "expected rejection with model name, got: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("qwen3:8b")),
            "expected available models list, got: {messages:?}"
        );
        assert!(
            !messages.iter().any(|m| m.contains("Switched to model")),
            "should not switch to invalid model, got: {messages:?}"
        );
    }

    #[tokio::test]
    async fn model_command_empty_model_list_warns_and_proceeds() {
        // Ensure cache is stale so the handler falls back to list_models_remote().
        // MockProvider returns empty vec → warning shown, switch proceeds.
        zeph_llm::model_cache::ModelCache::for_slug("mock").invalidate();

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.handle_model_command("/model unknown-model").await;

        let messages = sent.lock().unwrap();
        assert!(
            messages.iter().any(|m| m.contains("unavailable")),
            "expected warning about unavailable model list, got: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("Switched to model: unknown-model")),
            "expected switch to proceed despite missing model list, got: {messages:?}"
        );
    }

    #[tokio::test]
    async fn help_command_lists_commands() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/help".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        assert!(!messages.is_empty(), "expected /help output");
        let output = messages.join("\n");
        assert!(output.contains("/help"), "expected /help in output");
        assert!(output.contains("/exit"), "expected /exit in output");
        assert!(output.contains("/status"), "expected /status in output");
        assert!(output.contains("/skills"), "expected /skills in output");
        assert!(output.contains("/model"), "expected /model in output");
    }

    #[tokio::test]
    async fn help_command_does_not_include_unknown_commands() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/help".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        let output = messages.join("\n");
        // /ingest does not exist in the codebase — must not appear
        assert!(
            !output.contains("/ingest"),
            "unexpected /ingest in /help output"
        );
    }

    #[tokio::test]
    async fn status_command_includes_provider_and_model() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/status".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        assert!(!messages.is_empty(), "expected /status output");
        let output = messages.join("\n");
        assert!(output.contains("Provider:"), "expected Provider: field");
        assert!(output.contains("Model:"), "expected Model: field");
        assert!(output.contains("Uptime:"), "expected Uptime: field");
        assert!(output.contains("Tokens:"), "expected Tokens: field");
    }

    // Regression test for #1415: MetricsCollector must be wired in CLI mode (no TUI).
    // Before the fix, metrics_tx was None in non-TUI mode so /status always showed zeros.
    #[tokio::test]
    async fn status_command_shows_metrics_in_cli_mode() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/status".to_string()]);
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (tx, _rx) = watch::channel(MetricsSnapshot::default());
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_metrics(tx);

        // Simulate metrics that would be populated by a real LLM call.
        agent.update_metrics(|m| {
            m.api_calls = 3;
            m.prompt_tokens = 100;
            m.completion_tokens = 50;
        });

        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        let output = messages.join("\n");
        assert!(
            output.contains("API calls: 3"),
            "expected non-zero api_calls in /status output; got: {output}"
        );
        assert!(
            output.contains("100 prompt / 50 completion"),
            "expected non-zero tokens in /status output; got: {output}"
        );
    }

    #[tokio::test]
    async fn exit_command_breaks_run_loop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/exit".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());
        // /exit should not produce any LLM message — only system message in history
        assert_eq!(agent.messages.len(), 1, "expected only system message");
    }

    #[tokio::test]
    async fn quit_command_breaks_run_loop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/quit".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());
        assert_eq!(agent.messages.len(), 1, "expected only system message");
    }

    #[tokio::test]
    async fn exit_command_sends_info_and_continues_when_not_supported() {
        let provider = mock_provider(vec![]);
        // Channel that does not support exit: /exit should NOT break the loop,
        // it should send an info message and then yield the next message.
        let channel = MockChannel::new(vec![
            "/exit".to_string(),
            // second message is empty → causes recv() to return None → loop exits naturally
        ])
        .without_exit_support();
        let sent = channel.sent.clone();
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let result = agent.run().await;
        assert!(result.is_ok());

        let messages = sent.lock().unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("/exit is not supported")),
            "expected info message, got: {messages:?}"
        );
    }

    #[test]
    fn slash_commands_registry_has_no_ingest() {
        use super::slash_commands::COMMANDS;
        assert!(
            !COMMANDS.iter().any(|c| c.name == "/ingest"),
            "/ingest is not implemented and must not appear in COMMANDS"
        );
    }

    #[test]
    fn slash_commands_graph_and_plan_have_no_feature_gate() {
        use super::slash_commands::COMMANDS;
        for cmd in COMMANDS {
            if cmd.name == "/graph" || cmd.name == "/plan" {
                assert!(
                    cmd.feature_gate.is_none(),
                    "{} should have feature_gate: None",
                    cmd.name
                );
            }
        }
    }

    // Regression tests for issue #1418: bare slash commands must not fall through to LLM.

    #[tokio::test]
    async fn bare_skill_command_does_not_invoke_llm() {
        // Provider has no responses — if LLM is called the agent would receive an empty response
        // and send "empty response" to the channel. The handler should return before reaching LLM.
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/skill".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent = agent.channel.sent_messages();
        // Handler sends the "Unknown /skill subcommand" usage message — not an LLM response.
        assert!(
            sent.iter().any(|m| m.contains("Unknown /skill subcommand")),
            "bare /skill must send usage; got: {sent:?}"
        );
        // No assistant message should be added to history (LLM was not called).
        assert!(
            agent.messages.iter().all(|m| m.role != Role::Assistant),
            "bare /skill must not produce an assistant message; messages: {:?}",
            agent.messages
        );
    }

    #[tokio::test]
    async fn bare_feedback_command_does_not_invoke_llm() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/feedback".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|m| m.contains("Usage: /feedback")),
            "bare /feedback must send usage; got: {sent:?}"
        );
        assert!(
            agent.messages.iter().all(|m| m.role != Role::Assistant),
            "bare /feedback must not produce an assistant message; messages: {:?}",
            agent.messages
        );
    }

    #[tokio::test]
    async fn bare_image_command_sends_usage() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec!["/image".to_string()]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.run().await;
        assert!(result.is_ok());

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|m| m.contains("Usage: /image <path>")),
            "bare /image must send usage; got: {sent:?}"
        );
        assert!(
            agent.messages.iter().all(|m| m.role != Role::Assistant),
            "bare /image must not produce an assistant message; messages: {:?}",
            agent.messages
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

    /// E2E test: spawn sub-agent in background, verify it runs and produces output.
    ///
    /// Scope: spawn → text response → collect (MockProvider only supports text responses).
    #[tokio::test]
    async fn subagent_spawn_text_collect_e2e() {
        use crate::subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use crate::subagent::hooks::SubagentHooks;
        use crate::subagent::{AgentCommand, SubAgentDef, SubAgentManager};

        // Provider shared between main agent and sub-agent via Arc clone.
        // We pre-load a response that the sub-agent loop will consume.
        let provider = mock_provider(vec!["task completed successfully".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "worker".into(),
            description: "A worker bot".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions {
                max_turns: 1,
                ..SubAgentPermissions::default()
            },
            skills: SkillFilter::default(),
            system_prompt: "You are a worker.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        });
        agent.subagent_manager = Some(mgr);

        // Spawn the sub-agent in background — returns immediately with the task id.
        let spawn_resp = agent
            .handle_agent_command(AgentCommand::Background {
                name: "worker".into(),
                prompt: "do a task".into(),
            })
            .await
            .expect("Background spawn must return Some");
        assert!(
            spawn_resp.contains("worker"),
            "spawn response must mention agent name; got: {spawn_resp}"
        );
        assert!(
            spawn_resp.contains("started"),
            "spawn response must confirm start; got: {spawn_resp}"
        );

        // Extract the short id from response: "Sub-agent 'worker' started in background (id: XXXXXXXX)"
        let short_id = spawn_resp
            .split("id: ")
            .nth(1)
            .expect("response must contain 'id: '")
            .trim_end_matches(')')
            .trim()
            .to_string();
        assert!(!short_id.is_empty(), "short_id must not be empty");

        // Poll until the sub-agent reaches a terminal state (max 5s).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let full_id = loop {
            let mgr = agent.subagent_manager.as_ref().unwrap();
            let statuses = mgr.statuses();
            let found = statuses.iter().find(|(id, _)| id.starts_with(&short_id));
            if let Some((id, status)) = found {
                match status.state {
                    crate::subagent::SubAgentState::Completed => break id.clone(),
                    crate::subagent::SubAgentState::Failed => {
                        panic!(
                            "sub-agent reached Failed state unexpectedly: {:?}",
                            status.last_message
                        );
                    }
                    _ => {}
                }
            }
            if std::time::Instant::now() > deadline {
                panic!("sub-agent did not complete within timeout");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };

        // Collect result and verify output.
        let result = agent
            .subagent_manager
            .as_mut()
            .unwrap()
            .collect(&full_id)
            .await
            .expect("collect must succeed for completed sub-agent");
        assert!(
            result.contains("task completed successfully"),
            "collected result must contain sub-agent output; got: {result:?}"
        );
    }

    /// Unit test for secret bridge in foreground spawn poll loop.
    ///
    /// Verifies that when a sub-agent emits [REQUEST_SECRET: api-key], the bridge:
    /// - calls channel.confirm() with a prompt containing the key name
    /// - on approval, delivers the secret to the sub-agent
    /// The MockChannel confirm() is pre-loaded with `true` (approve).
    #[tokio::test]
    async fn foreground_spawn_secret_bridge_approves() {
        use crate::subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use crate::subagent::hooks::SubagentHooks;
        use crate::subagent::{AgentCommand, SubAgentDef, SubAgentManager};

        // Sub-agent loop responses:
        //   turn 1: request a secret
        //   turn 2: final reply after secret delivered
        let provider = mock_provider(vec![
            "[REQUEST_SECRET: api-key]".into(),
            "done with secret".into(),
        ]);

        // MockChannel with confirm() → true (approve)
        let channel = MockChannel::new(vec![]).with_confirmations(vec![true]);

        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "vault-bot".into(),
            description: "A bot that requests secrets".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions {
                max_turns: 2,
                secrets: vec!["api-key".into()],
                ..SubAgentPermissions::default()
            },
            skills: SkillFilter::default(),
            system_prompt: "You need a secret.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        });
        agent.subagent_manager = Some(mgr);

        // Foreground spawn — blocks until sub-agent completes.
        let resp: String = agent
            .handle_agent_command(AgentCommand::Spawn {
                name: "vault-bot".into(),
                prompt: "fetch the api key".into(),
            })
            .await
            .expect("Spawn must return Some");

        // Sub-agent completed after secret was bridged (approve path).
        // The sub-agent had 2 turns: turn 1 = secret request, turn 2 = final reply.
        // If the bridge did NOT call confirm(), the sub-agent would never get the
        // approval outcome and the foreground poll loop would stall or time out.
        // Reaching this point proves the bridge ran and confirm() was called.
        assert!(
            resp.contains("vault-bot"),
            "response must mention agent name; got: {resp}"
        );
        assert!(
            resp.contains("completed"),
            "sub-agent must complete successfully; got: {resp}"
        );
    }

    // ── /plan handler unit tests ─────────────────────────────────────────────

    use crate::orchestration::{
        GraphStatus, PlanCommand, TaskGraph, TaskNode, TaskResult, TaskStatus,
    };

    fn agent_with_orchestration() -> Agent<MockChannel> {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration_config.enabled = true;
        agent
    }

    fn make_simple_graph(status: GraphStatus) -> TaskGraph {
        let mut g = TaskGraph::new("test goal");
        let mut node = TaskNode::new(0, "task-0", "do something");
        node.status = match status {
            GraphStatus::Created => TaskStatus::Pending,
            GraphStatus::Running => TaskStatus::Ready,
            _ => TaskStatus::Completed,
        };
        if status == GraphStatus::Running || status == GraphStatus::Completed {
            node.result = Some(TaskResult {
                output: "done".into(),
                artifacts: vec![],
                duration_ms: 0,
                agent_id: None,
                agent_def: None,
            });
            if status == GraphStatus::Completed {
                node.status = TaskStatus::Completed;
            }
        }
        g.tasks.push(node);
        g.status = status;
        g
    }

    /// GAP-1: handle_plan_confirm with subagent_manager = None → fallback message,
    /// graph restored in pending_graph.
    #[tokio::test]
    async fn plan_confirm_no_manager_restores_graph() {
        let mut agent = agent_with_orchestration();

        let graph = make_simple_graph(GraphStatus::Created);
        agent.pending_graph = Some(graph);

        // No subagent_manager set.
        agent
            .handle_plan_command(PlanCommand::Confirm)
            .await
            .unwrap();

        // Graph must be restored.
        assert!(
            agent.pending_graph.is_some(),
            "graph must be restored when no manager configured"
        );
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|m| m.contains("sub-agent")),
            "must send fallback message; got: {msgs:?}"
        );
    }

    /// GAP-2: handle_plan_confirm with pending_graph = None → "No pending plan" message.
    #[tokio::test]
    async fn plan_confirm_no_pending_graph_sends_message() {
        let mut agent = agent_with_orchestration();

        // No pending_graph.
        agent
            .handle_plan_command(PlanCommand::Confirm)
            .await
            .unwrap();

        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|m| m.contains("No pending plan")),
            "must send 'No pending plan' message; got: {msgs:?}"
        );
    }

    /// GAP-3: happy path — pre-built Running graph with one already-Completed task.
    /// resume_from() accepts it; first tick() emits Done{Completed}; aggregation called.
    #[tokio::test]
    async fn plan_confirm_completed_graph_aggregates() {
        use crate::subagent::def::{SkillFilter, SubAgentPermissions, ToolPolicy};
        use crate::subagent::hooks::SubagentHooks;
        use crate::subagent::{SubAgentDef, SubAgentManager};

        // MockProvider returns the aggregation synthesis.
        let provider = mock_provider(vec!["synthesis result".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration_config.enabled = true;

        let mut mgr = SubAgentManager::new(4);
        mgr.definitions_mut().push(SubAgentDef {
            name: "worker".into(),
            description: "A worker".into(),
            model: None,
            tools: ToolPolicy::InheritAll,
            disallowed_tools: vec![],
            permissions: SubAgentPermissions::default(),
            skills: SkillFilter::default(),
            system_prompt: "You are helpful.".into(),
            hooks: SubagentHooks::default(),
            memory: None,
            source: None,
            file_path: None,
        });
        agent.subagent_manager = Some(mgr);

        // Graph with one already-Completed task in Running status: resume_from() accepts it,
        // and the first tick() will find no running/ready tasks → Done{Completed}.
        let mut graph = TaskGraph::new("test goal");
        let mut node = TaskNode::new(0, "task-0", "already done");
        node.status = TaskStatus::Completed;
        node.result = Some(TaskResult {
            output: "task output".into(),
            artifacts: vec![],
            duration_ms: 10,
            agent_id: None,
            agent_def: None,
        });
        graph.tasks.push(node);
        graph.status = GraphStatus::Running;
        agent.pending_graph = Some(graph);

        agent
            .handle_plan_command(PlanCommand::Confirm)
            .await
            .unwrap();

        let msgs = agent.channel.sent_messages();
        // Aggregation synthesis should appear in messages.
        assert!(
            msgs.iter().any(|m| m.contains("synthesis result")),
            "aggregation synthesis must be sent to user; got: {msgs:?}"
        );
        // Graph must be cleared after successful completion.
        assert!(
            agent.pending_graph.is_none(),
            "pending_graph must be cleared after Completed"
        );
    }

    /// GAP-4: handle_plan_confirm with a Paused graph → pending_graph gets restored,
    /// paused message sent to user.
    ///
    /// A graph in Paused status: resume_from() sets status=Running; the first tick()
    /// finds no Running, no Ready tasks (all Pending with unmet deps in a cycle that
    /// reset_for_retry would fix) → deadlock → Done{Failed}. We build the simplest
    /// case: a graph where the only task is Skipped (terminal but not Completed),
    /// meaning all tasks are terminal → Done{Completed}. Instead we use a Paused
    /// graph directly via finalize_plan_execution logic — but the Paused signal comes
    /// from the scheduler's `FailureStrategy::Ask` path which we cannot easily trigger
    /// with a mock.
    ///
    /// So we test a simpler invariant: a graph with `status=Paused` passed to
    /// `handle_plan_confirm` via `resume_from` → becomes Running → first tick finds
    /// a Ready task that cannot be spawned (no matching agent) → record_spawn_failure
    /// → if the graph has `FailureStrategy::Abort`, Done{Failed}. We verify the graph
    /// is restored and a failure message is sent.
    #[tokio::test]
    async fn plan_confirm_spawn_failure_restores_pending() {
        use crate::subagent::SubAgentManager;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        agent.orchestration_config.enabled = true;

        // Manager with no defined agents → spawn_for_task returns NotFound.
        agent.subagent_manager = Some(SubAgentManager::new(4));

        // Graph in Created status with one task; scheduler will try to spawn it,
        // fail (no agents), record_spawn_failure → Done{Failed}.
        let mut graph = TaskGraph::new("failing goal");
        let node = TaskNode::new(0, "task-0", "cannot be spawned");
        graph.tasks.push(node);
        graph.status = GraphStatus::Created;
        agent.pending_graph = Some(graph);

        agent
            .handle_plan_command(PlanCommand::Confirm)
            .await
            .unwrap();

        // Graph must be restored for /plan retry.
        assert!(
            agent.pending_graph.is_some(),
            "pending_graph must be restored after Failed so /plan retry works"
        );
        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|m| m.contains("failed")),
            "failure message must be sent; got: {msgs:?}"
        );
    }

    /// GAP-5: handle_plan_list with pending_graph → shows summary + status label.
    #[tokio::test]
    async fn plan_list_with_pending_graph_shows_summary() {
        let mut agent = agent_with_orchestration();

        agent.pending_graph = Some(make_simple_graph(GraphStatus::Created));

        agent.handle_plan_command(PlanCommand::List).await.unwrap();

        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|m| m.contains("awaiting confirmation")),
            "must show 'awaiting confirmation' status; got: {msgs:?}"
        );
    }

    /// GAP-6: handle_plan_list with no graph → "No recent plans."
    #[tokio::test]
    async fn plan_list_no_graph_shows_no_recent() {
        let mut agent = agent_with_orchestration();

        agent.handle_plan_command(PlanCommand::List).await.unwrap();

        let msgs = agent.channel.sent_messages();
        assert!(
            msgs.iter().any(|m| m.contains("No recent plans")),
            "must show 'No recent plans'; got: {msgs:?}"
        );
    }

    /// GAP-7: handle_plan_retry resets Running tasks to Ready and clears assigned_agent.
    #[tokio::test]
    async fn plan_retry_resets_running_tasks_to_ready() {
        let mut agent = agent_with_orchestration();

        let mut graph = TaskGraph::new("retry test");
        let mut failed = TaskNode::new(0, "failed-task", "desc");
        failed.status = TaskStatus::Failed;
        let mut stale_running = TaskNode::new(1, "stale-task", "desc");
        stale_running.status = TaskStatus::Running;
        stale_running.assigned_agent = Some("old-handle-id".into());
        graph.tasks.push(failed);
        graph.tasks.push(stale_running);
        graph.status = GraphStatus::Failed;
        agent.pending_graph = Some(graph);

        agent
            .handle_plan_command(PlanCommand::Retry(None))
            .await
            .unwrap();

        let g = agent
            .pending_graph
            .as_ref()
            .expect("graph must be present after retry");

        // Failed task must be reset to Ready.
        assert_eq!(
            g.tasks[0].status,
            TaskStatus::Ready,
            "failed task must be reset to Ready"
        );

        // Stale Running task must be reset to Ready and assigned_agent cleared.
        assert_eq!(
            g.tasks[1].status,
            TaskStatus::Ready,
            "stale Running task must be reset to Ready"
        );
        assert!(
            g.tasks[1].assigned_agent.is_none(),
            "assigned_agent must be cleared for stale Running task"
        );
    }
}
