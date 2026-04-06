// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod accessors;
mod builder;
pub(crate) mod compaction_strategy;
pub(super) mod compression_feedback;
mod context;
pub(crate) mod context_manager;
mod corrections;
pub mod error;
mod experiment_cmd;
pub(super) mod feedback_detector;
pub(crate) mod focus;
mod graph_commands;
mod guidelines_commands;
mod index;
mod learning;
pub(crate) mod learning_engine;
mod log_commands;
mod lsp_commands;
mod mcp;
mod memory_commands;
mod message_queue;
mod model_commands;
mod persistence;
mod plan;
mod policy_commands;
mod provider_cmd;
pub(crate) mod rate_limiter;
#[cfg(feature = "scheduler")]
mod scheduler_commands;
mod scheduler_loop;
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
pub(crate) const LSP_NOTE_PREFIX: &str = "[lsp ";
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
    /// Current tool loop iteration index within the active user turn. Reset to 0 at turn start,
    /// incremented each iteration. Used to compute remaining tool call budget for `BudgetHint` (#2267).
    pub(super) current_tool_iteration: usize,
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
        let embedding_provider = provider.clone();
        Self::new_with_registry_arc(
            provider,
            embedding_provider,
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
        embedding_provider: AnyProvider,
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
        let (exp_notify_tx, exp_notify_rx) = tokio::sync::mpsc::channel::<String>(4);
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
                last_recall_confidence: None,
                digest_config: crate::config::DigestConfig::default(),
                cached_session_digest: None,
                context_strategy: crate::config::ContextStrategy::default(),
                crossover_turn_threshold: 20,
                rpe_router: None,
                goal_text: None,
                persona_config: crate::config::PersonaConfig::default(),
                trajectory_config: crate::config::TrajectoryConfig::default(),
                category_config: crate::config::CategoryConfig::default(),
                tree_config: crate::config::TreeConfig::default(),
                tree_consolidation_handle: None,
            },
            skill_state: SkillState {
                registry,
                skill_paths: Vec::new(),
                managed_dir: None,
                trust_config: crate::config::TrustConfig::default(),
                matcher,
                max_active_skills,
                disambiguation_threshold: 0.20,
                min_injection_score: 0.20,
                embedding_model: String::new(),
                skill_reload_rx: None,
                active_skill_names: Vec::new(),
                last_skills_prompt: skills_prompt,
                prompt_mode: SkillPromptMode::Auto,
                available_custom_secrets: HashMap::new(),
                cosine_weight: 0.7,
                hybrid_search: false,
                bm25_index: None,
                two_stage_matching: false,
                confusability_threshold: 0.0,
                rl_head: None,
                rl_weight: 0.3,
                rl_warmup_updates: 50,
                generation_output_dir: None,
                generation_provider_name: String::new(),
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
                adversarial_policy_info: None,
                spawn_depth: 0,
                budget_hint_enabled: true,
                channel_skills: zeph_config::ChannelSkillsConfig::default(),
            },
            mcp: McpState {
                tools: Vec::new(),
                registry: None,
                manager: None,
                allowed_commands: Vec::new(),
                max_dynamic: 10,
                elicitation_rx: None,
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
                elicitation_warn_sensitive_fields: true,
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
                lsp_hooks: None,
                policy_config: None,
                hooks_config: state::HooksConfigSnapshot::default(),
            },
            instructions: InstructionState {
                blocks: Vec::new(),
                reload_rx: None,
                reload_state: None,
            },
            security: SecurityState {
                sanitizer: ContentSanitizer::new(&zeph_sanitizer::ContentIsolationConfig::default()),
                quarantine_summarizer: None,
                is_acp_session: false,
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
                #[cfg(feature = "classifiers")]
                pii_ner_max_chars: 8192,
                #[cfg(feature = "classifiers")]
                pii_ner_circuit_breaker_threshold: 2,
                #[cfg(feature = "classifiers")]
                pii_ner_consecutive_timeouts: 0,
                #[cfg(feature = "classifiers")]
                pii_ner_tripped: false,
                memory_validator: zeph_sanitizer::memory_validation::MemoryWriteValidator::new(
                    zeph_sanitizer::memory_validation::MemoryWriteValidationConfig::default(),
                ),
                guardrail: None,
                response_verifier: zeph_sanitizer::response_verifier::ResponseVerifier::new(
                    zeph_config::ResponseVerificationConfig::default(),
                ),
                causal_analyzer: None,
            },
            experiments: ExperimentState {
                config: crate::config::ExperimentConfig::default(),
                cancel: None,
                baseline: crate::experiments::ConfigSnapshot::default(),
                eval_provider: None,
                notify_rx: Some(exp_notify_rx),
                notify_tx: exp_notify_tx,
            },
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
                last_known_cwd: std::env::current_dir().unwrap_or_default(),
                file_changed_rx: None,
                file_watcher: None,
            },
            providers: ProviderState {
                summary_provider: None,
                provider_override: None,
                judge_provider: None,
                probe_provider: None,
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
            current_tool_iteration: 0,
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

    /// Persist tombstone `ToolResult` messages for any assistant `ToolUse` parts that were written
    /// to the DB during this session but never paired with a `ToolResult` (e.g. because stdin
    /// closed while tool execution was in progress). Without this the next session startup strips
    /// those assistant messages and emits orphan warnings.
    async fn flush_orphaned_tool_use_on_shutdown(&mut self) {
        use zeph_llm::provider::{MessagePart, Role};

        // Walk messages in reverse: if the last assistant message (ignoring any trailing
        // system messages) has ToolUse parts and is NOT immediately followed by a user
        // message whose ToolResult ids cover those ToolUse ids, persist tombstones.
        let msgs = &self.msg.messages;
        // Find last assistant message index.
        let Some(asst_idx) = msgs.iter().rposition(|m| m.role == Role::Assistant) else {
            return;
        };
        let asst_msg = &msgs[asst_idx];
        let tool_use_ids: Vec<(&str, &str, &serde_json::Value)> = asst_msg
            .parts
            .iter()
            .filter_map(|p| {
                if let MessagePart::ToolUse { id, name, input } = p {
                    Some((id.as_str(), name.as_str(), input))
                } else {
                    None
                }
            })
            .collect();
        if tool_use_ids.is_empty() {
            return;
        }

        // Check whether a following user message already pairs all ToolUse ids.
        let paired_ids: std::collections::HashSet<&str> = msgs
            .get(asst_idx + 1..)
            .into_iter()
            .flatten()
            .filter(|m| m.role == Role::User)
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| {
                if let MessagePart::ToolResult { tool_use_id, .. } = p {
                    Some(tool_use_id.as_str())
                } else {
                    None
                }
            })
            .collect();

        let unpaired: Vec<zeph_llm::provider::ToolUseRequest> = tool_use_ids
            .iter()
            .filter(|(id, _, _)| !paired_ids.contains(*id))
            .map(|(id, name, input)| zeph_llm::provider::ToolUseRequest {
                id: (*id).to_owned(),
                name: (*name).to_owned(),
                input: (*input).clone(),
            })
            .collect();

        if unpaired.is_empty() {
            return;
        }

        tracing::info!(
            count = unpaired.len(),
            "shutdown: persisting tombstone ToolResults for unpaired in-flight tool calls"
        );
        self.persist_cancelled_tool_results(&unpaired).await;
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

        // Flush tombstone ToolResults for any assistant ToolUse that was persisted but never
        // paired with a ToolResult (e.g. stdin EOF mid-execution). Without this the next session
        // startup strips the orphaned ToolUse and emits warnings.
        self.flush_orphaned_tool_use_on_shutdown().await;

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
                    transcript_dir: mgr
                        .agent_transcript_dir(&id)
                        .map(|p| p.to_string_lossy().into_owned()),
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

            // Process any pending MCP elicitation requests from MCP servers.
            self.process_pending_elicitations().await;

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
                    Some(event) = recv_optional(&mut self.lifecycle.file_changed_rx) => {
                        self.handle_file_changed(event).await;
                        continue;
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

        if self.pre_process_security(trimmed).await? {
            return Ok(());
        }

        self.advance_context_lifecycle(&text, trimmed).await;

        let user_msg = self.build_user_message(&text, image_parts);

        // Extract URLs from user input and add to user_provided_urls for grounding checks.
        let urls = zeph_sanitizer::exfiltration::extract_flagged_urls(trimmed);
        if !urls.is_empty()
            && let Ok(mut set) = self.security.user_provided_urls.write()
        {
            set.extend(urls);
        }

        // Capture raw user input as goal text for A-MAC goal-conditioned write gating (#2483).
        // Derived from the raw input text before context assembly to avoid timing dependencies.
        self.memory_state.goal_text = Some(text.clone());

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

    // Returns true if the input was blocked and the caller should return Ok(()) immediately.
    async fn pre_process_security(&mut self, trimmed: &str) -> Result<bool, error::AgentError> {
        // Guardrail: LLM-based prompt injection pre-screening at the user input boundary.
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
                        return Ok(true);
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
                        return Ok(true);
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
                    return Ok(true);
                }
                zeph_sanitizer::InjectionVerdict::Suspicious => {
                    tracing::warn!("injection_classifier soft_signal on user input");
                }
                zeph_sanitizer::InjectionVerdict::Clean => {}
            }
        }
        #[cfg(feature = "classifiers")]
        self.push_classifier_metrics();

        Ok(false)
    }

    async fn advance_context_lifecycle(&mut self, text: &str, trimmed: &str) {
        // Reset per-message pruning cache at the start of each turn (#2298).
        self.mcp.pruning_cache.reset();

        // Extract before rebuild_system_prompt so the value is not tainted
        // by the secrets-bearing system prompt (ConversationId is just an i64).
        let conv_id = self.memory_state.conversation_id;
        self.rebuild_system_prompt(text).await;

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

        // MAR: propagate top-1 recall confidence to the router for cost-aware routing.
        self.provider
            .set_memory_confidence(self.memory_state.last_recall_confidence);

        self.learning_engine.reset_reflection();
    }

    fn build_user_message(
        &mut self,
        text: &str,
        image_parts: Vec<zeph_llm::provider::MessagePart>,
    ) -> Message {
        let mut all_image_parts = std::mem::take(&mut self.msg.pending_image_parts);
        all_image_parts.extend(image_parts);

        if !all_image_parts.is_empty() && self.provider.supports_vision() {
            let mut parts = vec![zeph_llm::provider::MessagePart::Text {
                text: text.to_owned(),
            }];
            parts.extend(all_image_parts);
            Message::from_parts(Role::User, parts)
        } else {
            if !all_image_parts.is_empty() {
                tracing::warn!(
                    count = all_image_parts.len(),
                    "image attachments dropped: provider does not support vision"
                );
            }
            Message {
                role: Role::User,
                content: text.to_owned(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            }
        }
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

    #[allow(clippy::too_many_lines)]
    async fn handle_agent_command(&mut self, cmd: crate::subagent::AgentCommand) -> Option<String> {
        use crate::subagent::AgentCommand;

        match cmd {
            AgentCommand::List => self.handle_agent_list(),
            AgentCommand::Background { name, prompt } => {
                let provider = self.provider.clone();
                let tool_executor = Arc::clone(&self.tool_executor);
                let skills = self.filtered_skills_for(&name);
                let cfg = self.orchestration.subagent_config.clone();
                let spawn_ctx = self.build_spawn_context(&cfg);
                let mgr = self.orchestration.subagent_manager.as_mut()?;
                match mgr.spawn(
                    &name,
                    &prompt,
                    provider,
                    tool_executor,
                    skills,
                    &cfg,
                    spawn_ctx,
                ) {
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
                let cfg = self.orchestration.subagent_config.clone();
                let spawn_ctx = self.build_spawn_context(&cfg);
                let mgr = self.orchestration.subagent_manager.as_mut()?;
                let task_id = match mgr.spawn(
                    &name,
                    &prompt,
                    provider,
                    tool_executor,
                    skills,
                    &cfg,
                    spawn_ctx,
                ) {
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

    /// Build a `SpawnContext` from current agent state for sub-agent spawning.
    fn build_spawn_context(
        &self,
        cfg: &zeph_config::SubAgentConfig,
    ) -> crate::subagent::SpawnContext {
        crate::subagent::SpawnContext {
            parent_messages: self.extract_parent_messages(cfg),
            parent_cancel: Some(self.lifecycle.cancel_token.clone()),
            parent_provider_name: {
                let name = &self.runtime.active_provider_name;
                if name.is_empty() {
                    None
                } else {
                    Some(name.clone())
                }
            },
            spawn_depth: self.runtime.spawn_depth,
            mcp_tool_names: self.extract_mcp_tool_names(),
        }
    }

    /// Extract recent parent messages for history propagation (Section 5.7 in spec).
    ///
    /// Filters system messages, takes last `context_window_turns * 2` messages,
    /// and applies a 25% context window cap using a 4-chars-per-token heuristic.
    fn extract_parent_messages(
        &self,
        config: &zeph_config::SubAgentConfig,
    ) -> Vec<zeph_llm::provider::Message> {
        use zeph_llm::provider::Role;
        if config.context_window_turns == 0 {
            return Vec::new();
        }
        let non_system: Vec<_> = self
            .msg
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .cloned()
            .collect();
        let take_count = config.context_window_turns * 2;
        let start = non_system.len().saturating_sub(take_count);
        let mut msgs = non_system[start..].to_vec();

        // Cap at 25% of model context window (rough 4-chars-per-token heuristic).
        let max_chars = 128_000usize / 4; // conservative default; 25% of 128K tokens
        let mut total_chars: usize = 0;
        let mut keep = msgs.len();
        for (i, m) in msgs.iter().enumerate() {
            total_chars += m.content.len();
            if total_chars > max_chars {
                keep = i;
                break;
            }
        }
        if keep < msgs.len() {
            tracing::info!(
                kept = keep,
                requested = config.context_window_turns * 2,
                "[subagent] truncated parent history from {} to {} turns due to token budget",
                config.context_window_turns * 2,
                keep
            );
            msgs.truncate(keep);
        }
        msgs
    }

    /// Extract MCP tool names from the tool executor for diagnostic annotation.
    fn extract_mcp_tool_names(&self) -> Vec<String> {
        self.tool_executor
            .tool_definitions_erased()
            .into_iter()
            .filter(|t| t.id.starts_with("mcp_"))
            .map(|t| t.id.to_string())
            .collect()
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
                zeph_memory::store::SourceKind::Hub
            } else {
                zeph_memory::store::SourceKind::Local
            };
            let initial_level = if matches!(source_kind, zeph_memory::store::SourceKind::Hub) {
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
        self.skill_state.min_injection_score = config.skills.min_injection_score;
        self.skill_state.cosine_weight = config.skills.cosine_weight.clamp(0.0, 1.0);
        self.skill_state.hybrid_search = config.skills.hybrid_search;
        self.skill_state.two_stage_matching = config.skills.two_stage_matching;
        self.skill_state.confusability_threshold =
            config.skills.confusability_threshold.clamp(0.0, 1.0);
        config
            .skills
            .generation_provider
            .as_str()
            .clone_into(&mut self.skill_state.generation_provider_name);
        self.skill_state.generation_output_dir =
            config.skills.generation_output_dir.as_deref().map(|p| {
                if let Some(stripped) = p.strip_prefix("~/") {
                    dirs::home_dir()
                        .map_or_else(|| std::path::PathBuf::from(p), |h| h.join(stripped))
                } else {
                    std::path::PathBuf::from(p)
                }
            });

        if config.memory.context_budget_tokens > 0 {
            self.context_manager.budget = Some(
                ContextBudget::new(config.memory.context_budget_tokens, 0.20)
                    .with_graph_enabled(config.memory.graph.enabled),
            );
        } else {
            self.context_manager.budget = None;
        }

        {
            let graph_cfg = &config.memory.graph;
            if graph_cfg.rpe.enabled {
                // Re-create router only if it doesn't exist yet; preserve state on hot-reload.
                if self.memory_state.rpe_router.is_none() {
                    self.memory_state.rpe_router =
                        Some(std::sync::Mutex::new(zeph_memory::RpeRouter::new(
                            graph_cfg.rpe.threshold,
                            graph_cfg.rpe.max_skip_turns,
                        )));
                }
            } else {
                self.memory_state.rpe_router = None;
            }
            self.memory_state.graph_config = graph_cfg.clone();
        }
        self.context_manager.soft_compaction_threshold = config.memory.soft_compaction_threshold;
        self.context_manager.hard_compaction_threshold = config.memory.hard_compaction_threshold;
        self.context_manager.compaction_preserve_tail = config.memory.compaction_preserve_tail;
        self.context_manager.compaction_cooldown_turns = config.memory.compaction_cooldown_turns;
        self.context_manager.prune_protect_tokens = config.memory.prune_protect_tokens;
        self.context_manager.compression = config.memory.compression.clone();
        self.context_manager.routing = config.memory.store_routing.clone();
        // Resolve routing_classifier_provider from the provider pool (#2484).
        self.context_manager.store_routing_provider = if config
            .memory
            .store_routing
            .routing_classifier_provider
            .is_empty()
        {
            None
        } else {
            let resolved = self.resolve_background_provider(
                &config.memory.store_routing.routing_classifier_provider,
            );
            Some(std::sync::Arc::new(resolved))
        };
        self.memory_state.cross_session_score_threshold =
            config.memory.cross_session_score_threshold;

        self.index.repo_map_tokens = config.index.repo_map_tokens;
        self.index.repo_map_ttl = std::time::Duration::from_secs(config.index.repo_map_ttl_secs);

        tracing::info!("config reloaded");
    }

    /// `/focus` slash command: display Focus Agent status.
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

    /// Check if the process cwd has changed since last call and fire `CwdChanged` hooks.
    ///
    /// Called after each tool batch completes. The check is a single syscall and has
    /// negligible cost. Only fires when cwd actually changed (defense-in-depth: normally
    /// only `set_working_directory` changes cwd; shell child processes cannot affect it).
    pub(crate) async fn check_cwd_changed(&mut self) {
        let current = match std::env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("check_cwd_changed: failed to get cwd: {e}");
                return;
            }
        };
        if current == self.lifecycle.last_known_cwd {
            return;
        }
        let old_cwd = std::mem::replace(&mut self.lifecycle.last_known_cwd, current.clone());
        self.session.env_context.working_dir = current.display().to_string();

        tracing::info!(
            old = %old_cwd.display(),
            new = %current.display(),
            "working directory changed"
        );

        let _ = self
            .channel
            .send_status("Working directory changed\u{2026}")
            .await;

        let hooks = self.session.hooks_config.cwd_changed.clone();
        if !hooks.is_empty() {
            let mut env = std::collections::HashMap::new();
            env.insert("ZEPH_OLD_CWD".to_owned(), old_cwd.display().to_string());
            env.insert("ZEPH_NEW_CWD".to_owned(), current.display().to_string());
            if let Err(e) = zeph_subagent::hooks::fire_hooks(&hooks, &env).await {
                tracing::warn!(error = %e, "CwdChanged hook failed");
            }
        }

        let _ = self.channel.send_status("").await;
    }

    /// Handle a `FileChangedEvent` from the file watcher.
    pub(crate) async fn handle_file_changed(
        &mut self,
        event: crate::file_watcher::FileChangedEvent,
    ) {
        tracing::info!(path = %event.path.display(), "file changed");

        let _ = self
            .channel
            .send_status("Running file-change hook\u{2026}")
            .await;

        let hooks = self.session.hooks_config.file_changed_hooks.clone();
        if !hooks.is_empty() {
            let mut env = std::collections::HashMap::new();
            env.insert(
                "ZEPH_CHANGED_PATH".to_owned(),
                event.path.display().to_string(),
            );
            if let Err(e) = zeph_subagent::hooks::fire_hooks(&hooks, &env).await {
                tracing::warn!(error = %e, "FileChanged hook failed");
            }
        }

        let _ = self.channel.send_status("").await;
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
mod tests;

#[cfg(test)]
pub(crate) use tests::agent_tests;
