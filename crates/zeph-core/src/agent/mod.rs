// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod accessors;
mod builder;
pub(crate) mod compaction_strategy;
#[cfg(feature = "compression-guidelines")]
pub(super) mod compression_feedback;
mod context;
pub(crate) mod context_manager;
pub mod error;
#[cfg(feature = "experiments")]
mod experiment_cmd;
pub(super) mod feedback_detector;
pub(crate) mod focus;
mod graph_commands;
#[cfg(feature = "compression-guidelines")]
mod guidelines_commands;
mod index;
mod learning;
pub(crate) mod learning_engine;
mod log_commands;
#[cfg(feature = "lsp-context")]
mod lsp_commands;
mod mcp;
mod memory_commands;
mod message_queue;
mod persistence;
#[cfg(feature = "policy-enforcer")]
mod policy_commands;
mod provider_cmd;
pub(crate) mod rate_limiter;
#[cfg(feature = "scheduler")]
mod scheduler_commands;
pub mod session_config;
mod session_digest;
pub(crate) mod sidequest;
mod skill_management;
pub mod slash_commands;
pub(crate) mod state;
pub(crate) mod tool_execution;
pub(crate) mod tool_orchestrator;
mod trust_commands;
mod utils;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, Role};
use zeph_memory::TokenCounter;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::loader::Skill;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::prompt::format_skills_prompt;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ErasedToolExecutor, ToolExecutor};

use crate::channel::Channel;
use crate::config::Config;
use crate::config::{SecurityConfig, SkillPromptMode, TimeoutConfig};
use crate::context::{
    ContextBudget, EnvironmentContext, build_system_prompt, build_system_prompt_with_instructions,
};
use zeph_sanitizer::ContentSanitizer;

use message_queue::{MAX_AUDIO_BYTES, MAX_IMAGE_BYTES, detect_image_mime};
#[cfg(feature = "context-compression")]
use state::CompressionState;
use state::{
    DebugState, ExperimentState, FeedbackState, IndexState, InstructionState, LifecycleState,
    McpState, MemoryState, MessageState, MetricsState, OrchestrationState, ProviderState,
    RuntimeConfig, SecurityState, SessionState, SkillState,
};

pub(crate) const DOOM_LOOP_WINDOW: usize = 3;
pub(crate) const DOCUMENT_RAG_PREFIX: &str = "## Relevant documents\n";
pub(crate) const RECALL_PREFIX: &str = "[semantic recall]\n";
pub(crate) const CODE_CONTEXT_PREFIX: &str = "[code context]\n";
pub(crate) const SUMMARY_PREFIX: &str = "[conversation summaries]\n";
pub(crate) const CROSS_SESSION_PREFIX: &str = "[cross-session context]\n";
pub(crate) const CORRECTIONS_PREFIX: &str = "[past corrections]\n";
pub(crate) const GRAPH_FACTS_PREFIX: &str = "[known facts]\n";
pub(crate) const SCHEDULED_TASK_PREFIX: &str = "Execute the following scheduled task now: ";
pub(crate) const SESSION_DIGEST_PREFIX: &str = "[Session digest from previous interaction]\n";
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

/// Concatenate completed task outputs into a single string truncated to `max_tokens * 4` chars.
///
/// Logs a warning when truncation occurs (C3). Returns an empty string when no completed
/// tasks have results.
fn collect_and_truncate_task_outputs(
    graph: &crate::orchestration::TaskGraph,
    max_tokens: u32,
) -> String {
    use crate::orchestration::TaskStatus;

    let char_budget = max_tokens as usize * 4;
    let mut raw = String::new();
    for task in &graph.tasks {
        if task.status == TaskStatus::Completed
            && let Some(ref result) = task.result
        {
            if !raw.is_empty() {
                raw.push('\n');
            }
            raw.push_str(&result.output);
        }
    }
    if raw.len() > char_budget {
        tracing::warn!(
            original_len = raw.len(),
            truncated_to = char_budget,
            "whole-plan verify: output truncated to verify_max_tokens * 4 chars"
        );
        raw.chars().take(char_budget).collect()
    } else {
        raw
    }
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

pub struct Agent<C: Channel> {
    provider: AnyProvider,
    /// Dedicated embedding provider. Resolved once at bootstrap from `[[llm.providers]]`
    /// (the entry with `embed = true`, or first entry with `embedding_model` set).
    /// Falls back to `provider.clone()` when no dedicated entry exists.
    /// **Never replaced** by `/provider switch`.
    embedding_provider: AnyProvider,
    channel: C,
    pub(crate) tool_executor: Arc<dyn ErasedToolExecutor>,
    pub(super) msg: MessageState,
    pub(super) memory_state: MemoryState,
    pub(super) skill_state: SkillState,
    pub(super) context_manager: context_manager::ContextManager,
    pub(super) tool_orchestrator: tool_orchestrator::ToolOrchestrator,
    pub(super) learning_engine: learning_engine::LearningEngine,
    pub(super) feedback: FeedbackState,
    pub(super) runtime: RuntimeConfig,
    pub(super) mcp: McpState,
    pub(super) index: IndexState,
    pub(super) session: SessionState,
    pub(super) debug_state: DebugState,
    pub(super) instructions: InstructionState,
    pub(super) security: SecurityState,
    pub(super) experiments: ExperimentState,
    #[cfg(feature = "context-compression")]
    pub(super) compression: CompressionState,
    pub(super) lifecycle: LifecycleState,
    pub(super) providers: ProviderState,
    pub(super) metrics: MetricsState,
    pub(super) orchestration: OrchestrationState,
    /// Focus agent state: active session tracking, knowledge block, reminder counters (#1850).
    pub(super) focus: focus::FocusState,
    /// `SideQuest` state: cursor tracking, turn counter, eviction stats (#1885).
    pub(super) sidequest: sidequest::SidequestState,
    /// Dynamic tool schema filter: pre-computed tool embeddings for per-turn filtering (#2020).
    pub(super) tool_schema_filter: Option<zeph_tools::ToolSchemaFilter>,
    /// Cached filtered tool IDs for the current user turn. Set by `compute_filtered_tool_ids()`
    /// in `rebuild_system_prompt()`, consumed by the native tool loop on iteration 0.
    pub(super) cached_filtered_tool_ids: Option<HashSet<String>>,
    /// Tool dependency graph for sequential tool availability (issue #2024).
    /// Built once from config, applied per-turn after tool schema filtering.
    pub(super) dependency_graph: Option<zeph_tools::ToolDependencyGraph>,
    /// Always-on tool IDs, mirrored from the tool schema filter for dependency gate bypass.
    pub(super) dependency_always_on: HashSet<String>,
    /// Tool IDs that completed successfully in the current session.
    /// Grows monotonically per session; cleared on `/clear`.
    /// NOTE: bounded by session length, typically < 1000 entries.
    pub(super) completed_tool_ids: HashSet<String>,
    /// DB row ID of the most recently persisted message. Set by `persist_message`;
    /// consumed by `push_message` call sites to populate `metadata.db_id` on in-memory messages.
    pub(super) last_persisted_message_id: Option<i64>,
    /// DB message IDs pending hide after deferred tool pair summarization.
    pub(super) deferred_db_hide_ids: Vec<i64>,
    /// Summary texts pending insertion after deferred tool pair summarization.
    pub(super) deferred_db_summaries: Vec<String>,
    /// Runtime middleware layers for LLM calls and tool dispatch (#2286).
    ///
    /// Default: empty vec (zero-cost — loops never iterate).
    pub(super) runtime_layers: Vec<std::sync::Arc<dyn crate::runtime_layer::RuntimeLayer>>,
}

impl<C: Channel> Agent<C> {
    #[must_use]
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
    #[allow(clippy::too_many_lines)] // flat struct literal initializing all Agent sub-structs — one field per sub-struct, cannot be split further
    pub fn new_with_registry_arc(
        provider: AnyProvider,
        channel: C,
        registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
        matcher: Option<SkillMatcherBackend>,
        max_active_skills: usize,
        tool_executor: impl ToolExecutor + 'static,
    ) -> Self {
        debug_assert!(max_active_skills > 0, "max_active_skills must be > 0");
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
        let embedding_provider = provider.clone();
        Self {
            provider,
            embedding_provider,
            channel,
            tool_executor: Arc::new(tool_executor),
            msg: MessageState {
                messages: vec![Message {
                    role: Role::System,
                    content: system_prompt,
                    parts: vec![],
                    metadata: MessageMetadata::default(),
                }],
                message_queue: VecDeque::new(),
                pending_image_parts: Vec::new(),
            },
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
                compression_guidelines_config: zeph_memory::CompressionGuidelinesConfig::default(),
                shutdown_summary: true,
                shutdown_summary_min_messages: 4,
                shutdown_summary_max_messages: 20,
                shutdown_summary_timeout_secs: 10,
                structured_summaries: false,
                digest_config: crate::config::DigestConfig::default(),
                cached_session_digest: None,
                context_strategy: crate::config::ContextStrategy::default(),
                crossover_turn_threshold: 20,
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
            feedback: FeedbackState {
                detector: feedback_detector::FeedbackDetector::new(0.6),
                judge: None,
                llm_classifier: None,
            },
            debug_state: DebugState {
                debug_dumper: None,
                dump_format: crate::debug_dump::DumpFormat::default(),
                trace_collector: None,
                iteration_counter: 0,
                anomaly_detector: None,
                reasoning_model_warning: true,
                logging_config: crate::config::LoggingConfig::default(),
                dump_dir: None,
                trace_service_name: String::new(),
                trace_redact: true,
                current_iteration_span_id: None,
            },
            runtime: RuntimeConfig {
                security: SecurityConfig::default(),
                timeouts: TimeoutConfig::default(),
                model_name: String::new(),
                active_provider_name: String::new(),
                permission_policy: zeph_tools::PermissionPolicy::default(),
                redact_credentials: true,
                rate_limiter: rate_limiter::ToolRateLimiter::new(
                    rate_limiter::RateLimitConfig::default(),
                ),
                semantic_cache_enabled: false,
                semantic_cache_threshold: 0.95,
                semantic_cache_max_candidates: 10,
                dependency_config: zeph_tools::DependencyConfig::default(),
            },
            mcp: McpState {
                tools: Vec::new(),
                registry: None,
                manager: None,
                allowed_commands: Vec::new(),
                max_dynamic: 10,
                shared_tools: None,
                tool_rx: None,
                server_outcomes: Vec::new(),
                pruning_cache: zeph_mcp::PruningCache::new(),
                pruning_provider: None,
                pruning_enabled: false,
                pruning_params: zeph_mcp::PruningParams::default(),
                semantic_index: None,
                discovery_strategy: zeph_mcp::ToolDiscoveryStrategy::default(),
                discovery_params: zeph_mcp::DiscoveryParams::default(),
                discovery_provider: None,
            },
            index: IndexState {
                retriever: None,
                repo_map_tokens: 0,
                cached_repo_map: None,
                repo_map_ttl: std::time::Duration::from_secs(300),
            },
            session: SessionState {
                env_context: EnvironmentContext::gather(""),
                response_cache: None,
                parent_tool_use_id: None,
                status_tx: None,
                #[cfg(feature = "lsp-context")]
                lsp_hooks: None,
                #[cfg(feature = "policy-enforcer")]
                policy_config: None,
            },
            instructions: InstructionState {
                blocks: Vec::new(),
                reload_rx: None,
                reload_state: None,
            },
            security: SecurityState {
                sanitizer: ContentSanitizer::new(&zeph_sanitizer::ContentIsolationConfig::default()),
                quarantine_summarizer: None,
                exfiltration_guard: zeph_sanitizer::exfiltration::ExfiltrationGuard::new(
                    zeph_sanitizer::exfiltration::ExfiltrationGuardConfig::default(),
                ),
                flagged_urls: std::collections::HashSet::new(),
                user_provided_urls: std::sync::Arc::new(std::sync::RwLock::new(
                    std::collections::HashSet::new(),
                )),
                pii_filter: zeph_sanitizer::pii::PiiFilter::new(
                    zeph_sanitizer::pii::PiiFilterConfig::default(),
                ),
                #[cfg(feature = "classifiers")]
                pii_ner_backend: None,
                #[cfg(feature = "classifiers")]
                pii_ner_timeout_ms: 5000,
                memory_validator: zeph_sanitizer::memory_validation::MemoryWriteValidator::new(
                    zeph_sanitizer::memory_validation::MemoryWriteValidationConfig::default(),
                ),
                #[cfg(feature = "guardrail")]
                guardrail: None,
                response_verifier: zeph_sanitizer::response_verifier::ResponseVerifier::new(
                    zeph_config::ResponseVerificationConfig::default(),
                ),
            },
            experiments: ExperimentState {
                #[cfg(feature = "experiments")]
                config: crate::config::ExperimentConfig::default(),
                #[cfg(feature = "experiments")]
                cancel: None,
                #[cfg(feature = "experiments")]
                baseline: crate::experiments::ConfigSnapshot::default(),
                #[cfg(feature = "experiments")]
                eval_provider: None,
                notify_rx: Some(exp_notify_rx),
                #[cfg(feature = "experiments")]
                notify_tx: exp_notify_tx,
            },
            #[cfg(feature = "context-compression")]
            compression: CompressionState {
                current_task_goal: None,
                task_goal_user_msg_hash: None,
                pending_task_goal: None,
                pending_sidequest_result: None,
                subgoal_registry: crate::agent::compaction_strategy::SubgoalRegistry::default(),
                pending_subgoal: None,
                subgoal_user_msg_hash: None,
            },
            lifecycle: LifecycleState {
                shutdown: rx,
                start_time: Instant::now(),
                cancel_signal: Arc::new(Notify::new()),
                cancel_token: CancellationToken::new(),
                config_path: None,
                config_reload_rx: None,
                warmup_ready: None,
                update_notify_rx: None,
                custom_task_rx: None,
            },
            providers: ProviderState {
                summary_provider: None,
                provider_override: None,
                judge_provider: None,
                probe_provider: None,
                #[cfg(feature = "context-compression")]
                compress_provider: None,
                cached_prompt_tokens: initial_prompt_tokens,
                server_compaction_active: false,
                stt: None,
                provider_pool: Vec::new(),
                provider_config_snapshot: None,
            },
            metrics: MetricsState {
                metrics_tx: None,
                cost_tracker: None,
                token_counter,
                extended_context: false,
                classifier_metrics: None,
            },
            orchestration: OrchestrationState {
                planner_provider: None,
                verify_provider: None,
                pending_graph: None,
                plan_cancel_token: None,
                subagent_manager: None,
                subagent_config: crate::config::SubAgentConfig::default(),
                orchestration_config: crate::config::OrchestrationConfig::default(),
                plan_cache: None,
                pending_goal_embedding: None,
            },
            focus: focus::FocusState::default(),
            sidequest: sidequest::SidequestState::default(),
            tool_schema_filter: None,
            cached_filtered_tool_ids: None,
            dependency_graph: None,
            dependency_always_on: HashSet::new(),
            completed_tool_ids: HashSet::new(),
            last_persisted_message_id: None,
            deferred_db_hide_ids: Vec::new(),
            deferred_db_summaries: Vec::new(),
            runtime_layers: Vec::new(),
        }
    }

    /// Poll all active sub-agents for completed/failed/canceled results.
    ///
    /// Non-blocking: returns immediately with a list of `(task_id, result)` pairs
    /// for agents that have finished. Each completed agent is removed from the manager.
    pub async fn poll_subagents(&mut self) -> Vec<(String, String)> {
        let Some(mgr) = &mut self.orchestration.subagent_manager else {
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
        &self.orchestration.orchestration_config
    }

    /// Lazily initialize `OrchestrationState::plan_cache` on the first `/plan` call.
    ///
    /// No-op when the cache is already initialized, disabled in config, or memory is unavailable.
    async fn init_plan_cache_if_needed(&mut self) {
        let plan_cache_config = self.orchestration.orchestration_config.plan_cache.clone();
        if !plan_cache_config.enabled || self.orchestration.plan_cache.is_some() {
            return;
        }
        if let Some(ref memory) = self.memory_state.memory {
            let pool = memory.sqlite().pool().clone();
            let embed_model = self.skill_state.embedding_model.clone();
            match crate::orchestration::PlanCache::new(pool, plan_cache_config, &embed_model).await
            {
                Ok(cache) => self.orchestration.plan_cache = Some(cache),
                Err(e) => {
                    tracing::warn!(error = %e, "plan cache: init failed, proceeding without cache");
                }
            }
        } else {
            tracing::warn!("plan cache: memory not configured, proceeding without cache");
        }
    }

    /// Compute a normalized goal embedding for plan cache lookups (best-effort).
    ///
    /// Returns `None` when the cache is disabled, the provider does not support embeddings,
    /// or the embedding call fails.
    async fn goal_embedding_for_cache(&self, goal: &str) -> Option<Vec<f32>> {
        use crate::orchestration::normalize_goal;

        self.orchestration.plan_cache.as_ref()?;
        let normalized = normalize_goal(goal);
        match self.embedding_provider.embed(&normalized).await {
            Ok(emb) => Some(emb),
            Err(zeph_llm::LlmError::EmbedUnsupported { .. }) => {
                tracing::debug!(
                    "plan cache: provider does not support embeddings, skipping cache lookup"
                );
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "plan cache: goal embedding failed, skipping cache");
                None
            }
        }
    }

    async fn handle_plan_goal(&mut self, goal: &str) -> Result<(), error::AgentError> {
        use crate::orchestration::{LlmPlanner, plan_with_cache};

        if self.orchestration.pending_graph.is_some() {
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
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();

        let confirm_before_execute = self
            .orchestration
            .orchestration_config
            .confirm_before_execute;

        self.init_plan_cache_if_needed().await;
        let goal_embedding = self.goal_embedding_for_cache(goal).await;

        tracing::debug!(
            cache_enabled = self.orchestration.orchestration_config.plan_cache.enabled,
            has_embedding = goal_embedding.is_some(),
            "plan cache state for goal"
        );

        let planner_provider = self
            .orchestration
            .planner_provider
            .as_ref()
            .unwrap_or(&self.provider)
            .clone();
        let planner = LlmPlanner::new(planner_provider, &self.orchestration.orchestration_config);
        let embed_model = self.skill_state.embedding_model.clone();
        let (graph, planner_usage) = plan_with_cache(
            &planner,
            self.orchestration.plan_cache.as_ref(),
            &self.provider,
            goal_embedding.as_deref(),
            &embed_model,
            goal,
            &available_agents,
            self.orchestration.orchestration_config.max_tasks,
        )
        .await
        .map_err(|e| error::AgentError::Other(e.to_string()))?;

        // Store embedding for cache_plan() after execution completes.
        self.orchestration.pending_goal_embedding = goal_embedding;

        let task_count = graph.tasks.len() as u64;
        let snapshot = crate::metrics::TaskGraphSnapshot::from(&graph);
        let (planner_prompt, planner_completion) = planner_usage.unwrap_or((0, 0));
        self.update_metrics(|m| {
            m.api_calls += 1;
            m.prompt_tokens += planner_prompt;
            m.completion_tokens += planner_completion;
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
            m.orchestration.plans_total += 1;
            m.orchestration.tasks_total += task_count;
            m.orchestration_graph = Some(snapshot);
        });
        self.record_cost(planner_prompt, planner_completion);
        self.record_cache_usage();

        if confirm_before_execute {
            let summary = format_plan_summary(&graph);
            self.channel.send(&summary).await?;
            self.channel
                .send("Type `/plan confirm` to execute, or `/plan cancel` to abort.")
                .await?;
            self.orchestration.pending_graph = Some(graph);
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
            // pending_goal_embedding intentionally not cleared — overwritten on next /plan goal.
        }

        Ok(())
    }

    /// Validate that the pending plan graph can be executed.
    ///
    /// Sends an appropriate error message and restores the graph to `pending_graph` when
    /// validation fails. Returns `Ok(graph)` on success, `Err(())` when validation failed
    /// and the caller should return early.
    async fn validate_pending_graph(
        &mut self,
        graph: crate::orchestration::TaskGraph,
    ) -> Result<crate::orchestration::TaskGraph, ()> {
        use crate::orchestration::GraphStatus;

        if self.orchestration.subagent_manager.is_none() {
            let _ = self
                .channel
                .send(
                    "No sub-agents configured. Add sub-agent definitions to config \
                     to enable plan execution.",
                )
                .await;
            self.orchestration.pending_graph = Some(graph);
            return Err(());
        }

        // REV-2: pre-validate before moving graph into the constructor so we can
        // restore it to pending_graph on failure.
        if graph.tasks.is_empty() {
            let _ = self.channel.send("Plan has no tasks.").await;
            self.orchestration.pending_graph = Some(graph);
            return Err(());
        }

        // resume_from() rejects Completed and Canceled — guard those here too.
        if matches!(graph.status, GraphStatus::Completed | GraphStatus::Canceled) {
            let _ = self
                .channel
                .send(&format!(
                    "Cannot re-execute a {} plan. Use `/plan <goal>` to create a new one.",
                    graph.status
                ))
                .await;
            self.orchestration.pending_graph = Some(graph);
            return Err(());
        }

        Ok(graph)
    }

    /// Build a [`DagScheduler`] from the graph, reserving sub-agent slots.
    ///
    /// Returns `(scheduler, reserved)` on success or an `AgentError` on failure.
    /// Callers must call `mgr.release_reservation(reserved)` when done.
    fn build_dag_scheduler(
        &mut self,
        graph: crate::orchestration::TaskGraph,
    ) -> Result<(crate::orchestration::DagScheduler, usize), error::AgentError> {
        use crate::orchestration::{DagScheduler, GraphStatus, RuleBasedRouter};

        let available_agents = self
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();

        // Warn when max_concurrent is too low to support the configured parallelism.
        // This is the main cause of DagScheduler deadlocks (#1619): a planning-phase
        // sub-agent occupies the only slot while orchestration tasks are waiting.
        let max_concurrent = self.orchestration.subagent_config.max_concurrent;
        let max_parallel = self.orchestration.orchestration_config.max_parallel as usize;
        if max_concurrent < max_parallel + 1 {
            tracing::warn!(
                max_concurrent,
                max_parallel,
                "max_concurrent < max_parallel + 1: orchestration tasks may be starved by \
                 planning-phase sub-agents; recommend setting max_concurrent >= {}",
                max_parallel + 1
            );
        }

        // Reserve slots equal to max_parallel so the scheduler is guaranteed capacity
        // even if a planning-phase sub-agent is occupying a slot (#1619).
        let reserved = max_parallel.min(max_concurrent.saturating_sub(1));
        if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
            mgr.reserve_slots(reserved);
        }

        // Use resume_from() for graphs that are no longer in Created status
        // (e.g., after /plan retry which calls reset_for_retry and sets status=Running).
        let scheduler = if graph.status == GraphStatus::Created {
            DagScheduler::new(
                graph,
                &self.orchestration.orchestration_config,
                Box::new(RuleBasedRouter),
                available_agents,
            )
        } else {
            DagScheduler::resume_from(
                graph,
                &self.orchestration.orchestration_config,
                Box::new(RuleBasedRouter),
                available_agents,
            )
        }
        .map_err(|e| {
            // Release reservation before propagating error.
            if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                mgr.release_reservation(reserved);
            }
            error::AgentError::Other(e.to_string())
        })?;

        // Validate verify_provider name against the known provider pool (#2238).
        let provider_names: Vec<&str> = self
            .providers
            .provider_pool
            .iter()
            .filter_map(|e| e.name.as_deref())
            .collect();
        scheduler
            .validate_verify_config(&provider_names)
            .map_err(|e| {
                if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                    mgr.release_reservation(reserved);
                }
                error::AgentError::Other(e.to_string())
            })?;

        Ok((scheduler, reserved))
    }

    async fn handle_plan_confirm(&mut self) -> Result<(), error::AgentError> {
        let Some(graph) = self.orchestration.pending_graph.take() else {
            self.channel
                .send("No pending plan to confirm. Use `/plan <goal>` to create one.")
                .await?;
            return Ok(());
        };

        // validate_pending_graph sends the error message and restores the graph on failure.
        let Ok(graph) = self.validate_pending_graph(graph).await else {
            return Ok(());
        };

        let (mut scheduler, reserved) = self.build_dag_scheduler(graph)?;

        let task_count = scheduler.graph().tasks.len();
        self.channel
            .send(&format!(
                "Confirmed. Executing plan ({task_count} tasks)..."
            ))
            .await?;

        let plan_token = CancellationToken::new();
        self.orchestration.plan_cancel_token = Some(plan_token.clone());

        // Use match instead of ? so plan_cancel_token is always cleared (CRIT-07).
        let scheduler_result = self
            .run_scheduler_loop(&mut scheduler, task_count, plan_token)
            .await;
        self.orchestration.plan_cancel_token = None;

        // Always release the reservation, regardless of scheduler outcome.
        if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
            mgr.release_reservation(reserved);
        }

        let final_status = scheduler_result?;

        // Whole-plan verification: after all tasks complete, verify the full plan output
        // against the original goal and trigger a single replan cycle if gaps are found.
        // Only runs on Completed graphs — Failed/Canceled graphs skip verification.
        // Fail-open: any error logs a warn and proceeds to aggregation unchanged.
        let extra_task_outputs = self
            .run_whole_plan_verify(&mut scheduler, final_status)
            .await;

        let mut completed_graph = scheduler.into_graph();

        // Merge partial DAG outputs (from whole-plan replan) into the original graph so the
        // Aggregator sees both original and gap-filling task results (C2).
        // IDs are already offset by the original graph size (set in run_whole_plan_verify).
        if let Some(extra_tasks) = extra_task_outputs {
            completed_graph.tasks.extend(extra_tasks);
        }

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

    /// Run whole-plan verification after `DagScheduler` reaches `Done{Completed}`.
    ///
    /// Returns completed `TaskNode`s from the partial replan DAG when a replan cycle
    /// was executed. Returns `None` when verification is disabled, not applicable, or
    /// the plan passes the threshold. Returns `None` on any error (fail-open).
    ///
    /// The returned tasks must be merged into the original graph by the caller (C2)
    /// so the `Aggregator` sees both original and gap-filling task outputs.
    async fn run_whole_plan_verify(
        &mut self,
        scheduler: &mut crate::orchestration::DagScheduler,
        final_status: crate::orchestration::GraphStatus,
    ) -> Option<Vec<crate::orchestration::TaskNode>> {
        use crate::orchestration::{GraphStatus, PlanVerifier};

        if final_status != GraphStatus::Completed
            || !self.orchestration.orchestration_config.verify_completeness
            || scheduler.max_replans_remaining() == 0
        {
            return None;
        }

        let threshold = scheduler.completeness_threshold();
        let max_tokens = self.orchestration.orchestration_config.verify_max_tokens;
        let max_tasks = self.orchestration.orchestration_config.max_tasks;
        let goal = scheduler.graph().goal.clone();
        let truncated_output = collect_and_truncate_task_outputs(scheduler.graph(), max_tokens);

        if truncated_output.is_empty() {
            return None;
        }

        let verify_provider = self
            .orchestration
            .verify_provider
            .as_ref()
            .unwrap_or(&self.provider)
            .clone();
        let mut verifier =
            PlanVerifier::new(verify_provider, max_tokens, self.security.sanitizer.clone());
        let result = verifier.verify_plan(&goal, &truncated_output).await;

        tracing::debug!(
            complete = result.complete,
            confidence = result.confidence,
            gaps = result.gaps.len(),
            threshold,
            "whole-plan verification result"
        );

        let should_replan =
            !result.complete && result.confidence < f64::from(threshold) && !result.gaps.is_empty();

        if !should_replan {
            return None;
        }

        scheduler.record_whole_plan_replan();

        let next_id = u32::try_from(scheduler.graph().tasks.len()).unwrap_or(u32::MAX);
        let gap_tasks = match verifier
            .replan_from_plan(&goal, &result.gaps, next_id, max_tasks)
            .await
        {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "whole-plan replan_from_plan failed (fail-open)");
                return None;
            }
        };

        if gap_tasks.is_empty() {
            return None;
        }

        self.execute_partial_replan_dag(gap_tasks, &goal).await
    }

    /// Build and run a partial DAG from gap tasks generated by whole-plan verification.
    ///
    /// Uses `max_replans=0` and `verify_completeness=false` to prevent recursive replan
    /// loops (C1 / INV-2). Returns completed task nodes on success, `None` on any error.
    async fn execute_partial_replan_dag(
        &mut self,
        gap_tasks: Vec<crate::orchestration::TaskNode>,
        goal: &str,
    ) -> Option<Vec<crate::orchestration::TaskNode>> {
        use crate::orchestration::{DagScheduler, RuleBasedRouter, TaskStatus};

        let mut partial_graph = crate::orchestration::TaskGraph::new(goal);
        partial_graph.tasks = gap_tasks;

        let mut partial_config = self.orchestration.orchestration_config.clone();
        // INV-2: prevent recursive whole-plan replan loops in the partial DAG.
        partial_config.max_replans = 0;
        partial_config.verify_completeness = false;

        let available_agents = self
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().to_vec())
            .unwrap_or_default();

        let mut partial_scheduler = match DagScheduler::new(
            partial_graph,
            &partial_config,
            Box::new(RuleBasedRouter),
            available_agents,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "whole-plan replan: failed to create partial DagScheduler (fail-open)"
                );
                return None;
            }
        };

        let partial_task_count = partial_scheduler.graph().tasks.len();
        let cancel_token = CancellationToken::new();
        if let Err(e) = self
            .run_scheduler_loop(&mut partial_scheduler, partial_task_count, cancel_token)
            .await
        {
            tracing::warn!(
                error = %e,
                "whole-plan replan: partial DAG run failed (fail-open)"
            );
        }

        let completed: Vec<_> = partial_scheduler
            .into_graph()
            .tasks
            .into_iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .collect();

        if completed.is_empty() {
            None
        } else {
            Some(completed)
        }
    }

    /// Cancel all agents referenced in `cancel_actions`.
    ///
    /// Returns `Some(status)` if a `Done` action is encountered, `None` otherwise.
    fn cancel_agents_from_actions(
        &mut self,
        cancel_actions: Vec<crate::orchestration::SchedulerAction>,
    ) -> Option<crate::orchestration::GraphStatus> {
        use crate::orchestration::SchedulerAction;
        for action in cancel_actions {
            match action {
                SchedulerAction::Cancel { agent_handle_id } => {
                    if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                        let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                            tracing::trace!(error = %e, "cancel: agent already gone");
                        });
                    }
                }
                SchedulerAction::Done { status } => return Some(status),
                SchedulerAction::Spawn { .. }
                | SchedulerAction::RunInline { .. }
                | SchedulerAction::Verify { .. } => {}
            }
        }
        None
    }

    /// Handle a `SchedulerAction::Spawn` — attempt to spawn a sub-agent for the given task.
    ///
    /// Returns `(spawn_success, concurrency_fail, done_status)`.
    /// `done_status` is `Some` when spawn failure forces the scheduler to emit a `Done` action.
    async fn handle_scheduler_spawn_action(
        &mut self,
        scheduler: &mut crate::orchestration::DagScheduler,
        task_id: crate::orchestration::TaskId,
        agent_def_name: String,
        prompt: String,
        spawn_counter: &mut usize,
        task_count: usize,
    ) -> (bool, bool, Option<crate::orchestration::GraphStatus>) {
        let task_title = scheduler
            .graph()
            .tasks
            .get(task_id.index())
            .map_or("unknown", |t| t.title.as_str());

        let provider = self.provider.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        let skills = self.filtered_skills_for(&agent_def_name);
        let cfg = self.orchestration.subagent_config.clone();
        let event_tx = scheduler.event_sender();

        let mgr = self
            .orchestration
            .subagent_manager
            .as_mut()
            .expect("subagent_manager checked above");

        let on_done = {
            use crate::orchestration::{TaskEvent, TaskOutcome};
            move |handle_id: String, result: Result<String, crate::subagent::SubAgentError>| {
                let outcome = match &result {
                    Ok(output) => TaskOutcome::Completed {
                        output: output.clone(),
                        artifacts: vec![],
                    },
                    Err(e) => TaskOutcome::Failed {
                        error: e.to_string(),
                    },
                };
                let tx = event_tx;
                tokio::spawn(async move {
                    if let Err(e) = tx
                        .send(TaskEvent {
                            task_id,
                            agent_handle_id: handle_id,
                            outcome,
                        })
                        .await
                    {
                        tracing::warn!(
                            error = %e,
                            "failed to send TaskEvent: scheduler may have been dropped"
                        );
                    }
                });
            }
        };

        match mgr.spawn_for_task(
            &agent_def_name,
            &prompt,
            provider,
            tool_executor,
            skills,
            &cfg,
            on_done,
        ) {
            Ok(handle_id) => {
                *spawn_counter += 1;
                let _ = self
                    .channel
                    .send_status(&format!(
                        "Executing task {spawn_counter}/{task_count}: {task_title}..."
                    ))
                    .await;
                scheduler.record_spawn(task_id, handle_id, agent_def_name);
                (true, false, None)
            }
            Err(e) => {
                tracing::error!(error = %e, %task_id, "spawn_for_task failed");
                let concurrency_fail =
                    matches!(e, crate::subagent::SubAgentError::ConcurrencyLimit { .. });
                let extra = scheduler.record_spawn_failure(task_id, &e);
                let done_status = self.cancel_agents_from_actions(extra);
                (false, concurrency_fail, done_status)
            }
        }
    }

    /// Execute a `RunInline` scheduler action: run the task synchronously in the current agent.
    ///
    /// Sends a status update, registers the spawn with the scheduler, runs the inline tool
    /// loop (or cancels on token fire), and posts the completion event back to the scheduler.
    async fn handle_run_inline_action(
        &mut self,
        scheduler: &mut crate::orchestration::DagScheduler,
        task_id: crate::orchestration::TaskId,
        prompt: String,
        spawn_counter: usize,
        task_count: usize,
        cancel_token: &CancellationToken,
    ) {
        let task_title = scheduler
            .graph()
            .tasks
            .get(task_id.index())
            .map_or("unknown", |t| t.title.as_str());
        let _ = self
            .channel
            .send_status(&format!(
                "Executing task {spawn_counter}/{task_count} (inline): {task_title}..."
            ))
            .await;

        // record_spawn before the inline call so the completion event is always
        // buffered before any timeout check fires in the next tick().
        let handle_id = format!("__inline_{task_id}__");
        scheduler.record_spawn(task_id, handle_id.clone(), "__main__".to_string());

        let event_tx = scheduler.event_sender();
        let max_iter = self.tool_orchestrator.max_iterations;
        let outcome = tokio::select! {
            result = self.run_inline_tool_loop(&prompt, max_iter) => {
                match result {
                    Ok(output) => crate::orchestration::TaskOutcome::Completed {
                        output,
                        artifacts: vec![],
                    },
                    Err(e) => crate::orchestration::TaskOutcome::Failed {
                        error: e.to_string(),
                    },
                }
            }
            () = cancel_token.cancelled() => {
                // TODO: use TaskOutcome::Canceled when the variant is added (#1603)
                crate::orchestration::TaskOutcome::Failed {
                    error: "canceled".to_string(),
                }
            }
        };
        let event = crate::orchestration::TaskEvent {
            task_id,
            agent_handle_id: handle_id,
            outcome,
        };
        if let Err(e) = event_tx.send(event).await {
            tracing::warn!(%task_id, error = %e, "inline task event send failed");
        }
    }

    // too_many_lines: sequential scheduler event loop with 4 tokio::select! branches
    // (cancel token, scheduler tick, channel recv with /plan cancel + channel-close paths,
    // shutdown signal) — each branch requires distinct cancel/fail/ignore semantics that
    // cannot be split without introducing shared mutable state across async boundaries.
    #[allow(clippy::too_many_lines)]
    /// Drive the [`DagScheduler`] tick loop until it emits `SchedulerAction::Done`.
    ///
    /// Each iteration yields at `wait_event()`, during which `channel.recv()` is polled
    /// concurrently via `tokio::select!`. If the user sends `/plan cancel`, all running
    /// sub-agent tasks are aborted and the loop exits with [`GraphStatus::Canceled`].
    /// If the channel is closed (`Ok(None)`), all running sub-agent tasks are aborted
    /// and the loop exits with [`GraphStatus::Failed`].
    /// Other messages received during execution are queued in `message_queue` and
    /// processed after the plan completes.
    ///
    /// # Known limitations
    ///
    /// `RunInline` tasks block the tick loop for their entire duration — `/plan cancel`
    /// cannot interrupt an in-progress inline LLM call and will only be delivered on the
    /// next iteration after the call completes.
    async fn run_scheduler_loop(
        &mut self,
        scheduler: &mut crate::orchestration::DagScheduler,
        task_count: usize,
        cancel_token: CancellationToken,
    ) -> Result<crate::orchestration::GraphStatus, error::AgentError> {
        use crate::orchestration::{PlanVerifier, SchedulerAction};

        // Sequential spawn counter for human-readable "task N/M" progress messages.
        // task_id.index() reflects array position and can be non-contiguous for
        // parallel plans (e.g. 0, 2, 4), so we use a local counter instead.
        let mut spawn_counter: usize = 0;

        // Tracks (handle_id, secret_key) pairs denied this plan execution to prevent
        // re-prompting the user when a sub-agent re-requests the same secret after denial.
        let mut denied_secrets: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();

        // Lazily initialized per-task verifier. Created once on the first Verify action;
        // reused for all subsequent per-task Verify calls in this scheduler run.
        let mut plan_verifier: Option<PlanVerifier<AnyProvider>> = None;

        let final_status = 'tick: loop {
            let actions = scheduler.tick();

            // Track batch-level spawn outcomes for record_batch_backoff() below.
            let mut any_spawn_success = false;
            let mut any_concurrency_failure = false;

            for action in actions {
                match action {
                    SchedulerAction::Spawn {
                        task_id,
                        agent_def_name,
                        prompt,
                    } => {
                        let (success, fail, done) = self
                            .handle_scheduler_spawn_action(
                                scheduler,
                                task_id,
                                agent_def_name,
                                prompt,
                                &mut spawn_counter,
                                task_count,
                            )
                            .await;
                        any_spawn_success |= success;
                        any_concurrency_failure |= fail;
                        if let Some(s) = done {
                            break 'tick s;
                        }
                    }
                    SchedulerAction::Cancel { agent_handle_id } => {
                        if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                            // benign race: agent may have already finished
                            let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                                tracing::trace!(error = %e, "cancel: agent already gone");
                            });
                        }
                    }
                    // Inline execution: the LLM call blocks this tick loop for its
                    // duration. This is intentionally sequential and only expected in
                    // single-agent setups where no sub-agents are configured.
                    // Known limitation: if a RunInline action appears before Spawn actions
                    // in the same batch (mixed routing), those Spawn actions are delayed
                    // until the inline call completes. Refactor to tokio::spawn if mixed
                    // batches become common.
                    // TODO(post-MVP): wire CancellationToken into run_inline_tool_loop so
                    // that /plan cancel can interrupt a long-running inline LLM call instead
                    // of waiting for the current iteration to complete.
                    SchedulerAction::RunInline { task_id, prompt } => {
                        spawn_counter += 1;
                        self.handle_run_inline_action(
                            scheduler,
                            task_id,
                            prompt,
                            spawn_counter,
                            task_count,
                            &cancel_token,
                        )
                        .await;
                    }
                    SchedulerAction::Done { status } => {
                        break 'tick status;
                    }
                    SchedulerAction::Verify { task_id, output } => {
                        // Per-task verification: evaluate the completed task's output and
                        // optionally inject replan tasks if gaps are found below threshold.
                        // Fail-open: any error logs a warning and continues without blocking.
                        let verify_provider = self
                            .orchestration
                            .verify_provider
                            .as_ref()
                            .unwrap_or(&self.provider)
                            .clone();
                        let max_tokens = self.orchestration.orchestration_config.verify_max_tokens;
                        let threshold = self
                            .orchestration
                            .orchestration_config
                            .completeness_threshold;
                        let sanitizer = self.security.sanitizer.clone();

                        // Initialize the verifier once; reuse across Verify actions.
                        let verifier = plan_verifier.get_or_insert_with(|| {
                            PlanVerifier::new(verify_provider, max_tokens, sanitizer)
                        });

                        let task = scheduler.graph().tasks.get(task_id.index()).cloned();

                        if let Some(task) = task {
                            let result = verifier.verify(&task, &output).await;
                            tracing::debug!(
                                task_id = %task_id,
                                complete = result.complete,
                                confidence = result.confidence,
                                gaps = result.gaps.len(),
                                "per-task verification result"
                            );

                            let should_replan = !result.complete
                                && result.confidence < f64::from(threshold)
                                && result.gaps.iter().any(|g| {
                                    matches!(
                                        g.severity,
                                        crate::orchestration::GapSeverity::Critical
                                            | crate::orchestration::GapSeverity::Important
                                    )
                                });

                            if should_replan {
                                let max_tasks_u32 =
                                    self.orchestration.orchestration_config.max_tasks;
                                let max_tasks = max_tasks_u32 as usize;
                                match verifier
                                    .replan(&task, &result.gaps, scheduler.graph(), max_tasks_u32)
                                    .await
                                {
                                    Ok(new_tasks) if !new_tasks.is_empty() => {
                                        if let Err(e) =
                                            scheduler.inject_tasks(task_id, new_tasks, max_tasks)
                                        {
                                            tracing::warn!(
                                                error = %e,
                                                task_id = %task_id,
                                                "per-task replan inject_tasks failed (fail-open)"
                                            );
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            task_id = %task_id,
                                            "per-task replan failed (fail-open)"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Update batch-level backoff counter after processing all Spawn actions.
            scheduler.record_batch_backoff(any_spawn_success, any_concurrency_failure);

            // Drain all pending secret requests this tick (MED-2 fix).
            self.process_pending_secret_requests(&mut denied_secrets)
                .await;

            // Update TUI with current graph state.
            let snapshot = crate::metrics::TaskGraphSnapshot::from(scheduler.graph());
            self.update_metrics(|m| {
                m.orchestration_graph = Some(snapshot);
            });

            // NOTE(Telegram): Telegram's recv() is not fully cancel-safe — a message
            // consumed from the internal mpsc but not yet returned can be lost if the
            // select! cancels the future during the /start send().await path. For
            // non-command messages the race window is negligible. Acceptable for MVP.
            //
            // NOTE(RunInline): tasks in the RunInline arm above block this tick loop
            // synchronously (no await between loop iteration start and wait_event).
            // /plan cancel cannot interrupt an inline LLM call mid-execution; it is
            // delivered on the next tick after the inline call completes.
            // TODO(post-MVP): wire CancellationToken into run_inline_tool_loop.
            tokio::select! {
                // biased: token cancellation takes priority over new events and input.
                biased;
                () = cancel_token.cancelled() => {
                    let cancel_actions = scheduler.cancel_all();
                    for action in cancel_actions {
                        match action {
                            SchedulerAction::Cancel { agent_handle_id } => {
                                if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                                    let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                                        tracing::trace!(
                                            error = %e,
                                            "cancel during plan cancellation: agent already gone"
                                        );
                                    });
                                }
                            }
                            SchedulerAction::Done { status } => {
                                break 'tick status;
                            }
                            SchedulerAction::Spawn { .. }
                            | SchedulerAction::RunInline { .. }
                            | SchedulerAction::Verify { .. } => {}
                        }
                    }
                    // Defensive fallback: cancel_all always emits Done, but guard against
                    // future changes.
                    break 'tick crate::orchestration::GraphStatus::Canceled;
                }
                () = scheduler.wait_event() => {}
                result = self.channel.recv() => {
                    if let Ok(Some(msg)) = result {
                        if msg.text.trim().eq_ignore_ascii_case("/plan cancel") {
                            let _ = self.channel.send_status("Canceling plan...").await;
                            let cancel_actions = scheduler.cancel_all();
                            for ca in cancel_actions {
                                match ca {
                                    SchedulerAction::Cancel { agent_handle_id } => {
                                        if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                                            // benign race: agent may have already finished
                                            let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                                                tracing::trace!(error = %e, "cancel on user request: agent already gone");
                                            });
                                        }
                                    }
                                    SchedulerAction::Done { status } => {
                                        break 'tick status;
                                    }
                                    SchedulerAction::Spawn { .. }
                                    | SchedulerAction::RunInline { .. }
                                    | SchedulerAction::Verify { .. } => {}
                                }
                            }
                            // Defensive fallback: cancel_all always emits Done, but guard
                            // against future changes.
                            break 'tick crate::orchestration::GraphStatus::Canceled;
                        }
                        self.enqueue_or_merge(msg.text, vec![], msg.attachments);
                    } else {
                        // Channel closed. Drain buffered completion events BEFORE canceling
                        // so that tasks which completed between the last tick and the
                        // channel-close are recorded as Completed, not Canceled.
                        // cancel_all() empties self.running first, causing process_event()
                        // to silently discard any late completions — drain must come first.
                        let drain_actions = scheduler.tick();
                        let mut natural_done: Option<crate::orchestration::GraphStatus> = None;
                        for action in drain_actions {
                            match action {
                                SchedulerAction::Cancel { agent_handle_id } => {
                                    if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                                        let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                                            tracing::trace!(
                                                error = %e,
                                                "cancel during drain on channel close: agent already gone"
                                            );
                                        });
                                    }
                                }
                                SchedulerAction::Done { status } => {
                                    natural_done = Some(status);
                                }
                                // Ignore Spawn/RunInline/Verify — we are shutting down.
                                SchedulerAction::Spawn { .. }
                                | SchedulerAction::RunInline { .. }
                                | SchedulerAction::Verify { .. } => {}
                            }
                        }

                        // If the plan completed naturally during the drain tick, honor that.
                        if let Some(status) = natural_done {
                            break 'tick status;
                        }

                        // Cancel remaining running tasks after the drain.
                        let cancel_actions = scheduler.cancel_all();
                        let n = cancel_actions
                            .iter()
                            .filter(|a| matches!(a, SchedulerAction::Cancel { .. }))
                            .count();
                        // Use supports_exit() to distinguish termination semantics:
                        // - CLI/TUI (supports_exit=true): stdin EOF or TUI close → Canceled
                        // - Telegram/Discord/Slack (supports_exit=false): infra failure → Failed
                        //   so the user can /plan retry after reconnecting.
                        let shutdown_status = if self.channel.supports_exit() {
                            crate::orchestration::GraphStatus::Canceled
                        } else {
                            crate::orchestration::GraphStatus::Failed
                        };
                        tracing::warn!(
                            sub_agents = n,
                            supports_exit = self.channel.supports_exit(),
                            status = ?shutdown_status,
                            "scheduler channel closed, canceling running sub-agents"
                        );
                        for action in cancel_actions {
                            match action {
                                SchedulerAction::Cancel { agent_handle_id } => {
                                    if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                                        let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                                            tracing::trace!(
                                                error = %e,
                                                "cancel on channel close: agent already gone"
                                            );
                                        });
                                    }
                                }
                                // Intentionally ignore Done here — we use shutdown_status above.
                                SchedulerAction::Done { .. }
                                | SchedulerAction::Spawn { .. }
                                | SchedulerAction::RunInline { .. }
                                | SchedulerAction::Verify { .. } => {}
                            }
                        }
                        break 'tick shutdown_status;
                    }
                }
                // Shutdown signal received — cancel running sub-agents and exit cleanly.
                () = shutdown_signal(&mut self.lifecycle.shutdown) => {
                    let cancel_actions = scheduler.cancel_all();
                    let n = cancel_actions
                        .iter()
                        .filter(|a| matches!(a, SchedulerAction::Cancel { .. }))
                        .count();
                    tracing::warn!(sub_agents = n, "shutdown signal received, canceling running sub-agents");
                    for action in cancel_actions {
                        match action {
                            SchedulerAction::Cancel { agent_handle_id } => {
                                if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                                    let _ = mgr.cancel(&agent_handle_id).inspect_err(|e| {
                                        tracing::trace!(
                                            error = %e,
                                            "cancel on shutdown: agent already gone"
                                        );
                                    });
                                }
                            }
                            SchedulerAction::Done { status } => {
                                break 'tick status;
                            }
                            SchedulerAction::Spawn { .. }
                            | SchedulerAction::RunInline { .. }
                            | SchedulerAction::Verify { .. } => {}
                        }
                    }
                    // Defensive fallback: cancel_all always emits Done, but guard against
                    // future changes.
                    break 'tick crate::orchestration::GraphStatus::Canceled;
                }
            }
        };

        // Final drain: if the loop exited via Done on the first tick, secret
        // requests buffered before completion would otherwise be silently dropped.
        self.process_pending_secret_requests(&mut std::collections::HashSet::new())
            .await;

        Ok(final_status)
    }

    /// Run a tool-aware LLM loop for an inline scheduled task.
    ///
    /// Unlike [`process_response_native_tools`], this is intentionally stripped of all
    /// interactive-session machinery (channel sends, doom-loop detection, summarization,
    /// learning engine, sanitizer, metrics). Inline tasks are short-lived orchestration
    /// sub-tasks that run synchronously inside the scheduler tick loop.
    async fn run_inline_tool_loop(
        &self,
        prompt: &str,
        max_iterations: usize,
    ) -> Result<String, zeph_llm::LlmError> {
        use zeph_llm::provider::{ChatResponse, Message, MessagePart, Role, ToolDefinition};
        use zeph_tools::executor::ToolCall;

        // CRIT-01 / TAFC isolation: inline tool loops run as subagent orchestration tasks
        // (scheduler, planner, aggregator) and intentionally bypass TAFC augmentation.
        // They use their own private message history and never surface TAFC think fields
        // to the interactive session, so no stripping is needed here (CRIT-02 compliance).
        let tool_defs: Vec<ToolDefinition> = self
            .tool_executor
            .tool_definitions_erased()
            .iter()
            .map(tool_execution::tool_def_to_definition)
            .collect();

        tracing::debug!(
            prompt_len = prompt.len(),
            max_iterations,
            tool_count = tool_defs.len(),
            "inline tool loop: starting"
        );

        let mut messages: Vec<Message> = vec![Message::from_legacy(Role::User, prompt)];
        let mut last_text = String::new();

        for iteration in 0..max_iterations {
            let response = self.provider.chat_with_tools(&messages, &tool_defs).await?;

            match response {
                ChatResponse::Text(text) => {
                    tracing::debug!(iteration, "inline tool loop: text response, returning");
                    return Ok(text);
                }
                ChatResponse::ToolUse {
                    text, tool_calls, ..
                } => {
                    tracing::debug!(
                        iteration,
                        tools = ?tool_calls.iter().map(|tc| &tc.name).collect::<Vec<_>>(),
                        "inline tool loop: tool use"
                    );

                    if let Some(ref t) = text {
                        last_text.clone_from(t);
                    }

                    // Build assistant message with optional leading text + tool use parts.
                    let mut parts: Vec<MessagePart> = Vec::new();
                    if let Some(ref t) = text
                        && !t.is_empty()
                    {
                        parts.push(MessagePart::Text { text: t.clone() });
                    }
                    for tc in &tool_calls {
                        parts.push(MessagePart::ToolUse {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            input: tc.input.clone(),
                        });
                    }
                    messages.push(Message::from_parts(Role::Assistant, parts));

                    // Execute each tool call and collect results.
                    let mut result_parts: Vec<MessagePart> = Vec::new();
                    for tc in &tool_calls {
                        let call = ToolCall {
                            tool_id: tc.name.clone(),
                            params: match &tc.input {
                                serde_json::Value::Object(map) => map.clone(),
                                _ => serde_json::Map::new(),
                            },
                        };
                        let output = match self.tool_executor.execute_tool_call_erased(&call).await
                        {
                            Ok(Some(out)) => out.summary,
                            Ok(None) => "(no output)".to_owned(),
                            Err(e) => format!("[error] {e}"),
                        };
                        let is_error = output.starts_with("[error]");
                        result_parts.push(MessagePart::ToolResult {
                            tool_use_id: tc.id.clone(),
                            content: output,
                            is_error,
                        });
                    }
                    messages.push(Message::from_parts(Role::User, result_parts));
                }
            }
        }

        tracing::debug!(
            max_iterations,
            last_text_empty = last_text.is_empty(),
            "inline tool loop: iteration limit reached"
        );
        Ok(last_text)
    }

    /// Bridge pending secret requests from sub-agents to the user (non-blocking, time-bounded).
    ///
    /// SEC-P1-02: explicit user confirmation is required before granting any secret to a
    /// sub-agent. Denial is the default on timeout or channel error.
    ///
    /// `denied` tracks `(handle_id, secret_key)` pairs already denied this plan execution.
    /// Re-requests for a denied pair are auto-denied without prompting the user.
    async fn process_pending_secret_requests(
        &mut self,
        denied: &mut std::collections::HashSet<(String, String)>,
    ) {
        loop {
            let pending = self
                .orchestration
                .subagent_manager
                .as_mut()
                .and_then(crate::subagent::SubAgentManager::try_recv_secret_request);
            let Some((req_handle_id, req)) = pending else {
                break;
            };
            let deny_key = (req_handle_id.clone(), req.secret_key.clone());
            if denied.contains(&deny_key) {
                tracing::debug!(
                    handle_id = %req_handle_id,
                    secret_key = %req.secret_key,
                    "skipping duplicate secret prompt for already-denied key"
                );
                if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                    let _ = mgr.deny_secret(&req_handle_id);
                }
                continue;
            }
            let prompt = format!(
                "Sub-agent requests secret '{}'. Allow?{}",
                crate::text::truncate_to_chars(&req.secret_key, 100),
                req.reason
                    .as_deref()
                    .map(|r| format!(" Reason: {}", crate::text::truncate_to_chars(r, 200)))
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
            if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
                if approved {
                    let ttl = std::time::Duration::from_secs(300);
                    let key = req.secret_key.clone();
                    if mgr.approve_secret(&req_handle_id, &key, ttl).is_ok() {
                        let _ = mgr.deliver_secret(&req_handle_id, key);
                    }
                } else {
                    denied.insert(deny_key);
                    let _ = mgr.deny_secret(&req_handle_id);
                }
            }
        }
    }

    /// Aggregate results or report failure after the tick loop completes.
    #[allow(clippy::too_many_lines)]
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
                let skipped_count = completed_graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == crate::orchestration::TaskStatus::Skipped)
                    .count() as u64;
                self.update_metrics(|m| {
                    m.orchestration.tasks_completed += completed_count;
                    m.orchestration.tasks_skipped += skipped_count;
                });

                let aggregator = LlmAggregator::new(
                    self.provider.clone(),
                    &self.orchestration.orchestration_config,
                );
                match aggregator.aggregate(&completed_graph).await {
                    Ok((synthesis, aggregator_usage)) => {
                        let (aggr_prompt, aggr_completion) = aggregator_usage.unwrap_or((0, 0));
                        self.update_metrics(|m| {
                            m.api_calls += 1;
                            m.prompt_tokens += aggr_prompt;
                            m.completion_tokens += aggr_completion;
                            m.total_tokens = m.prompt_tokens + m.completion_tokens;
                        });
                        self.record_cost(aggr_prompt, aggr_completion);
                        self.record_cache_usage();
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

                // Cache the completed plan template (best-effort, never blocks execution).
                if let Some(ref cache) = self.orchestration.plan_cache
                    && let Some(embedding) = self.orchestration.pending_goal_embedding.take()
                {
                    let embed_model = self.skill_state.embedding_model.clone();
                    if let Err(e) = cache
                        .cache_plan(&completed_graph, &embedding, &embed_model)
                        .await
                    {
                        tracing::warn!(error = %e, "plan cache: failed to cache completed plan");
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
                let cancelled_tasks: Vec<_> = completed_graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == crate::orchestration::TaskStatus::Canceled)
                    .collect();
                let completed_count = completed_graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == crate::orchestration::TaskStatus::Completed)
                    .count() as u64;
                let skipped_count = completed_graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == crate::orchestration::TaskStatus::Skipped)
                    .count() as u64;
                self.update_metrics(|m| {
                    m.orchestration.tasks_failed += failed_tasks.len() as u64;
                    m.orchestration.tasks_completed += completed_count;
                    m.orchestration.tasks_skipped += skipped_count;
                });
                let total = completed_graph.tasks.len();
                let msg = if failed_tasks.is_empty() && !cancelled_tasks.is_empty() {
                    // Pure scheduler deadlock: no tasks actually failed, some were canceled.
                    format!(
                        "Plan canceled. {}/{} tasks did not run.\n\
                         Use `/plan retry` to retry or check logs for details.",
                        cancelled_tasks.len(),
                        total
                    )
                } else if failed_tasks.is_empty() && cancelled_tasks.is_empty() {
                    // Should not occur through normal scheduler paths; make it visible.
                    tracing::warn!(
                        "plan finished with GraphStatus::Failed but no failed or canceled tasks"
                    );
                    "Plan failed. No task errors recorded; check logs for details.".to_string()
                } else {
                    let mut m = if cancelled_tasks.is_empty() {
                        format!(
                            "Plan failed. {}/{} tasks failed:\n",
                            failed_tasks.len(),
                            total
                        )
                    } else {
                        format!(
                            "Plan failed. {}/{} tasks failed, {} canceled:\n",
                            failed_tasks.len(),
                            total,
                            cancelled_tasks.len()
                        )
                    };
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
                        let _ = writeln!(m, "  - {}: {err}", t.title);
                    }
                    m.push_str("\nUse `/plan retry` to retry failed tasks.");
                    m
                };
                self.channel.send(&msg).await?;
                // Store graph back so /plan retry and /plan resume work.
                // pending_goal_embedding is retained: retry/resume goes through handle_plan_confirm
                // -> finalize_plan_execution again, reusing the same embedding. A new /plan goal
                // cannot be issued while pending_graph is Some, so the embedding cannot go stale.
                self.orchestration.pending_graph = Some(completed_graph);
                "failed"
            }
            GraphStatus::Paused => {
                self.channel
                    .send(
                        "Plan paused due to a task failure (ask strategy). \
                         Use `/plan resume` to continue or `/plan retry` to retry failed tasks.",
                    )
                    .await?;
                // Same retention rationale as Failed: embedding reused on resume/retry.
                self.orchestration.pending_graph = Some(completed_graph);
                "paused"
            }
            GraphStatus::Canceled => {
                let done_count = completed_graph
                    .tasks
                    .iter()
                    .filter(|t| t.status == crate::orchestration::TaskStatus::Completed)
                    .count();
                self.update_metrics(|m| m.orchestration.tasks_completed += done_count as u64);
                let total = completed_graph.tasks.len();
                self.channel
                    .send(&format!(
                        "Plan canceled. {done_count}/{total} tasks completed before cancellation."
                    ))
                    .await?;
                // Do NOT store graph back into pending_graph — canceled plans are not
                // retryable via /plan retry.
                self.orchestration.pending_goal_embedding.take();
                "canceled"
            }
            other => {
                tracing::warn!(%other, "unexpected graph status after Done");
                self.channel
                    .send(&format!("Plan ended with status: {other}"))
                    .await?;
                self.orchestration.pending_goal_embedding.take();
                "unknown"
            }
        };
        Ok(result_label)
    }

    async fn handle_plan_status(
        &mut self,
        _graph_id: Option<&str>,
    ) -> Result<(), error::AgentError> {
        use crate::orchestration::GraphStatus;
        let Some(ref graph) = self.orchestration.pending_graph else {
            self.channel.send("No active plan.").await?;
            return Ok(());
        };
        let msg = match graph.status {
            GraphStatus::Created => {
                "A plan is awaiting confirmation. Type `/plan confirm` to execute or `/plan cancel` to abort."
            }
            GraphStatus::Running => "Plan is currently running.",
            GraphStatus::Paused => {
                "Plan is paused. Use `/plan resume` to continue or `/plan cancel` to abort."
            }
            GraphStatus::Failed => {
                "Plan failed. Use `/plan retry` to retry or `/plan cancel` to discard."
            }
            GraphStatus::Completed => "Plan completed successfully.",
            GraphStatus::Canceled => "Plan was canceled.",
        };
        self.channel.send(msg).await?;
        Ok(())
    }

    async fn handle_plan_list(&mut self) -> Result<(), error::AgentError> {
        if let Some(ref graph) = self.orchestration.pending_graph {
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
        if let Some(token) = self.orchestration.plan_cancel_token.take() {
            // In-flight plan: signal cancellation. The scheduler loop will pick this up
            // in the next tokio::select! iteration at wait_event().
            // NOTE: Due to &mut self being held by run_scheduler_loop, this branch is only
            // reachable if the channel has a concurrent reader (e.g. Telegram, TUI events).
            // CLI and synchronous channels cannot deliver this while the loop is active
            // (see #1603, SEC-M34-002).
            token.cancel();
            self.channel.send("Canceling plan execution...").await?;
        } else if self.orchestration.pending_graph.take().is_some() {
            let now = std::time::Instant::now();
            self.update_metrics(|m| {
                if let Some(ref mut s) = m.orchestration_graph {
                    "canceled".clone_into(&mut s.status);
                    s.completed_at = Some(now);
                }
            });
            self.orchestration.pending_goal_embedding = None;
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

        let Some(ref graph) = self.orchestration.pending_graph else {
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

        let graph = self.orchestration.pending_graph.take().unwrap();

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
        self.orchestration.pending_graph = Some(graph);
        Ok(())
    }

    /// Retry failed tasks in a graph.
    ///
    /// Resets all `Failed` tasks to `Ready` and all `Skipped` dependents back
    /// to `Pending`, then re-stores the graph as pending for re-execution.
    /// If `graph_id` is provided it must match the active graph's id (SEC-P5-04).
    async fn handle_plan_retry(&mut self, graph_id: Option<&str>) -> Result<(), error::AgentError> {
        use crate::orchestration::{GraphStatus, dag};

        let Some(ref graph) = self.orchestration.pending_graph else {
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

        let mut graph = self.orchestration.pending_graph.take().unwrap();

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
        self.orchestration.pending_graph = Some(graph);
        Ok(())
    }

    /// Call the LLM to generate a structured session summary with a configurable timeout.
    ///
    /// Falls back to plain-text chat if structured output fails or times out. Returns `None` on
    /// any failure, logging a warning — callers must treat `None` as "skip storage".
    ///
    /// Each LLM attempt is bounded by `shutdown_summary_timeout_secs`; in the worst case
    /// (structured call times out and plain-text fallback also times out) this adds up to
    /// `2 * shutdown_summary_timeout_secs` of shutdown latency.
    async fn call_llm_for_session_summary(
        &self,
        chat_messages: &[Message],
    ) -> Option<zeph_memory::StructuredSummary> {
        let timeout_dur =
            std::time::Duration::from_secs(self.memory_state.shutdown_summary_timeout_secs);
        match tokio::time::timeout(
            timeout_dur,
            self.provider
                .chat_typed_erased::<zeph_memory::StructuredSummary>(chat_messages),
        )
        .await
        {
            Ok(Ok(s)) => Some(s),
            Ok(Err(e)) => {
                tracing::warn!(
                    "shutdown summary: structured LLM call failed, falling back to plain: {e:#}"
                );
                self.plain_text_summary_fallback(chat_messages, timeout_dur)
                    .await
            }
            Err(_) => {
                tracing::warn!(
                    "shutdown summary: structured LLM call timed out after {}s, falling back to plain",
                    self.memory_state.shutdown_summary_timeout_secs
                );
                self.plain_text_summary_fallback(chat_messages, timeout_dur)
                    .await
            }
        }
    }

    async fn plain_text_summary_fallback(
        &self,
        chat_messages: &[Message],
        timeout_dur: std::time::Duration,
    ) -> Option<zeph_memory::StructuredSummary> {
        match tokio::time::timeout(timeout_dur, self.provider.chat(chat_messages)).await {
            Ok(Ok(plain)) => Some(zeph_memory::StructuredSummary {
                summary: plain,
                key_facts: vec![],
                entities: vec![],
            }),
            Ok(Err(e)) => {
                tracing::warn!("shutdown summary: plain LLM fallback failed: {e:#}");
                None
            }
            Err(_) => {
                tracing::warn!("shutdown summary: plain LLM fallback timed out");
                None
            }
        }
    }

    /// Generate and store a lightweight session summary at shutdown when no hard compaction fired.
    ///
    /// Guards:
    /// - `shutdown_summary` config must be enabled
    /// - `conversation_id` must be set (memory must be attached)
    /// - no existing session summary in the store (primary guard — resilient to failed Qdrant writes)
    /// - at least `shutdown_summary_min_messages` user-turn messages in history
    ///
    /// All errors are logged as warnings and swallowed — shutdown must never fail.
    async fn maybe_store_shutdown_summary(&mut self) {
        if !self.memory_state.shutdown_summary {
            return;
        }
        let Some(memory) = self.memory_state.memory.clone() else {
            return;
        };
        let Some(conversation_id) = self.memory_state.conversation_id else {
            return;
        };

        // Primary guard: check if a summary already exists (handles failed Qdrant writes too).
        match memory.has_session_summary(conversation_id).await {
            Ok(true) => {
                tracing::debug!("shutdown summary: session already has a summary, skipping");
                return;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!("shutdown summary: failed to check existing summary: {e:#}");
                return;
            }
        }

        // Count user-turn messages only (skip system prompt at index 0).
        let user_count = self
            .msg
            .messages
            .iter()
            .skip(1)
            .filter(|m| m.role == Role::User)
            .count();
        if user_count < self.memory_state.shutdown_summary_min_messages {
            tracing::debug!(
                user_count,
                min = self.memory_state.shutdown_summary_min_messages,
                "shutdown summary: too few user messages, skipping"
            );
            return;
        }

        // TUI status — send errors silently ignored (TUI may already be gone at shutdown).
        let _ = self.channel.send_status("Saving session summary...").await;

        // Collect last N messages (skip system prompt at index 0).
        let max = self.memory_state.shutdown_summary_max_messages;
        if max == 0 {
            tracing::debug!("shutdown summary: max_messages=0, skipping");
            return;
        }
        let non_system: Vec<_> = self.msg.messages.iter().skip(1).collect();
        let slice = if non_system.len() > max {
            &non_system[non_system.len() - max..]
        } else {
            &non_system[..]
        };

        let msgs_for_prompt: Vec<(zeph_memory::MessageId, String, String)> = slice
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::User => "user".to_owned(),
                    Role::Assistant => "assistant".to_owned(),
                    Role::System => "system".to_owned(),
                };
                (zeph_memory::MessageId(0), role, m.content.clone())
            })
            .collect();

        let prompt = zeph_memory::build_summarization_prompt(&msgs_for_prompt);
        let chat_messages = vec![Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let Some(structured) = self.call_llm_for_session_summary(&chat_messages).await else {
            let _ = self.channel.send_status("").await;
            return;
        };

        if let Err(e) = memory
            .store_shutdown_summary(conversation_id, &structured.summary, &structured.key_facts)
            .await
        {
            tracing::warn!("shutdown summary: storage failed: {e:#}");
        } else {
            tracing::info!(
                conversation_id = conversation_id.0,
                "shutdown summary stored"
            );
        }

        // Clear TUI status.
        let _ = self.channel.send_status("").await;
    }

    pub async fn shutdown(&mut self) {
        self.channel.send("Shutting down...").await.ok();

        // CRIT-1: persist Thompson state accumulated during this session.
        self.provider.save_router_state();

        if let Some(ref mut mgr) = self.orchestration.subagent_manager {
            mgr.shutdown_all();
        }

        if let Some(ref manager) = self.mcp.manager {
            manager.shutdown_all_shared().await;
        }

        // Finalize compaction trajectory: push the last open segment into the Vec.
        // This segment would otherwise only be pushed when the next hard compaction fires,
        // which never happens at session end.
        if let Some(turns) = self.context_manager.turns_since_last_hard_compaction {
            self.update_metrics(|m| {
                m.compaction_turns_after_hard.push(turns);
            });
            self.context_manager.turns_since_last_hard_compaction = None;
        }

        if let Some(ref tx) = self.metrics.metrics_tx {
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
            if m.compaction_hard_count > 0 {
                tracing::info!(
                    hard_compactions = m.compaction_hard_count,
                    turns_after_hard = ?m.compaction_turns_after_hard,
                    "hard compaction trajectory"
                );
            }
        }

        self.maybe_store_shutdown_summary().await;
        self.maybe_store_session_digest().await;

        tracing::info!("agent shutdown complete");
    }

    /// Run the chat loop, receiving messages via the channel until EOF or shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if channel I/O or LLM communication fails.
    /// Refresh sub-agent metrics snapshot for the TUI metrics panel.
    fn refresh_subagent_metrics(&mut self) {
        let Some(ref mgr) = self.orchestration.subagent_manager else {
            return;
        };
        let sub_agent_metrics: Vec<crate::metrics::SubAgentMetrics> = mgr
            .statuses()
            .into_iter()
            .map(|(id, s)| {
                let def = mgr.agents_def(&id);
                crate::metrics::SubAgentMetrics {
                    name: def.map_or_else(|| id[..8.min(id.len())].to_owned(), |d| d.name.clone()),
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
                            PermissionMode::BypassPermissions => "bypass_permissions".into(),
                            PermissionMode::Plan => "plan".into(),
                        }
                    }),
                }
            })
            .collect();
        self.update_metrics(|m| m.sub_agents = sub_agent_metrics);
    }

    /// Non-blocking poll: notify the user when background sub-agents complete.
    async fn notify_completed_subagents(&mut self) -> Result<(), error::AgentError> {
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
        Ok(())
    }

    /// Run the agent main loop.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel, LLM provider, or tool execution encounters a fatal error.
    pub async fn run(&mut self) -> Result<(), error::AgentError> {
        if let Some(mut rx) = self.lifecycle.warmup_ready.take()
            && !*rx.borrow()
        {
            let _ = rx.changed().await;
            if !*rx.borrow() {
                tracing::warn!("model warmup did not complete successfully");
            }
        }

        // Load the session digest once at session start for context injection.
        self.load_and_cache_session_digest().await;

        loop {
            // Apply any pending provider override (from ACP set_session_config_option).
            if let Some(ref slot) = self.providers.provider_override
                && let Some(new_provider) = slot
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            {
                tracing::debug!(provider = new_provider.name(), "ACP model override applied");
                self.provider = new_provider;
            }

            // Poll for MCP tool list updates from tools/list_changed notifications.
            self.check_tool_refresh().await;

            // Refresh sub-agent status in metrics before polling.
            self.refresh_subagent_metrics();

            // Non-blocking poll: notify user when background sub-agents complete.
            self.notify_completed_subagents().await?;

            self.drain_channel();

            let (text, image_parts) = if let Some(queued) = self.msg.message_queue.pop_front() {
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
                    () = shutdown_signal(&mut self.lifecycle.shutdown) => {
                        tracing::info!("shutting down");
                        break;
                    }
                    Some(_) = recv_optional(&mut self.skill_state.skill_reload_rx) => {
                        self.reload_skills().await;
                        continue;
                    }
                    Some(_) = recv_optional(&mut self.instructions.reload_rx) => {
                        self.reload_instructions();
                        continue;
                    }
                    Some(_) = recv_optional(&mut self.lifecycle.config_reload_rx) => {
                        self.reload_config();
                        continue;
                    }
                    Some(msg) = recv_optional(&mut self.lifecycle.update_notify_rx) => {
                        if let Err(e) = self.channel.send(&msg).await {
                            tracing::warn!("failed to send update notification: {e}");
                        }
                        continue;
                    }
                    Some(msg) = recv_optional(&mut self.experiments.notify_rx) => {
                        // Experiment engine completed (ok or err). Clear the cancel token so
                        // status reports idle and new experiments can be started.
                        #[cfg(feature = "experiments")]
                        { self.experiments.cancel = None; }
                        if let Err(e) = self.channel.send(&msg).await {
                            tracing::warn!("failed to send experiment completion: {e}");
                        }
                        continue;
                    }
                    Some(prompt) = recv_optional(&mut self.lifecycle.custom_task_rx) => {
                        tracing::info!("scheduler: injecting custom task as agent turn");
                        let text = format!("{SCHEDULED_TASK_PREFIX}{prompt}");
                        Some(crate::channel::ChannelMessage { text, attachments: Vec::new() })
                    }
                };
                let Some(msg) = incoming else { break };
                self.drain_channel();
                self.resolve_message(msg).await
            };

            let trimmed = text.trim();

            match self.handle_builtin_command(trimmed).await? {
                Some(true) => break,
                Some(false) => continue,
                None => {}
            }

            self.process_user_message(text, image_parts).await?;
        }

        // Flush trace collector on normal exit (C-04: Drop handles error/panic paths).
        if let Some(ref mut tc) = self.debug_state.trace_collector {
            tc.finish();
        }

        Ok(())
    }

    /// Handle built-in slash commands that short-circuit the main `run` loop.
    ///
    /// Returns `Some(true)` to break the loop (exit), `Some(false)` to continue to the next
    /// iteration, or `None` if the command was not recognized (caller should call
    /// `process_user_message`).
    async fn handle_builtin_command(
        &mut self,
        trimmed: &str,
    ) -> Result<Option<bool>, error::AgentError> {
        if trimmed == "/clear-queue" {
            let n = self.clear_queue();
            self.notify_queue_count().await;
            self.channel
                .send(&format!("Cleared {n} queued messages."))
                .await?;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/compact" {
            if self.msg.messages.len() > self.context_manager.compaction_preserve_tail + 1 {
                match self.compact_context().await {
                    Ok(
                        context::CompactionOutcome::Compacted
                        | context::CompactionOutcome::NoChange,
                    ) => {
                        let _ = self.channel.send("Context compacted successfully.").await;
                    }
                    Ok(context::CompactionOutcome::ProbeRejected) => {
                        let _ = self
                            .channel
                            .send(
                                "Compaction rejected: summary quality below threshold. \
                                 Original context preserved.",
                            )
                            .await;
                    }
                    Err(e) => {
                        let _ = self.channel.send(&format!("Compaction failed: {e}")).await;
                    }
                }
            } else {
                let _ = self.channel.send("Nothing to compact.").await;
            }
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/clear" {
            self.clear_history();
            self.tool_orchestrator.clear_cache();
            if let Ok(mut urls) = self.security.user_provided_urls.write() {
                urls.clear();
            }
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/reset" {
            self.clear_history();
            self.tool_orchestrator.clear_cache();
            if let Ok(mut urls) = self.security.user_provided_urls.write() {
                urls.clear();
            }
            self.channel.send("Conversation history reset.").await?;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/cache-stats" {
            let stats = self.tool_orchestrator.cache_stats();
            self.channel.send(&stats).await?;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/model" || trimmed.starts_with("/model ") {
            self.handle_model_command(trimmed).await;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/provider" || trimmed.starts_with("/provider ") {
            self.handle_provider_command(trimmed).await;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/debug-dump" || trimmed.starts_with("/debug-dump ") {
            self.handle_debug_dump_command(trimmed).await;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed.starts_with("/dump-format") {
            self.handle_dump_format_command(trimmed).await;
            let _ = self.channel.flush_chunks().await;
            return Ok(Some(false));
        }

        if trimmed == "/exit" || trimmed == "/quit" {
            if self.channel.supports_exit() {
                return Ok(Some(true));
            }
            let _ = self
                .channel
                .send("/exit is not supported in this channel.")
                .await;
            return Ok(Some(false));
        }

        Ok(None)
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

    async fn handle_model_refresh(&mut self) {
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
    }

    async fn handle_model_list(&mut self) {
        let cache = zeph_llm::model_cache::ModelCache::for_slug(self.provider.name());
        let cached = if cache.is_stale() {
            None
        } else {
            cache.load().unwrap_or(None)
        };
        let models = if let Some(m) = cached {
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
    }

    async fn handle_model_switch(&mut self, model_id: &str) {
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

    /// Handle `/model`, `/model <id>`, and `/model refresh` commands.
    async fn handle_model_command(&mut self, trimmed: &str) {
        let arg = trimmed.strip_prefix("/model").map_or("", str::trim);
        if arg == "refresh" {
            self.handle_model_refresh().await;
        } else if arg.is_empty() {
            self.handle_model_list().await;
        } else {
            self.handle_model_switch(arg).await;
        }
    }

    /// Handle `/debug-dump` and `/debug-dump <path>` commands.
    async fn handle_debug_dump_command(&mut self, trimmed: &str) {
        let arg = trimmed.strip_prefix("/debug-dump").map_or("", str::trim);
        if arg.is_empty() {
            match &self.debug_state.debug_dumper {
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
        match crate::debug_dump::DebugDumper::new(&dir, self.debug_state.dump_format) {
            Ok(dumper) => {
                let path = dumper.dir().display().to_string();
                self.debug_state.debug_dumper = Some(dumper);
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

    /// Handle `/dump-format <json|raw|trace>` command — switch debug dump format at runtime.
    async fn handle_dump_format_command(&mut self, trimmed: &str) {
        let arg = trimmed.strip_prefix("/dump-format").map_or("", str::trim);
        if arg.is_empty() {
            let _ = self
                .channel
                .send(&format!(
                    "Current dump format: {:?}. Use `/dump-format json|raw|trace` to change.",
                    self.debug_state.dump_format
                ))
                .await;
            return;
        }
        let new_format = match arg {
            "json" => crate::debug_dump::DumpFormat::Json,
            "raw" => crate::debug_dump::DumpFormat::Raw,
            "trace" => crate::debug_dump::DumpFormat::Trace,
            other => {
                let _ = self
                    .channel
                    .send(&format!(
                        "Unknown format '{other}'. Valid values: json, raw, trace."
                    ))
                    .await;
                return;
            }
        };
        let was_trace = self.debug_state.dump_format == crate::debug_dump::DumpFormat::Trace;
        let now_trace = new_format == crate::debug_dump::DumpFormat::Trace;

        // CR-04: when switching TO trace, create a fresh TracingCollector.
        if now_trace
            && !was_trace
            && let Some(ref dump_dir) = self.debug_state.dump_dir.clone()
        {
            let service_name = self.debug_state.trace_service_name.clone();
            let redact = self.debug_state.trace_redact;
            match crate::debug_dump::trace::TracingCollector::new(
                dump_dir.as_path(),
                &service_name,
                redact,
                None,
            ) {
                Ok(collector) => {
                    self.debug_state.trace_collector = Some(collector);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to create TracingCollector on format switch");
                }
            }
        }
        // CR-04: when switching AWAY from trace, flush and drop the collector.
        if was_trace
            && !now_trace
            && let Some(mut tc) = self.debug_state.trace_collector.take()
        {
            tc.finish();
        }

        self.debug_state.dump_format = new_format;
        let _ = self
            .channel
            .send(&format!("Debug dump format set to: {arg}"))
            .await;
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
            has_stt = self.providers.stt.is_some(),
            "resolve_message attachments"
        );

        let text = if !audio_attachments.is_empty()
            && let Some(stt) = self.providers.stt.as_ref()
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

    /// Dispatch slash commands. Returns `Some(Ok(()))` when handled,
    /// `Some(Err(_))` on I/O error, `None` to fall through to LLM processing.
    #[allow(clippy::too_many_lines)]
    async fn dispatch_slash_command(
        &mut self,
        trimmed: &str,
    ) -> Option<Result<(), error::AgentError>> {
        macro_rules! handled {
            ($expr:expr) => {{
                if let Err(e) = $expr {
                    return Some(Err(e));
                }
                let _ = self.channel.flush_chunks().await;
                return Some(Ok(()));
            }};
        }

        // Slash command arguments may contain user-provided URLs (e.g. `/browse https://...`).
        // Extract them here so UrlGroundingVerifier allows follow-up fetch calls.
        let slash_urls = zeph_sanitizer::exfiltration::extract_flagged_urls(trimmed);
        if !slash_urls.is_empty()
            && let Ok(mut set) = self.security.user_provided_urls.write()
        {
            set.extend(slash_urls);
        }

        if trimmed == "/help" {
            handled!(self.handle_help_command().await);
        }

        if trimmed == "/status" {
            handled!(self.handle_status_command().await);
        }

        #[cfg(feature = "guardrail")]
        if trimmed == "/guardrail" {
            handled!(self.handle_guardrail_command().await);
        }

        if trimmed == "/skills" {
            handled!(self.handle_skills_command().await);
        }

        if trimmed == "/skill" || trimmed.starts_with("/skill ") {
            let rest = trimmed
                .strip_prefix("/skill")
                .unwrap_or("")
                .trim()
                .to_owned();
            handled!(self.handle_skill_command(&rest).await);
        }

        if trimmed == "/feedback" || trimmed.starts_with("/feedback ") {
            let rest = trimmed
                .strip_prefix("/feedback")
                .unwrap_or("")
                .trim()
                .to_owned();
            handled!(self.handle_feedback(&rest).await);
        }

        if trimmed == "/mcp" || trimmed.starts_with("/mcp ") {
            let args = trimmed.strip_prefix("/mcp").unwrap_or("").trim().to_owned();
            handled!(self.handle_mcp_command(&args).await);
        }

        if trimmed == "/image" || trimmed.starts_with("/image ") {
            let path = trimmed
                .strip_prefix("/image")
                .unwrap_or("")
                .trim()
                .to_owned();
            if path.is_empty() {
                handled!(
                    self.channel
                        .send("Usage: /image <path>")
                        .await
                        .map_err(Into::into)
                );
            }
            handled!(self.handle_image_command(&path).await);
        }

        if trimmed == "/plan" || trimmed.starts_with("/plan ") {
            return Some(self.dispatch_plan_command(trimmed).await);
        }

        if trimmed == "/graph" || trimmed.starts_with("/graph ") {
            handled!(self.handle_graph_command(trimmed).await);
        }

        if trimmed == "/memory" || trimmed.starts_with("/memory ") {
            handled!(self.handle_memory_command(trimmed).await);
        }

        #[cfg(feature = "compression-guidelines")]
        if trimmed == "/guidelines" {
            handled!(self.handle_guidelines_command().await);
        }

        #[cfg(feature = "scheduler")]
        if trimmed == "/scheduler" || trimmed.starts_with("/scheduler ") {
            handled!(self.handle_scheduler_command(trimmed).await);
        }

        #[cfg(feature = "experiments")]
        if trimmed == "/experiment" || trimmed.starts_with("/experiment ") {
            handled!(self.handle_experiment_command(trimmed).await);
        }

        #[cfg(feature = "lsp-context")]
        if trimmed == "/lsp" {
            handled!(self.handle_lsp_status_command().await);
        }

        #[cfg(feature = "policy-enforcer")]
        if trimmed == "/policy" || trimmed.starts_with("/policy ") {
            let args = trimmed
                .strip_prefix("/policy")
                .unwrap_or("")
                .trim()
                .to_owned();
            handled!(self.handle_policy_command(&args).await);
        }

        if trimmed == "/log" {
            handled!(self.handle_log_command().await);
        }

        if trimmed.starts_with("/agent") || trimmed.starts_with('@') {
            return self.dispatch_agent_command(trimmed).await;
        }

        #[cfg(feature = "context-compression")]
        if trimmed == "/focus" {
            handled!(self.handle_focus_status_command().await);
        }

        #[cfg(feature = "context-compression")]
        if trimmed == "/sidequest" {
            handled!(self.handle_sidequest_status_command().await);
        }

        None
    }

    async fn dispatch_plan_command(&mut self, trimmed: &str) -> Result<(), error::AgentError> {
        match crate::orchestration::PlanCommand::parse(trimmed) {
            Ok(cmd) => {
                self.handle_plan_command(cmd).await?;
            }
            Err(e) => {
                self.channel
                    .send(&e.to_string())
                    .await
                    .map_err(error::AgentError::from)?;
            }
        }
        let _ = self.channel.flush_chunks().await;
        Ok(())
    }

    async fn dispatch_agent_command(
        &mut self,
        trimmed: &str,
    ) -> Option<Result<(), error::AgentError>> {
        let known: Vec<String> = self
            .orchestration
            .subagent_manager
            .as_ref()
            .map(|m| m.definitions().iter().map(|d| d.name.clone()).collect())
            .unwrap_or_default();
        match crate::subagent::AgentCommand::parse(trimmed, &known) {
            Ok(cmd) => {
                if let Some(msg) = self.handle_agent_command(cmd).await
                    && let Err(e) = self.channel.send(&msg).await
                {
                    return Some(Err(e.into()));
                }
                let _ = self.channel.flush_chunks().await;
                Some(Ok(()))
            }
            Err(e) if trimmed.starts_with('@') => {
                // Unknown @token — fall through to normal LLM processing
                tracing::debug!("@mention not matched as agent: {e}");
                None
            }
            Err(e) => {
                if let Err(send_err) = self.channel.send(&e.to_string()).await {
                    return Some(Err(send_err.into()));
                }
                let _ = self.channel.flush_chunks().await;
                Some(Ok(()))
            }
        }
    }

    /// Spawn a background task to evaluate the user message with the LLM judge (or `LlmClassifier`)
    /// and store the correction result. Non-blocking: the task runs independently of the response
    /// pipeline.
    ///
    /// # Notes
    ///
    /// TODO(I3): `JoinHandle`s are not tracked — outstanding tasks may be aborted on runtime
    /// shutdown before `store_user_correction` completes. Acceptable for MVP.
    #[allow(clippy::too_many_lines)]
    fn spawn_judge_correction_check(
        &mut self,
        trimmed: &str,
        conv_id: Option<zeph_memory::ConversationId>,
    ) {
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
        let confidence_threshold = self
            .learning_engine
            .config
            .as_ref()
            .map_or(0.6, |c| c.correction_confidence_threshold);

        if let Some(llm_classifier) = self.feedback.llm_classifier.clone() {
            // DetectorMode::Model: clone the classifier (cheap — it holds Arc<AnyProvider>).
            let user_msg = user_msg_owned.clone();
            let assistant = assistant_snippet.clone();
            let memory_arc2 = memory_arc.clone();
            let skill_name2 = skill_name.clone();
            // Clone metrics handles for use inside the spawned task.
            let classifier_metrics_bg = self.metrics.classifier_metrics.clone();
            let metrics_tx_bg = self.metrics.metrics_tx.clone();
            tokio::spawn(async move {
                match llm_classifier
                    .classify_feedback(&user_msg, &assistant, confidence_threshold)
                    .await
                {
                    Ok(verdict) => {
                        // Push classifier snapshot after feedback classification.
                        if let (Some(ref cm), Some(ref tx)) = (classifier_metrics_bg, metrics_tx_bg)
                        {
                            let snap = cm.snapshot();
                            tx.send_modify(|ms| ms.classifier = snap);
                        }
                        if let Some(signal) = feedback_verdict_into_signal(&verdict, &user_msg) {
                            let is_self_correction =
                                signal.kind == feedback_detector::CorrectionKind::SelfCorrection;
                            tracing::info!(
                                kind = signal.kind.as_str(),
                                confidence = signal.confidence,
                                source = "llm-classifier",
                                is_self_correction,
                                "correction signal detected"
                            );
                            store_correction_in_memory(
                                memory_arc2,
                                conv_id_bg,
                                &assistant,
                                &user_msg,
                                skill_name2,
                                signal.kind.as_str(),
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("llm-classifier failed: {e:#}");
                    }
                }
            });
        } else {
            // DetectorMode::Judge (legacy path).
            let judge_provider = self
                .providers
                .judge_provider
                .clone()
                .unwrap_or_else(|| self.provider.clone());
            let user_msg = user_msg_owned.clone();
            let assistant = assistant_snippet.clone();
            tokio::spawn(async move {
                match feedback_detector::JudgeDetector::evaluate(
                    &judge_provider,
                    &user_msg,
                    &assistant,
                    confidence_threshold,
                )
                .await
                {
                    Ok(verdict) => {
                        if let Some(signal) = verdict.into_signal(&user_msg) {
                            // Self-corrections (user corrects their own statement) must not
                            // penalize skills. The judge path has no record_skill_outcomes()
                            // call today, but this guard mirrors the regex path to make the
                            // intent explicit and prevent future regressions if parity is added.
                            let is_self_correction =
                                signal.kind == feedback_detector::CorrectionKind::SelfCorrection;
                            tracing::info!(
                                kind = signal.kind.as_str(),
                                confidence = signal.confidence,
                                source = "judge",
                                is_self_correction,
                                "correction signal detected"
                            );
                            store_correction_in_memory(
                                memory_arc,
                                conv_id_bg,
                                &assistant,
                                &user_msg,
                                skill_name,
                                signal.kind.as_str(),
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("judge detector failed: {e:#}");
                    }
                }
            });
        }
    }

    /// Detect implicit corrections in the user's message and record them in the learning engine.
    ///
    /// Uses regex-based `FeedbackDetector` first. If a `JudgeDetector` is configured and the
    /// regex result is borderline, the LLM judge runs in a background task (non-blocking).
    /// When `DetectorMode::Model` and an `LlmClassifier` is attached, the LLM classifier is
    /// used instead of `JudgeDetector`, sharing the same adaptive thresholds and rate limiter.
    #[allow(clippy::too_many_lines)]
    async fn detect_and_record_corrections(
        &mut self,
        trimmed: &str,
        conv_id: Option<zeph_memory::ConversationId>,
    ) {
        let correction_detection_enabled = self
            .learning_engine
            .config
            .as_ref()
            .is_none_or(|c| c.correction_detection);
        if !correction_detection_enabled {
            return;
        }

        let previous_user_messages: Vec<&str> = self
            .msg
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .collect();

        let regex_signal = self
            .feedback
            .detector
            .detect(trimmed, &previous_user_messages);

        // Judge/Model mode: invoke LLM in background if regex is borderline or missed.
        //
        // The LLM call is decoupled from the response pipeline — it records the
        // correction asynchronously via tokio::spawn and returns None immediately
        // so the user response is not blocked.
        //
        // TODO(I3): JoinHandles are not tracked — outstanding tasks may be aborted
        // on runtime shutdown before store_user_correction completes. This is
        // acceptable for the learning subsystem at MVP. Future: collect handles in
        // Agent and drain on graceful shutdown.
        // Check rate limit synchronously before deciding to spawn.
        // The feedback.judge is &mut self so check_rate_limit() can update call_times.
        //
        // DetectorMode::Model reuses the judge's adaptive thresholds + rate limiter.
        // If llm_classifier is present but judge is None, create a temporary JudgeDetector
        // for threshold/rate-limit checking only (not for actual LLM calls).
        let judge_should_run = if self.feedback.llm_classifier.is_some() {
            // Model mode: use judge thresholds + rate limiter for gating.
            let adaptive_low = self
                .learning_engine
                .config
                .as_ref()
                .map_or(0.5, |c| c.judge_adaptive_low);
            let adaptive_high = self
                .learning_engine
                .config
                .as_ref()
                .map_or(0.8, |c| c.judge_adaptive_high);
            let should_invoke = self
                .feedback
                .judge
                .get_or_insert_with(|| {
                    feedback_detector::JudgeDetector::new(adaptive_low, adaptive_high)
                })
                .should_invoke(regex_signal.as_ref());
            should_invoke
                && self
                    .feedback
                    .judge
                    .as_mut()
                    .is_some_and(feedback_detector::JudgeDetector::check_rate_limit)
        } else {
            // Judge mode (or regex-only when neither judge nor llm_classifier is set).
            self.feedback
                .judge
                .as_ref()
                .is_some_and(|jd| jd.should_invoke(regex_signal.as_ref()))
                && self
                    .feedback
                    .judge
                    .as_mut() // lgtm[rust/cleartext-logging]
                    .is_some_and(feedback_detector::JudgeDetector::check_rate_limit)
        };

        let (signal, signal_source) = if judge_should_run {
            self.spawn_judge_correction_check(trimmed, conv_id);
            // Judge runs in background — return None so the response pipeline continues.
            (None, "judge")
        } else {
            (regex_signal, "regex")
        };

        let Some(signal) = signal else { return };
        tracing::info!(
            kind = signal.kind.as_str(),
            confidence = signal.confidence,
            source = signal_source,
            "implicit correction detected"
        );
        // REV-PH2-002 + SEC-PH2-002: cap feedback_text to 500 chars (UTF-8 safe)
        let feedback_text = context::truncate_chars(&signal.feedback_text, 500);
        // Self-corrections (user corrects their own statement) must not penalize skills —
        // the agent did nothing wrong. Store for analytics but skip skill outcome recording.
        if self.is_learning_enabled()
            && signal.kind != feedback_detector::CorrectionKind::SelfCorrection
        {
            self.record_skill_outcomes(
                "user_rejection",
                Some(&feedback_text),
                Some(signal.kind.as_str()),
            )
            .await;
        }
        if let Some(memory) = &self.memory_state.memory {
            // Use `trimmed` (raw user input, untainted by secrets) instead of
            // `feedback_text` (derived from previous_user_messages → self.msg.messages)
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

    async fn process_user_message(
        &mut self,
        text: String,
        image_parts: Vec<zeph_llm::provider::MessagePart>,
    ) -> Result<(), error::AgentError> {
        // Record iteration start in trace collector (C-02: owned guard, no borrow held).
        let iteration_index = self.debug_state.iteration_counter;
        self.debug_state.iteration_counter += 1;
        if let Some(ref mut tc) = self.debug_state.trace_collector {
            tc.begin_iteration(iteration_index, text.trim());
            // CR-01: store the span ID so LLM/tool execution can attach child spans.
            self.debug_state.current_iteration_span_id =
                tc.current_iteration_span_id(iteration_index);
        }

        let result = self
            .process_user_message_inner(text, image_parts, iteration_index)
            .await;

        // Close iteration span regardless of outcome (partial trace preserved on error).
        if let Some(ref mut tc) = self.debug_state.trace_collector {
            let status = if result.is_ok() {
                crate::debug_dump::trace::SpanStatus::Ok
            } else {
                crate::debug_dump::trace::SpanStatus::Error {
                    message: "iteration failed".to_owned(),
                }
            };
            tc.end_iteration(iteration_index, status);
        }
        self.debug_state.current_iteration_span_id = None;

        result
    }

    #[allow(clippy::too_many_lines)]
    async fn process_user_message_inner(
        &mut self,
        text: String,
        image_parts: Vec<zeph_llm::provider::MessagePart>,
        iteration_index: usize,
    ) -> Result<(), error::AgentError> {
        let _ = iteration_index; // Used indirectly via debug_state.current_iteration_span_id.
        self.lifecycle.cancel_token = CancellationToken::new();
        let signal = Arc::clone(&self.lifecycle.cancel_signal);
        let token = self.lifecycle.cancel_token.clone();
        tokio::spawn(async move {
            signal.notified().await;
            token.cancel();
        });
        let trimmed = text.trim();

        if let Some(result) = self.dispatch_slash_command(trimmed).await {
            return result;
        }

        self.check_pending_rollbacks().await;

        // Guardrail: LLM-based prompt injection pre-screening at the user input boundary.
        #[cfg(feature = "guardrail")]
        if let Some(ref guardrail) = self.security.guardrail {
            use zeph_sanitizer::guardrail::GuardrailVerdict;
            let verdict = guardrail.check(trimmed).await;
            match &verdict {
                GuardrailVerdict::Flagged { reason, .. } => {
                    tracing::warn!(
                        reason = %reason,
                        should_block = verdict.should_block(),
                        "guardrail flagged user input"
                    );
                    if verdict.should_block() {
                        let msg = format!("[guardrail] Input blocked: {reason}");
                        let _ = self.channel.send(&msg).await;
                        let _ = self.channel.flush_chunks().await;
                        return Ok(());
                    }
                    // Warn mode: notify but continue.
                    let _ = self
                        .channel
                        .send(&format!("[guardrail] Warning: {reason}"))
                        .await;
                }
                GuardrailVerdict::Error { error } => {
                    if guardrail.error_should_block() {
                        tracing::warn!(%error, "guardrail check failed (fail_strategy=closed), blocking input");
                        let msg = "[guardrail] Input blocked: check failed (see logs for details)";
                        let _ = self.channel.send(msg).await;
                        let _ = self.channel.flush_chunks().await;
                        return Ok(());
                    }
                    tracing::warn!(%error, "guardrail check failed (fail_strategy=open), allowing input");
                }
                GuardrailVerdict::Safe => {}
            }
        }

        // ML classifier: lightweight injection detection on user input boundary.
        // Runs after guardrail (LLM-based) to layer defenses. On detection, blocks and returns.
        // Falls back to regex on classifier error/timeout — never degrades below regex baseline.
        // Gated by `scan_user_input`: DeBERTa is tuned for external/untrusted content, not
        // direct user chat. Disabled by default to prevent false positives on benign messages.
        #[cfg(feature = "classifiers")]
        if self.security.sanitizer.scan_user_input() {
            match self.security.sanitizer.classify_injection(trimmed).await {
                zeph_sanitizer::InjectionVerdict::Blocked => {
                    self.push_classifier_metrics();
                    let _ = self
                        .channel
                        .send("[security] Input blocked: injection detected by classifier.")
                        .await;
                    let _ = self.channel.flush_chunks().await;
                    return Ok(());
                }
                zeph_sanitizer::InjectionVerdict::Suspicious => {
                    tracing::warn!("injection_classifier soft_signal on user input");
                }
                zeph_sanitizer::InjectionVerdict::Clean => {}
            }
        }
        #[cfg(feature = "classifiers")]
        self.push_classifier_metrics();

        // Reset per-message pruning cache at the start of each turn (#2298).
        self.mcp.pruning_cache.reset();

        // Extract before rebuild_system_prompt so the value is not tainted
        // by the secrets-bearing system prompt (ConversationId is just an i64).
        let conv_id = self.memory_state.conversation_id;
        self.rebuild_system_prompt(&text).await;

        self.detect_and_record_corrections(trimmed, conv_id).await;
        self.learning_engine.tick();
        self.analyze_and_learn().await;
        self.sync_graph_counts().await;

        // Reset per-turn compaction guard FIRST so SideQuest sees a clean slate (C2 fix).
        // complete_focus and maybe_sidequest_eviction set this flag when they run (C1 fix).
        // advance_turn() transitions CompactedThisTurn → Cooling/Ready; all other states
        // pass through unchanged. See CompactionState::advance_turn for ordering guarantees.
        self.context_manager.compaction = self.context_manager.compaction.advance_turn();

        // Tick Focus Agent and SideQuest turn counters (#1850, #1885).
        #[cfg(feature = "context-compression")]
        {
            self.focus.tick();

            // SideQuest eviction: runs every N user turns when enabled.
            // Skipped when is_compacted_this_turn (focus truncation or prior eviction ran).
            let sidequest_should_fire = self.sidequest.tick();
            if sidequest_should_fire && !self.context_manager.compaction.is_compacted_this_turn() {
                self.maybe_sidequest_eviction();
            }
        }

        // Tier 0: batch-apply deferred tool summaries when approaching context limit.
        // This is a pure in-memory operation (no LLM call) — summaries were pre-computed
        // during the tool loop. Intentionally does NOT set compacted_this_turn, so
        // proactive/reactive compaction may still fire if tokens remain above their thresholds.
        self.maybe_apply_deferred_summaries();
        self.flush_deferred_summaries().await;

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

        let mut all_image_parts = std::mem::take(&mut self.msg.pending_image_parts);
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
        // Extract URLs from user input and add to user_provided_urls for grounding checks.
        let urls = zeph_sanitizer::exfiltration::extract_flagged_urls(trimmed);
        if !urls.is_empty()
            && let Ok(mut set) = self.security.user_provided_urls.write()
        {
            set.extend(urls);
        }

        // Image parts intentionally excluded — base64 payloads too large for message history.
        self.persist_message(Role::User, &text, &[], false).await;
        self.push_message(user_msg);

        if let Err(e) = self.process_response().await {
            tracing::error!("Response processing failed: {e:#}");
            let user_msg = format!("Error: {e:#}");
            self.channel.send(&user_msg).await?;
            self.msg.messages.pop();
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
            let _ = self.channel.flush_chunks().await;
            return Ok(());
        }

        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                self.channel
                    .send(&format!("Cannot read image {path}: {e}"))
                    .await?;
                let _ = self.channel.flush_chunks().await;
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
            let _ = self.channel.flush_chunks().await;
            return Ok(());
        }
        let mime_type = detect_image_mime(Some(path)).to_string();
        self.msg
            .pending_image_parts
            .push(MessagePart::Image(Box::new(ImageData { data, mime_type })));
        self.channel
            .send(&format!("Image loaded: {path}. Send your message."))
            .await?;
        let _ = self.channel.flush_chunks().await;
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

    #[allow(clippy::too_many_lines)]
    async fn handle_status_command(&mut self) -> Result<(), error::AgentError> {
        use std::fmt::Write;

        let uptime = self.lifecycle.start_time.elapsed().as_secs();
        let msg_count = self
            .msg
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count();

        let (
            api_calls,
            prompt_tokens,
            completion_tokens,
            cost_cents,
            mcp_servers,
            orch_plans,
            orch_tasks,
            orch_completed,
            orch_failed,
            orch_skipped,
        ) = if let Some(ref tx) = self.metrics.metrics_tx {
            let m = tx.borrow();
            (
                m.api_calls,
                m.prompt_tokens,
                m.completion_tokens,
                m.cost_spent_cents,
                m.mcp_server_count,
                m.orchestration.plans_total,
                m.orchestration.tasks_total,
                m.orchestration.tasks_completed,
                m.orchestration.tasks_failed,
                m.orchestration.tasks_skipped,
            )
        } else {
            (0, 0, 0, 0.0, 0, 0, 0, 0, 0, 0)
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
        if let Some(ref tf) = self.tool_schema_filter {
            let _ = writeln!(
                out,
                "Filter:    enabled (top_k={}, always_on={}, {} embeddings)",
                tf.top_k(),
                tf.always_on_count(),
                tf.embedding_count(),
            );
        }
        if cost_cents > 0.0 {
            let _ = writeln!(out, "Cost:      ${:.4}", cost_cents / 100.0);
        }
        if orch_plans > 0 {
            let _ = writeln!(out);
            let _ = writeln!(out, "Orchestration:");
            let _ = writeln!(out, "  Plans:     {orch_plans}");
            let _ = writeln!(out, "  Tasks:     {orch_completed}/{orch_tasks} completed");
            if orch_failed > 0 {
                let _ = writeln!(out, "  Failed:    {orch_failed}");
            }
            if orch_skipped > 0 {
                let _ = writeln!(out, "  Skipped:   {orch_skipped}");
            }
        }

        // Subgoal display (#2022): show active subgoal when a subgoal strategy is active.
        #[cfg(feature = "context-compression")]
        {
            use crate::config::PruningStrategy;
            if matches!(
                self.context_manager.compression.pruning_strategy,
                PruningStrategy::Subgoal | PruningStrategy::SubgoalMig
            ) {
                let _ = writeln!(out);
                let _ = writeln!(
                    out,
                    "Pruning:   {}",
                    match self.context_manager.compression.pruning_strategy {
                        PruningStrategy::SubgoalMig => "subgoal_mig",
                        _ => "subgoal",
                    }
                );
                let subgoal_count = self.compression.subgoal_registry.subgoals.len();
                let _ = writeln!(out, "Subgoals:  {subgoal_count} tracked");
                if let Some(active) = self.compression.subgoal_registry.active_subgoal() {
                    let _ = writeln!(out, "Active:    \"{}\"", active.description);
                } else {
                    let _ = writeln!(out, "Active:    (none yet)");
                }
            }
        }

        // Graph memory status: show recall mode when graph memory is enabled.
        let gc = &self.memory_state.graph_config;
        if gc.enabled {
            let _ = writeln!(out);
            if gc.spreading_activation.enabled {
                let _ = writeln!(
                    out,
                    "Graph recall: spreading activation (lambda={:.2}, hops={})",
                    gc.spreading_activation.decay_lambda, gc.spreading_activation.max_hops,
                );
            } else {
                let _ = writeln!(out, "Graph recall: BFS (hops={})", gc.max_hops,);
            }
        }

        self.channel.send(out.trim_end()).await?;
        Ok(())
    }

    #[cfg(feature = "guardrail")]
    async fn handle_guardrail_command(&mut self) -> Result<(), error::AgentError> {
        use std::fmt::Write;

        let mut out = String::new();
        if let Some(ref guardrail) = self.security.guardrail {
            let stats = guardrail.stats();
            let _ = writeln!(out, "Guardrail: enabled");
            let _ = writeln!(out, "Action:    {:?}", guardrail.action());
            let _ = writeln!(out, "Fail strategy: {:?}", guardrail.fail_strategy());
            let _ = writeln!(out, "Timeout:   {}ms", guardrail.timeout_ms());
            let _ = writeln!(
                out,
                "Tool scan: {}",
                if guardrail.scan_tool_output() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            let _ = writeln!(out, "\nStats:");
            let _ = writeln!(out, "  Total checks:  {}", stats.total_checks);
            let _ = writeln!(out, "  Flagged:       {}", stats.flagged_count);
            let _ = writeln!(out, "  Errors:        {}", stats.error_count);
            let _ = writeln!(out, "  Avg latency:   {}ms", stats.avg_latency_ms());
        } else {
            out.push_str("Guardrail: disabled\n");
            out.push_str(
                "Enable with: --guardrail flag or [security.guardrail] enabled = true in config",
            );
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

        let outcome_type = if self.feedback.detector.detect(feedback, &[]).is_some() {
            "user_rejection"
        } else {
            "user_approval"
        };

        memory
            .sqlite()
            .record_skill_outcome(
                skill_name,
                None,
                self.memory_state.conversation_id,
                outcome_type,
                None,
                Some(feedback),
            )
            .await?;

        if self.is_learning_enabled() && outcome_type == "user_rejection" {
            self.generate_improved_skill(skill_name, feedback, "", Some(feedback))
                .await
                .ok();
        }

        self.channel
            .send(&format!("Feedback recorded for \"{skill_name}\"."))
            .await?;
        Ok(())
    }

    /// Poll a sub-agent until it reaches a terminal state, bridging secret requests to the
    /// channel. Returns a human-readable status string suitable for sending to the user.
    async fn poll_subagent_until_done(&mut self, task_id: &str, label: &str) -> Option<String> {
        use crate::subagent::SubAgentState;
        let result = loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // Bridge secret requests from sub-agent to channel.confirm().
            // Fetch the pending request first, then release the borrow before
            // calling channel.confirm() (which requires &mut self).
            #[allow(clippy::redundant_closure_for_method_calls)]
            let pending = self
                .orchestration
                .subagent_manager
                .as_mut()
                .and_then(|m| m.try_recv_secret_request());
            if let Some((req_task_id, req)) = pending {
                // req.secret_key is pre-validated to [a-zA-Z0-9_-] in manager.rs
                // (SEC-P1-02), so it is safe to embed in the prompt string.
                let confirm_prompt = format!(
                    "Sub-agent requests secret '{}'. Allow?",
                    crate::text::truncate_to_chars(&req.secret_key, 100)
                );
                let approved = self.channel.confirm(&confirm_prompt).await.unwrap_or(false);
                if let Some(mgr) = self.orchestration.subagent_manager.as_mut() {
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

            let mgr = self.orchestration.subagent_manager.as_ref()?;
            let statuses = mgr.statuses();
            let Some((_, status)) = statuses.iter().find(|(id, _)| id == task_id) else {
                break format!("{label} completed (no status available).");
            };
            match status.state {
                SubAgentState::Completed => {
                    let msg = status.last_message.clone().unwrap_or_else(|| "done".into());
                    break format!("{label} completed: {msg}");
                }
                SubAgentState::Failed => {
                    let msg = status
                        .last_message
                        .clone()
                        .unwrap_or_else(|| "unknown error".into());
                    break format!("{label} failed: {msg}");
                }
                SubAgentState::Canceled => {
                    break format!("{label} was cancelled.");
                }
                _ => {
                    let _ = self
                        .channel
                        .send_status(&format!(
                            "{label}: turn {}/{}",
                            status.turns_used,
                            self.orchestration
                                .subagent_manager
                                .as_ref()
                                .and_then(|m| m.agents_def(task_id))
                                .map_or(20, |d| d.permissions.max_turns)
                        ))
                        .await;
                }
            }
        };
        Some(result)
    }

    /// Resolve a unique full `task_id` from a prefix. Returns `None` if the manager is absent,
    /// `Some(Err(msg))` on ambiguity/not-found, `Some(Ok(full_id))` on success.
    fn resolve_agent_id_prefix(&mut self, prefix: &str) -> Option<Result<String, String>> {
        let mgr = self.orchestration.subagent_manager.as_mut()?;
        let full_ids: Vec<String> = mgr
            .statuses()
            .into_iter()
            .map(|(tid, _)| tid)
            .filter(|tid| tid.starts_with(prefix))
            .collect();
        Some(match full_ids.as_slice() {
            [] => Err(format!("No sub-agent with id prefix '{prefix}'")),
            [fid] => Ok(fid.clone()),
            _ => Err(format!(
                "Ambiguous id prefix '{prefix}': matches {} agents",
                full_ids.len()
            )),
        })
    }

    fn handle_agent_list(&self) -> Option<String> {
        use std::fmt::Write as _;
        let mgr = self.orchestration.subagent_manager.as_ref()?;
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

    fn handle_agent_status(&self) -> Option<String> {
        use std::fmt::Write as _;
        let mgr = self.orchestration.subagent_manager.as_ref()?;
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
                && let Ok(dir) = crate::subagent::memory::resolve_memory_dir(scope, &def.name)
            {
                let _ = writeln!(out, "       memory: {}", dir.display());
            }
        }
        Some(out)
    }

    fn handle_agent_approve(&mut self, id: &str) -> Option<String> {
        let full_id = match self.resolve_agent_id_prefix(id)? {
            Ok(fid) => fid,
            Err(msg) => return Some(msg),
        };
        let mgr = self.orchestration.subagent_manager.as_mut()?;
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

    fn handle_agent_deny(&mut self, id: &str) -> Option<String> {
        let full_id = match self.resolve_agent_id_prefix(id)? {
            Ok(fid) => fid,
            Err(msg) => return Some(msg),
        };
        let mgr = self.orchestration.subagent_manager.as_mut()?;
        match mgr.deny_secret(&full_id) {
            Ok(()) => Some(format!("Secret request denied for sub-agent '{full_id}'.")),
            Err(e) => Some(format!("Deny failed: {e}")),
        }
    }

    async fn handle_agent_command(&mut self, cmd: crate::subagent::AgentCommand) -> Option<String> {
        use crate::subagent::AgentCommand;

        match cmd {
            AgentCommand::List => self.handle_agent_list(),
            AgentCommand::Background { name, prompt } => {
                let provider = self.provider.clone();
                let tool_executor = Arc::clone(&self.tool_executor);
                let skills = self.filtered_skills_for(&name);
                let mgr = self.orchestration.subagent_manager.as_mut()?;
                let cfg = self.orchestration.subagent_config.clone();
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
                let mgr = self.orchestration.subagent_manager.as_mut()?;
                let cfg = self.orchestration.subagent_config.clone();
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
                let label = format!("Sub-agent '{name}'");
                self.poll_subagent_until_done(&task_id, &label).await
            }
            AgentCommand::Status => self.handle_agent_status(),
            AgentCommand::Cancel { id } => {
                let mgr = self.orchestration.subagent_manager.as_mut()?;
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
            AgentCommand::Approve { id } => self.handle_agent_approve(&id),
            AgentCommand::Deny { id } => self.handle_agent_deny(&id),
            AgentCommand::Resume { id, prompt } => {
                let cfg = self.orchestration.subagent_config.clone();
                // Resolve definition name from transcript meta before spawning so we can
                // look up skills by definition name rather than the UUID prefix (S1 fix).
                let def_name = {
                    let mgr = self.orchestration.subagent_manager.as_ref()?;
                    match mgr.def_name_for_resume(&id, &cfg) {
                        Ok(name) => name,
                        Err(e) => return Some(format!("Failed to resume sub-agent: {e}")),
                    }
                };
                let skills = self.filtered_skills_for(&def_name);
                let provider = self.provider.clone();
                let tool_executor = Arc::clone(&self.tool_executor);
                let mgr = self.orchestration.subagent_manager.as_mut()?;
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
                self.poll_subagent_until_done(&task_id, "Resumed sub-agent")
                    .await
            }
        }
    }

    fn filtered_skills_for(&self, agent_name: &str) -> Option<Vec<String>> {
        let mgr = self.orchestration.subagent_manager.as_ref()?;
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

    /// Update trust DB records for all reloaded skills.
    async fn update_trust_for_reloaded_skills(&self, all_meta: &[zeph_skills::loader::SkillMeta]) {
        let Some(ref memory) = self.memory_state.memory else {
            return;
        };
        let trust_cfg = self.skill_state.trust_config.clone();
        let managed_dir = self.skill_state.managed_dir.clone();
        for meta in all_meta {
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

    /// Rebuild or sync the in-memory skill matcher and BM25 index after a registry update.
    async fn rebuild_skill_matcher(&mut self, all_meta: &[&zeph_skills::loader::SkillMeta]) {
        let provider = self.embedding_provider.clone();
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
            self.skill_state.matcher = SkillMatcher::new(all_meta, embed_fn)
                .await
                .map(SkillMatcherBackend::InMemory);
        } else if let Some(ref mut backend) = self.skill_state.matcher {
            let _ = self.channel.send_status("syncing skill index...").await;
            if let Err(e) = backend
                .sync(all_meta, &self.skill_state.embedding_model, embed_fn)
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
    }

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

        self.update_trust_for_reloaded_skills(&all_meta).await;

        let all_meta_refs = all_meta.iter().collect::<Vec<_>>();
        self.rebuild_skill_matcher(&all_meta_refs).await;

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
        if let Some(msg) = self.msg.messages.first_mut() {
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
        if let Some(ref mut rx) = self.instructions.reload_rx {
            while rx.try_recv().is_ok() {}
        }
        let Some(ref state) = self.instructions.reload_state else {
            return;
        };
        let new_blocks = crate::instructions::load_instructions(
            &state.base_dir,
            &state.provider_kinds,
            &state.explicit_files,
            state.auto_detect,
        );
        let old_sources: std::collections::HashSet<_> =
            self.instructions.blocks.iter().map(|b| &b.source).collect();
        let new_sources: std::collections::HashSet<_> =
            new_blocks.iter().map(|b| &b.source).collect();
        for added in new_sources.difference(&old_sources) {
            tracing::info!(path = %added.display(), "instruction file added");
        }
        for removed in old_sources.difference(&new_sources) {
            tracing::info!(path = %removed.display(), "instruction file removed");
        }
        tracing::info!(
            old_count = self.instructions.blocks.len(),
            new_count = new_blocks.len(),
            "reloaded instruction files"
        );
        self.instructions.blocks = new_blocks;
    }

    fn reload_config(&mut self) {
        let Some(ref path) = self.lifecycle.config_path else {
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
        self.context_manager.soft_compaction_threshold = config.memory.soft_compaction_threshold;
        self.context_manager.hard_compaction_threshold = config.memory.hard_compaction_threshold;
        self.context_manager.compaction_preserve_tail = config.memory.compaction_preserve_tail;
        self.context_manager.compaction_cooldown_turns = config.memory.compaction_cooldown_turns;
        self.context_manager.prune_protect_tokens = config.memory.prune_protect_tokens;
        self.context_manager.compression = config.memory.compression.clone();
        self.context_manager.routing = config.memory.routing.clone();
        self.memory_state.cross_session_score_threshold =
            config.memory.cross_session_score_threshold;

        self.index.repo_map_tokens = config.index.repo_map_tokens;
        self.index.repo_map_ttl = std::time::Duration::from_secs(config.index.repo_map_ttl_secs);

        tracing::info!("config reloaded");
    }

    /// `/focus` slash command: display Focus Agent status.
    #[cfg(feature = "context-compression")]
    async fn handle_focus_status_command(&mut self) -> Result<(), error::AgentError> {
        use std::fmt::Write;
        let mut out = String::from("Focus Agent status\n\n");
        let _ = writeln!(out, "Enabled:          {}", self.focus.config.enabled);
        let _ = writeln!(out, "Active session:   {}", self.focus.is_active());
        if let Some(ref scope) = self.focus.active_scope {
            let _ = writeln!(out, "Active scope:     {scope}");
        }
        let _ = writeln!(
            out,
            "Knowledge blocks: {}",
            self.focus.knowledge_blocks.len()
        );
        let _ = writeln!(out, "Turns since focus: {}", self.focus.turns_since_focus);
        self.channel.send(&out).await?;
        Ok(())
    }

    /// `/sidequest` slash command: display `SideQuest` eviction stats.
    #[cfg(feature = "context-compression")]
    async fn handle_sidequest_status_command(&mut self) -> Result<(), error::AgentError> {
        use std::fmt::Write;
        let mut out = String::from("SideQuest status\n\n");
        let _ = writeln!(out, "Enabled:        {}", self.sidequest.config.enabled);
        let _ = writeln!(
            out,
            "Interval turns: {}",
            self.sidequest.config.interval_turns
        );
        let _ = writeln!(out, "Turn counter:   {}", self.sidequest.turn_counter);
        let _ = writeln!(out, "Passes run:     {}", self.sidequest.passes_run);
        let _ = writeln!(
            out,
            "Total evicted:  {} tool outputs",
            self.sidequest.total_evicted
        );
        self.channel.send(&out).await?;
        Ok(())
    }

    /// Run `SideQuest` tool output eviction pass (#1885).
    ///
    /// PERF-1 fix: two-phase non-blocking design.
    ///
    /// Phase 1 (apply, this turn): check for a background LLM result spawned last turn,
    /// validate and apply it immediately.
    ///
    /// Phase 2 (schedule, this turn): rebuild cursors and spawn a background `tokio::spawn`
    /// task for the LLM call. The result is stored in `pending_sidequest_result` and applied
    /// next turn, so the current agent turn is never blocked by the LLM call.
    #[cfg(feature = "context-compression")]
    #[allow(clippy::too_many_lines)]
    fn maybe_sidequest_eviction(&mut self) {
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        // S1 runtime guard: warn when SideQuest is enabled alongside a non-Reactive pruning
        // strategy — the two systems share the same pool of evictable tool outputs and can
        // interfere. Disable sidequest.enabled when pruning_strategy != Reactive.
        if self.sidequest.config.enabled {
            use crate::config::PruningStrategy;
            if !matches!(
                self.context_manager.compression.pruning_strategy,
                PruningStrategy::Reactive
            ) {
                tracing::warn!(
                    strategy = ?self.context_manager.compression.pruning_strategy,
                    "sidequest is enabled alongside a non-Reactive pruning strategy; \
                     consider disabling sidequest.enabled to avoid redundant eviction"
                );
            }
        }

        // Guard: do not evict while a focus session is active.
        if self.focus.is_active() {
            tracing::debug!("sidequest: skipping — focus session active");
            // Drop any pending result — cursors may be stale relative to focus truncation.
            self.compression.pending_sidequest_result = None;
            return;
        }

        // Phase 1: apply pending result from last turn's background LLM call.
        if let Some(handle) = self.compression.pending_sidequest_result.take() {
            // `now_or_never` avoids blocking — if the task isn't done yet, skip this turn.
            use futures::FutureExt as _;
            match handle.now_or_never() {
                Some(Ok(Some(evicted_indices))) if !evicted_indices.is_empty() => {
                    let cursors_snapshot = self.sidequest.tool_output_cursors.clone();
                    let freed = self.sidequest.apply_eviction(
                        &mut self.msg.messages,
                        &evicted_indices,
                        &self.metrics.token_counter,
                    );
                    if freed > 0 {
                        self.recompute_prompt_tokens();
                        // C1 fix: prevent maybe_compact() from firing in the same turn.
                        // cooldown=0: eviction does not impose post-compaction cooldown.
                        self.context_manager.compaction =
                            crate::agent::context_manager::CompactionState::CompactedThisTurn {
                                cooldown: 0,
                            };
                        tracing::info!(
                            freed_tokens = freed,
                            evicted_cursors = evicted_indices.len(),
                            pass = self.sidequest.passes_run,
                            "sidequest eviction complete"
                        );
                        if let Some(ref d) = self.debug_state.debug_dumper {
                            d.dump_sidequest_eviction(&cursors_snapshot, &evicted_indices, freed);
                        }
                        if let Some(ref tx) = self.session.status_tx {
                            let _ = tx.send(format!("SideQuest evicted {freed} tokens"));
                        }
                    } else {
                        // apply_eviction returned 0 — clear spinner so it doesn't dangle.
                        if let Some(ref tx) = self.session.status_tx {
                            let _ = tx.send(String::new());
                        }
                    }
                }
                Some(Ok(None | Some(_))) => {
                    tracing::debug!("sidequest: pending result: no cursors to evict");
                    if let Some(ref tx) = self.session.status_tx {
                        let _ = tx.send(String::new());
                    }
                }
                Some(Err(e)) => {
                    tracing::debug!("sidequest: background task panicked: {e}");
                    if let Some(ref tx) = self.session.status_tx {
                        let _ = tx.send(String::new());
                    }
                }
                None => {
                    // Task still running — re-store and wait another turn.
                    // We already took it; we'd need to re-spawn, but instead just drop and
                    // schedule fresh below to keep the cursor list current.
                    tracing::debug!(
                        "sidequest: background LLM task not yet complete, rescheduling"
                    );
                }
            }
        }

        // Phase 2: rebuild cursors and schedule the next background eviction LLM call.
        self.sidequest
            .rebuild_cursors(&self.msg.messages, &self.metrics.token_counter);

        if self.sidequest.tool_output_cursors.is_empty() {
            tracing::debug!("sidequest: no eligible cursors");
            return;
        }

        let prompt = self.sidequest.build_eviction_prompt();
        let max_eviction_ratio = self.sidequest.config.max_eviction_ratio;
        let n_cursors = self.sidequest.tool_output_cursors.len();
        // Clone the provider so the spawn closure owns it without borrowing self.
        let provider = self.summary_or_primary_provider().clone();

        // Spawn background task: the LLM call runs without blocking the agent loop.
        let handle = tokio::spawn(async move {
            let msgs = [Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];
            let response =
                match tokio::time::timeout(std::time::Duration::from_secs(5), provider.chat(&msgs))
                    .await
                {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        tracing::debug!("sidequest bg: LLM call failed: {e:#}");
                        return None;
                    }
                    Err(_) => {
                        tracing::debug!("sidequest bg: LLM call timed out");
                        return None;
                    }
                };

            let start = response.find('{')?;
            let end = response.rfind('}')?;
            if start > end {
                return None;
            }
            let json_slice = &response[start..=end];
            let parsed: sidequest::EvictionResponse = serde_json::from_str(json_slice).ok()?;
            let mut valid: Vec<usize> = parsed
                .del_cursors
                .into_iter()
                .filter(|&c| c < n_cursors)
                .collect();
            valid.sort_unstable();
            valid.dedup();
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let max_evict = ((n_cursors as f32) * max_eviction_ratio).ceil() as usize;
            valid.truncate(max_evict);
            Some(valid)
        });

        self.compression.pending_sidequest_result = Some(handle);
        tracing::debug!("sidequest: background LLM eviction task spawned");
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send("SideQuest: scoring tool outputs...".into());
        }
    }
}
pub(crate) async fn shutdown_signal(rx: &mut watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Convert a `FeedbackVerdict` (from `LlmClassifier`) into a `CorrectionSignal`.
///
/// Mirrors `JudgeVerdict::into_signal` to keep both code paths symmetric.
fn feedback_verdict_into_signal(
    verdict: &zeph_llm::classifier::llm::FeedbackVerdict,
    user_message: &str,
) -> Option<feedback_detector::CorrectionSignal> {
    if !verdict.is_correction {
        return None;
    }
    let confidence = verdict.confidence.clamp(0.0, 1.0);
    let kind_raw = verdict.kind.trim().to_lowercase().replace(' ', "_");
    let kind = match kind_raw.as_str() {
        "explicit_rejection" => feedback_detector::CorrectionKind::ExplicitRejection,
        "alternative_request" => feedback_detector::CorrectionKind::AlternativeRequest,
        "repetition" => feedback_detector::CorrectionKind::Repetition,
        "self_correction" => feedback_detector::CorrectionKind::SelfCorrection,
        other => {
            tracing::warn!(
                kind = other,
                "llm-classifier returned unknown correction kind, discarding"
            );
            return None;
        }
    };
    Some(feedback_detector::CorrectionSignal {
        confidence,
        kind,
        feedback_text: user_message.to_owned(),
    })
}

/// Store a correction record in memory (shared by judge and llm-classifier paths).
async fn store_correction_in_memory(
    memory: Option<std::sync::Arc<zeph_memory::semantic::SemanticMemory>>,
    conv_id: Option<zeph_memory::ConversationId>,
    assistant_snippet: &str,
    user_msg: &str,
    skill_name: String,
    kind_str: &str,
) {
    let Some(mem) = memory else { return };
    let correction_text = context::truncate_chars(user_msg, 500);
    match mem
        .sqlite()
        .store_user_correction(
            conv_id.map(|c| c.0),
            assistant_snippet,
            &correction_text,
            if skill_name.is_empty() {
                None
            } else {
                Some(skill_name.as_str())
            },
            kind_str,
        )
        .await
    {
        Ok(correction_id) => {
            if let Err(e) = mem
                .store_correction_embedding(correction_id, &correction_text)
                .await
            {
                tracing::warn!("failed to store correction embedding: {e:#}");
            }
        }
        Err(e) => {
            tracing::warn!("failed to store judge correction: {e:#}");
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
mod tests;

#[cfg(test)]
pub(crate) use tests::agent_tests;
