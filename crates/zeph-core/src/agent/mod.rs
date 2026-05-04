// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// TODO(arch-revised-2026-04-26): agent/ module split deferred to epic/m49+/agent-split.
// Hard prerequisite: 100% task_supervisor adoption (currently 10/31 raw tokio::spawn
// sites in this directory). See arch-assessment-revised-2026-04-26T02-04-23.md PR 1
// and PR 8. Do not split this file until PR 1 is merged and a /specs/ entry exists.

mod acp_commands;
mod agent_access_impl;
pub(crate) mod agent_supervisor;
mod autodream;
mod builder;
pub(crate) mod channel_impl;
mod command_context_impls;
pub(super) mod compression_feedback;
mod context;
mod context_impls;
pub(crate) mod context_manager;
mod corrections;
pub mod error;
mod experiment_cmd;
pub(crate) mod focus;
mod index;
mod learning;
pub(crate) mod learning_engine;
mod log_commands;
mod loop_event;
mod lsp_commands;
mod magic_docs;
mod mcp;
pub(crate) mod memcot;
mod message_queue;
mod microcompact;
mod model_commands;
mod persistence;
#[cfg(feature = "scheduler")]
mod plan;
mod policy_commands;
mod provider_cmd;
#[cfg(feature = "self-check")]
mod quality_hook;
pub(crate) mod rate_limiter;
#[cfg(feature = "scheduler")]
mod scheduler_commands;
#[cfg(feature = "scheduler")]
mod scheduler_loop;
mod scope_commands;
pub mod session_config;
mod session_digest;
pub(crate) mod sidequest;
mod skill_management;
pub mod slash_commands;
pub mod speculative;
pub(crate) mod state;
pub(crate) mod task_injection;
pub(crate) mod tool_execution;
pub(crate) mod tool_orchestrator;
pub mod trajectory;
mod trajectory_commands;
mod trust_commands;
pub mod turn;
mod utils;
pub(crate) mod vigil;

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Arc;

use parking_lot::RwLock;

use tokio::sync::{mpsc, watch};
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
use crate::context::{ContextBudget, build_system_prompt};
use zeph_common::text::estimate_tokens;

use loop_event::LoopEvent;
use message_queue::{MAX_AUDIO_BYTES, MAX_IMAGE_BYTES, detect_image_mime};
use state::MessageState;

pub(crate) const DOOM_LOOP_WINDOW: usize = 3;
// CODE_CONTEXT_PREFIX is re-exported from zeph-agent-context::helpers so callers inside
// zeph-core that build system-prompt injections can use it without depending on zeph-agent-context
// directly. SESSION_DIGEST_PREFIX was removed when assembly migrated to ContextService.
pub(crate) use zeph_agent_context::helpers::CODE_CONTEXT_PREFIX;
pub(crate) const SCHEDULED_TASK_PREFIX: &str = "Execute the following scheduled task now: ";
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

/// Zeph agent: autonomous AI system with multi-model inference, semantic memory, skills,
/// tool orchestration, and multi-channel I/O.
///
/// The agent maintains conversation history, manages LLM provider state, coordinates tool
/// execution, and orchestrates memory and skill subsystems. It communicates with the outside
/// world via the [`Channel`] trait, enabling support for CLI, Telegram, TUI, or custom I/O.
///
/// # Architecture
///
/// - **Message state**: Conversation history with system prompt, message queue, and metadata
/// - **Memory state**: `SQLite` + Qdrant vector store for semantic search and compaction
/// - **Skill state**: Registry, matching engine, and self-learning evolution
/// - **Context manager**: Token budgeting, context assembly, and summarization
/// - **Tool orchestrator**: DAG-based multi-tool execution with streaming output
/// - **MCP client**: Multi-server support for Model Context Protocol
/// - **Index state**: AST-based code indexing and semantic retrieval
/// - **Security**: Sanitization, exfiltration detection, adversarial probes
/// - **Metrics**: Token usage, latency, cost, and anomaly tracking
///
/// # Channel Contract
///
/// The agent requires a [`Channel`] implementation for user interaction:
/// - Sends agent responses via `channel.send(message)`
/// - Receives user input via `channel.recv()` / `channel.recv_internal()`
/// - Supports structured events: tool invocations, tool output, streaming updates
///
/// # Lifecycle
///
/// 1. Create with [`Self::new`] or [`Self::new_with_registry_arc`]
/// 2. Run main loop with [`Self::run`]
/// 3. Clean up with [`Self::shutdown`] to persist state and close resources
///
pub struct Agent<C: Channel> {
    // --- I/O & primary providers (kept inline) ---
    provider: AnyProvider,
    /// Dedicated embedding provider. Resolved once at bootstrap from `[[llm.providers]]`
    /// (the entry with `embed = true`, or first entry with `embedding_model` set).
    /// Falls back to `provider.clone()` when no dedicated entry exists.
    /// **Never replaced** by `/provider switch`.
    embedding_provider: AnyProvider,
    channel: C,
    pub(crate) tool_executor: Arc<dyn ErasedToolExecutor>,

    // --- Conversation core (kept inline) ---
    pub(super) msg: MessageState,
    pub(super) context_manager: context_manager::ContextManager,
    pub(super) tool_orchestrator: tool_orchestrator::ToolOrchestrator,

    // --- Aggregated background services ---
    pub(super) services: state::Services,

    // --- Aggregated runtime / lifecycle / telemetry ---
    pub(super) runtime: state::AgentRuntime,
}

/// Control flow signal returned by [`Agent::apply_dispatch_result`].
enum DispatchFlow {
    /// The command requested exit; the agent loop should `break`.
    Break,
    /// The command was handled; the agent loop should `continue`.
    Continue,
    /// The command was not recognised; the agent loop should fall through.
    Fallthrough,
}

impl<C: Channel> Agent<C> {
    /// Create a new agent instance with the given LLM provider, I/O channel, and subsystems.
    ///
    /// # Arguments
    ///
    /// * `provider` — Multi-model LLM provider (Claude, `OpenAI`, Ollama, Candle)
    /// * `channel` — I/O abstraction for user interaction (CLI, Telegram, TUI, etc.)
    /// * `registry` — Skill registry; moved into an internal `Arc<RwLock<_>>` for sharing
    /// * `matcher` — Optional semantic skill matcher (e.g., Qdrant, BM25). If `None`,
    ///   skills are matched by exact name only
    /// * `max_active_skills` — Max concurrent skills in execution (must be > 0)
    /// * `tool_executor` — Trait object for executing shell, web, and custom tools
    ///
    /// # Initialization
    ///
    /// The constructor:
    /// 1. Wraps the skill registry into `Arc<RwLock<_>>` internally
    /// 2. Builds the system prompt from registered skills
    /// 3. Initializes all subsystems (memory, context manager, metrics, security)
    /// 4. Returns a ready-to-run agent
    ///
    /// # Panics
    ///
    /// Panics if `max_active_skills` is 0.
    #[must_use]
    pub fn new(
        provider: AnyProvider,
        channel: C,
        registry: SkillRegistry,
        matcher: Option<SkillMatcherBackend>,
        max_active_skills: usize,
        tool_executor: impl ToolExecutor + 'static,
    ) -> Self {
        let registry = Arc::new(RwLock::new(registry));
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
    pub fn new_with_registry_arc(
        provider: AnyProvider,
        embedding_provider: AnyProvider,
        channel: C,
        registry: Arc<RwLock<SkillRegistry>>,
        matcher: Option<SkillMatcherBackend>,
        max_active_skills: usize,
        tool_executor: impl ToolExecutor + 'static,
    ) -> Self {
        use state::{
            AgentRuntime, CompressionState, DebugState, ExperimentState, FeedbackState, IndexState,
            InstructionState, LifecycleState, McpState, MemoryState, MetricsState,
            OrchestrationState, ProviderState, RuntimeConfig, SecurityState, Services,
            SessionState, SkillState, ToolState,
        };

        debug_assert!(max_active_skills > 0, "max_active_skills must be > 0");
        let all_skills: Vec<Skill> = {
            let reg = registry.read();
            reg.all_meta()
                .iter()
                .filter_map(|m| reg.skill(&m.name).ok())
                .collect()
        };
        let empty_trust = HashMap::new();
        let empty_health: HashMap<String, (f64, u32)> = HashMap::new();
        let skills_prompt = format_skills_prompt(&all_skills, &empty_trust, &empty_health);
        let system_prompt = build_system_prompt(&skills_prompt, None);
        tracing::debug!(len = system_prompt.len(), "initial system prompt built");
        tracing::trace!(prompt = %system_prompt, "full system prompt");

        let initial_prompt_tokens = estimate_tokens(&system_prompt) as u64;
        let token_counter = Arc::new(TokenCounter::new());

        let services = Services {
            memory: MemoryState::default(),
            skill: SkillState::new(registry, matcher, max_active_skills, skills_prompt),
            learning_engine: learning_engine::LearningEngine::new(),
            feedback: FeedbackState::default(),
            mcp: McpState::default(),
            index: IndexState::default(),
            session: SessionState::new(),
            security: SecurityState::default(),
            experiments: ExperimentState::new(),
            compression: CompressionState::default(),
            orchestration: OrchestrationState::default(),
            focus: focus::FocusState::default(),
            sidequest: sidequest::SidequestState::default(),
            tool_state: ToolState::default(),
            goal_accounting: None,
            #[cfg(feature = "self-check")]
            quality: None,
            proactive_explorer: None,
            promotion_engine: None,
            taco_compressor: None,
        };

        let runtime = AgentRuntime {
            config: RuntimeConfig::default(),
            lifecycle: LifecycleState::new(),
            providers: ProviderState::new(initial_prompt_tokens),
            metrics: MetricsState::new(token_counter),
            debug: DebugState::default(),
            instructions: InstructionState::default(),
        };

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
                last_persisted_message_id: None,
                deferred_db_hide_ids: Vec::new(),
                deferred_db_summaries: Vec::new(),
            },
            context_manager: context_manager::ContextManager::new(),
            tool_orchestrator: tool_orchestrator::ToolOrchestrator::new(),
            services,
            runtime,
        }
    }

    /// Consume the agent and return the inner channel.
    ///
    /// Call this after [`run`][Agent::run] completes to retrieve the I/O channel (e.g., to
    /// read captured responses from a headless channel such as `BenchmarkChannel`).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use zeph_core::agent::Agent;
    /// // After agent.run().await completes, consume the agent to retrieve the channel.
    /// // let channel: MyChannel = agent.into_channel();
    /// ```
    #[must_use]
    pub fn into_channel(self) -> C {
        self.channel
    }

    /// Poll all active sub-agents for completed/failed/canceled results.
    ///
    /// Non-blocking: returns immediately with a list of `(task_id, result)` pairs
    /// for agents that have finished. Each completed agent is removed from the manager.
    pub async fn poll_subagents(&mut self) -> Vec<(String, String)> {
        let Some(mgr) = &mut self.services.orchestration.subagent_manager else {
            return vec![];
        };

        let finished: Vec<String> = mgr
            .statuses()
            .into_iter()
            .filter_map(|(id, status)| {
                if matches!(
                    status.state,
                    zeph_subagent::SubAgentState::Completed
                        | zeph_subagent::SubAgentState::Failed
                        | zeph_subagent::SubAgentState::Canceled
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
        let timeout_dur = std::time::Duration::from_secs(
            self.services
                .memory
                .compaction
                .shutdown_summary_timeout_secs,
        );
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
                    self.services
                        .memory
                        .compaction
                        .shutdown_summary_timeout_secs
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
                name: (*name).to_owned().into(),
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
        if !self.services.memory.compaction.shutdown_summary {
            return;
        }
        let Some(memory) = self.services.memory.persistence.memory.clone() else {
            return;
        };
        let Some(conversation_id) = self.services.memory.persistence.conversation_id else {
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
        if user_count
            < self
                .services
                .memory
                .compaction
                .shutdown_summary_min_messages
        {
            tracing::debug!(
                user_count,
                min = self
                    .services
                    .memory
                    .compaction
                    .shutdown_summary_min_messages,
                "shutdown summary: too few user messages, skipping"
            );
            return;
        }

        // TUI status — send errors silently ignored (TUI may already be gone at shutdown).
        let _ = self.channel.send_status("Saving session summary...").await;

        // Collect last N messages (skip system prompt at index 0).
        let max = self
            .services
            .memory
            .compaction
            .shutdown_summary_max_messages;
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

    /// Gracefully shut down the agent and persist state.
    ///
    /// Performs the following cleanup:
    ///
    /// 1. **Message persistence** — Deferred database writes (hide/summary operations)
    ///    are flushed to memory or disk
    /// 2. **Provider state** — LLM router state (e.g., Thompson sampling counters) is saved
    ///    to the vault
    /// 3. **Sub-agents** — All active sub-agent tasks are terminated
    /// 4. **MCP servers** — All connected Model Context Protocol servers are shut down
    /// 5. **Metrics finalization** — Compaction metrics and session metrics are recorded
    /// 6. **Memory finalization** — Vector stores and semantic indices are flushed
    /// 7. **Skill state** — Self-learning engine saves evolved skill definitions
    ///
    /// Call this before dropping the agent to ensure no data loss.
    pub async fn shutdown(&mut self) {
        let _ = self.channel.send_status("Shutting down...").await;

        // CRIT-1: persist Thompson state accumulated during this session.
        self.provider.save_router_state().await;

        // Persist AdaptOrch Beta-arm table alongside Thompson state.
        if let Some(ref advisor) = self.services.orchestration.topology_advisor
            && let Err(e) = advisor.save()
        {
            tracing::warn!(error = %e, "adaptorch: failed to persist state");
        }

        if let Some(ref mut mgr) = self.services.orchestration.subagent_manager {
            mgr.shutdown_all();
        }

        if let Some(ref manager) = self.services.mcp.manager {
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

        if let Some(ref tx) = self.runtime.metrics.metrics_tx {
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

        // NOTE: forcibly aborts in-flight Enrichment and Telemetry tasks tracked by the
        // supervisor. Before the sprint-1 refactor, experiment sessions were detached
        // tokio::spawn calls that survived shutdown; they are now intentionally untracked
        // (see experiment_cmd.rs) and will continue running until their own CancellationToken
        // is triggered or the process exits.
        self.runtime.lifecycle.supervisor.abort_all();

        // Abort background task handles not tracked by BackgroundSupervisor.
        // Per the Await Discipline rule, fire-and-forget handles must be aborted on shutdown.
        if let Some(h) = self.services.compression.pending_task_goal.take() {
            h.abort();
        }
        if let Some(h) = self.services.compression.pending_sidequest_result.take() {
            h.abort();
        }
        if let Some(h) = self.services.compression.pending_subgoal.take() {
            h.abort();
        }

        // Abort learning tasks (JoinSet detached at turn boundaries but not on shutdown).
        self.services.learning_engine.learning_tasks.abort_all();

        // Allow cancelled tasks to release their HTTP connections before the summary LLM call.
        // abort_all() posts cancellation signals but does not drain tasks; aborted futures only
        // observe cancellation at their next .await point. Without yielding here the summary
        // call races in-flight enrichment HTTP connections for the same API rate-limit budget.
        for _ in 0..4 {
            tokio::task::yield_now().await;
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
        let Some(ref mgr) = self.services.orchestration.subagent_manager else {
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
                        use zeph_subagent::def::PermissionMode;
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
    #[allow(clippy::too_many_lines)] // run loop is inherently large; each branch is independent
    pub async fn run(&mut self) -> Result<(), error::AgentError>
    where
        C: 'static,
    {
        if let Some(mut rx) = self.runtime.lifecycle.warmup_ready.take()
            && !*rx.borrow()
        {
            let _ = rx.changed().await;
            if !*rx.borrow() {
                tracing::warn!("model warmup did not complete successfully");
            }
        }

        // Restore the last-used provider preference before any user interaction (#3308).
        self.restore_channel_provider().await;

        // Load the session digest once at session start for context injection.
        self.load_and_cache_session_digest().await;
        self.maybe_send_resume_recap().await;

        loop {
            self.apply_provider_override();
            self.check_tool_refresh().await;
            self.process_pending_elicitations().await;
            self.refresh_subagent_metrics();
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
                match self.next_event().await? {
                    None | Some(LoopEvent::Shutdown) => break,
                    Some(LoopEvent::SkillReload) => {
                        self.reload_skills().await;
                        continue;
                    }
                    Some(LoopEvent::InstructionReload) => {
                        self.reload_instructions();
                        continue;
                    }
                    Some(LoopEvent::ConfigReload) => {
                        self.reload_config();
                        continue;
                    }
                    Some(LoopEvent::UpdateNotification(msg)) => {
                        if let Err(e) = self.channel.send(&msg).await {
                            tracing::warn!("failed to send update notification: {e}");
                        }
                        continue;
                    }
                    Some(LoopEvent::ExperimentCompleted(msg)) => {
                        self.services.experiments.cancel = None;
                        if let Err(e) = self.channel.send(&msg).await {
                            tracing::warn!("failed to send experiment completion: {e}");
                        }
                        continue;
                    }
                    Some(LoopEvent::ScheduledTask(prompt)) => {
                        let text = format!("{SCHEDULED_TASK_PREFIX}{prompt}");
                        let msg = crate::channel::ChannelMessage {
                            text,
                            attachments: Vec::new(),
                        };
                        self.drain_channel();
                        self.resolve_message(msg).await
                    }
                    Some(LoopEvent::TaskInjected(injection)) => {
                        if let Some(ref mut ls) = self.runtime.lifecycle.user_loop {
                            ls.iteration += 1;
                            tracing::info!(iteration = ls.iteration, "loop: tick");
                        }
                        let msg = crate::channel::ChannelMessage {
                            text: injection.prompt,
                            attachments: Vec::new(),
                        };
                        self.drain_channel();
                        self.resolve_message(msg).await
                    }
                    Some(LoopEvent::FileChanged(event)) => {
                        self.handle_file_changed(event).await;
                        continue;
                    }
                    Some(LoopEvent::Message(msg)) => {
                        self.drain_channel();
                        self.resolve_message(msg).await
                    }
                }
            };

            let trimmed = text.trim();

            // M3: extract flagged URLs from all slash commands before any registry dispatch,
            // so `/skill install <url>` and similar commands populate user_provided_urls.
            if trimmed.starts_with('/') {
                let slash_urls = zeph_sanitizer::exfiltration::extract_flagged_urls(trimmed);
                if !slash_urls.is_empty() {
                    self.services
                        .security
                        .user_provided_urls
                        .write()
                        .extend(slash_urls);
                }
            }

            // Registry dispatch: two-phase command dispatch.
            //
            // Phase 1 (session/debug): handlers that need sink + debug + messages but NOT agent.
            // Phase 2 (agent): handlers that need &mut Agent directly; use null sentinels for
            // the other CommandContext fields to satisfy the type but avoid borrow conflicts.
            //
            // STRUCTURAL NOTE (C4 — borrow-checker constraint, not deferred by oversight):
            // A `TurnState<'a, C>` struct grouping disjoint `&mut Agent<C>` sub-fields would
            // eliminate the LIFO-sentinel ordering below. The obstacle: `AgentAccess` is
            // implemented on `Agent<C>` itself (see `agent_access_impl.rs`), which accesses
            // fields like `memory_state`, `providers`, `mcp`, and `skill_state`. Those fields
            // overlap with what a `TurnState` would need to borrow, so `AgentBackend::Real`
            // cannot simultaneously hold `&mut Agent` while `TurnState` holds `&mut Agent.providers`.
            // The fix requires splitting `Agent<C>` fields into two disjoint sub-structs and moving
            // `AgentAccess` to the sub-struct that is disjoint from `TurnState`'s borrow set.
            // That restructuring touches `agent_access_impl.rs`, `state.rs`, `builder.rs`, all
            // command handlers, and the binary crate — estimated > 300 lines across > 5 files.
            // Track as a multi-PR refactor; the current sentinel pattern is correct and safe.
            //
            // Drop-order rules enforced here:
            //   - `sink_adapter` / `null_agent` declared before the registry block → dropped after.
            //   - Phase-2 sentinels declared before `ctx` → dropped after `ctx`.
            let session_impl = command_context_impls::SessionAccessImpl {
                supports_exit: self.channel.supports_exit(),
            };
            let mut messages_impl = command_context_impls::MessageAccessImpl {
                msg: &mut self.msg,
                tool_state: &mut self.services.tool_state,
                providers: &mut self.runtime.providers,
                metrics: &self.runtime.metrics,
                security: &mut self.services.security,
                tool_orchestrator: &mut self.tool_orchestrator,
            };
            // sink_adapter declared before reg so it is dropped after reg (LIFO).
            let mut sink_adapter = crate::channel::ChannelSinkAdapter(&mut self.channel);
            // null_agent must be declared before reg so it lives longer (LIFO drop order).
            let mut null_agent = zeph_commands::NullAgent;
            let registry_handled = {
                use zeph_commands::CommandRegistry;
                use zeph_commands::handlers::debug::{
                    DebugDumpCommand, DumpFormatCommand, LogCommand,
                };
                use zeph_commands::handlers::help::HelpCommand;
                use zeph_commands::handlers::session::{
                    ClearCommand, ClearQueueCommand, ExitCommand, QuitCommand, ResetCommand,
                };

                let mut reg = CommandRegistry::new();
                reg.register(ExitCommand);
                reg.register(QuitCommand);
                reg.register(ClearCommand);
                reg.register(ResetCommand);
                reg.register(ClearQueueCommand);
                reg.register(LogCommand);
                reg.register(DebugDumpCommand);
                reg.register(DumpFormatCommand);
                reg.register(HelpCommand);
                #[cfg(test)]
                reg.register(test_stubs::TestErrorCommand);

                let mut ctx = zeph_commands::CommandContext {
                    sink: &mut sink_adapter,
                    debug: &mut self.runtime.debug,
                    messages: &mut messages_impl,
                    session: &session_impl,
                    agent: &mut null_agent,
                };
                reg.dispatch(&mut ctx, trimmed).await
            };
            let session_reg_missed = registry_handled.is_none();
            match self
                .apply_dispatch_result(registry_handled, trimmed, false)
                .await
            {
                DispatchFlow::Break => break,
                DispatchFlow::Continue => continue,
                DispatchFlow::Fallthrough => {
                    // Not handled by the session/debug registry; try agent-command registry.
                }
            }

            // Agent-command registry: handlers access Agent<C> directly.
            // Null sentinels declared here so they outlive ctx regardless of whether the `if`
            // block is entered. `ctx` borrows both `self` and the sentinels; it must drop before
            // any subsequent `self.channel.*` calls. Because Rust drops in LIFO order, the
            // sentinels here will outlive ctx (ctx is declared later, inside the block).
            let mut agent_null_debug = command_context_impls::NullDebugAccess;
            let mut agent_null_messages = command_context_impls::NullMessageAccess;
            let agent_null_session = command_context_impls::NullSessionAccess;
            let mut agent_null_sink = zeph_commands::NullSink;
            let agent_result: Option<
                Result<zeph_commands::CommandOutput, zeph_commands::CommandError>,
            > = if session_reg_missed {
                use zeph_commands::CommandRegistry;
                use zeph_commands::handlers::{
                    acp::AcpCommand,
                    agent_cmd::AgentCommand,
                    compaction::{CompactCommand, NewConversationCommand, RecapCommand},
                    experiment::ExperimentCommand,
                    goal::GoalCommand,
                    loop_cmd::LoopCommand,
                    lsp::LspCommand,
                    mcp::McpCommand,
                    memory::{GraphCommand, GuidelinesCommand, MemoryCommand},
                    misc::{CacheStatsCommand, ImageCommand, NotifyTestCommand},
                    model::{ModelCommand, ProviderCommand},
                    plan::PlanCommand,
                    plugins::PluginsCommand,
                    policy::PolicyCommand,
                    scheduler::SchedulerCommand,
                    skill::{FeedbackCommand, SkillCommand, SkillsCommand},
                    status::{FocusCommand, GuardrailCommand, SideQuestCommand, StatusCommand},
                    trajectory::{ScopeCommand, TrajectoryCommand},
                };

                let mut agent_reg = CommandRegistry::new();
                agent_reg.register(MemoryCommand);
                agent_reg.register(GraphCommand);
                agent_reg.register(GuidelinesCommand);
                agent_reg.register(ModelCommand);
                agent_reg.register(ProviderCommand);
                // Phase 6 migrations: /skill, /skills, /feedback use clone-before-await pattern.
                agent_reg.register(SkillCommand);
                agent_reg.register(SkillsCommand);
                agent_reg.register(FeedbackCommand);
                agent_reg.register(McpCommand);
                agent_reg.register(PolicyCommand);
                agent_reg.register(SchedulerCommand);
                agent_reg.register(LspCommand);
                // Phase 4 migrations (Send-safe commands):
                agent_reg.register(CacheStatsCommand);
                agent_reg.register(ImageCommand);
                agent_reg.register(NotifyTestCommand);
                agent_reg.register(StatusCommand);
                agent_reg.register(GuardrailCommand);
                agent_reg.register(FocusCommand);
                agent_reg.register(SideQuestCommand);
                agent_reg.register(AgentCommand);
                // Phase 5 migrations (Send-compatible):
                agent_reg.register(CompactCommand);
                agent_reg.register(NewConversationCommand);
                agent_reg.register(RecapCommand);
                agent_reg.register(ExperimentCommand);
                agent_reg.register(PlanCommand);
                agent_reg.register(LoopCommand);
                agent_reg.register(PluginsCommand);
                agent_reg.register(AcpCommand);
                agent_reg.register(TrajectoryCommand);
                agent_reg.register(ScopeCommand);
                agent_reg.register(GoalCommand);

                let mut ctx = zeph_commands::CommandContext {
                    sink: &mut agent_null_sink,
                    debug: &mut agent_null_debug,
                    messages: &mut agent_null_messages,
                    session: &agent_null_session,
                    agent: self,
                };
                // self is reborrowed; ctx drops at end of this block.
                agent_reg.dispatch(&mut ctx, trimmed).await
            } else {
                None
            };
            // self.channel is available again here (ctx borrow dropped above).
            // Post-dispatch learning hook for `/skill reject` / `/feedback` is triggered
            // inside apply_dispatch_result when with_learning = true.
            match self
                .apply_dispatch_result(agent_result, trimmed, true)
                .await
            {
                DispatchFlow::Break => break,
                DispatchFlow::Continue => continue,
                DispatchFlow::Fallthrough => {
                    // Not handled by agent registry; fall through to existing dispatch.
                }
            }

            match self.handle_builtin_command(trimmed) {
                Some(true) => break,
                Some(false) => continue,
                None => {}
            }

            self.process_user_message(text, image_parts).await?;
        }

        // autoDream: run background memory consolidation if conditions are met (#2697).
        // Runs with a timeout — partial state is acceptable for MVP.
        self.maybe_autodream().await;

        // Flush trace collector on normal exit (C-04: Drop handles error/panic paths).
        if let Some(ref mut tc) = self.runtime.debug.trace_collector {
            tc.finish();
        }

        Ok(())
    }

    /// Dispatch a slash-command registry result and flush the channel.
    ///
    /// Returns [`DispatchFlow::Break`] on exit, [`DispatchFlow::Continue`] when handled, or
    /// [`DispatchFlow::Fallthrough`] when `result` is `None`.
    /// When `with_learning` is `true`, triggers the post-command learning hook for `Message` output.
    async fn apply_dispatch_result(
        &mut self,
        result: Option<Result<zeph_commands::CommandOutput, zeph_commands::CommandError>>,
        command: &str,
        with_learning: bool,
    ) -> DispatchFlow {
        match result {
            Some(Ok(zeph_commands::CommandOutput::Exit)) => {
                let _ = self.channel.flush_chunks().await;
                DispatchFlow::Break
            }
            Some(Ok(
                zeph_commands::CommandOutput::Continue | zeph_commands::CommandOutput::Silent,
            )) => {
                let _ = self.channel.flush_chunks().await;
                DispatchFlow::Continue
            }
            Some(Ok(zeph_commands::CommandOutput::Message(msg))) => {
                let _ = self.channel.send(&msg).await;
                let _ = self.channel.flush_chunks().await;
                if with_learning {
                    self.maybe_trigger_post_command_learning(command).await;
                }
                DispatchFlow::Continue
            }
            Some(Err(e)) => {
                let _ = self.channel.send(&e.to_string()).await;
                let _ = self.channel.flush_chunks().await;
                tracing::warn!(command = %command, error = %e.0, "slash command failed");
                DispatchFlow::Continue
            }
            None => DispatchFlow::Fallthrough,
        }
    }

    /// Apply any pending LLM provider override from ACP `set_session_config_option`.
    fn apply_provider_override(&mut self) {
        if let Some(ref slot) = self.runtime.providers.provider_override
            && let Some(new_provider) = slot.write().take()
        {
            tracing::debug!(provider = new_provider.name(), "ACP model override applied");
            self.provider = new_provider;
        }
    }

    /// Poll all event sources and return the next [`LoopEvent`].
    ///
    /// Returns `None` when the inbound channel closes (graceful shutdown).
    ///
    /// # Errors
    ///
    /// Propagates channel receive errors.
    async fn next_event(&mut self) -> Result<Option<LoopEvent>, error::AgentError> {
        let event = tokio::select! {
            result = self.channel.recv() => {
                return Ok(result?.map(LoopEvent::Message));
            }
            () = shutdown_signal(&mut self.runtime.lifecycle.shutdown) => {
                tracing::info!("shutting down");
                LoopEvent::Shutdown
            }
            Some(_) = recv_optional(&mut self.services.skill.skill_reload_rx) => {
                LoopEvent::SkillReload
            }
            Some(_) = recv_optional(&mut self.runtime.instructions.reload_rx) => {
                LoopEvent::InstructionReload
            }
            Some(_) = recv_optional(&mut self.runtime.lifecycle.config_reload_rx) => {
                LoopEvent::ConfigReload
            }
            Some(msg) = recv_optional(&mut self.runtime.lifecycle.update_notify_rx) => {
                LoopEvent::UpdateNotification(msg)
            }
            Some(msg) = recv_optional(&mut self.services.experiments.notify_rx) => {
                LoopEvent::ExperimentCompleted(msg)
            }
            Some(prompt) = recv_optional(&mut self.runtime.lifecycle.custom_task_rx) => {
                tracing::info!("scheduler: injecting custom task as agent turn");
                LoopEvent::ScheduledTask(prompt)
            }
            () = async {
                if let Some(ref mut ls) = self.runtime.lifecycle.user_loop {
                    if ls.cancel_tx.is_cancelled() {
                        std::future::pending::<()>().await;
                    } else {
                        ls.interval.tick().await;
                    }
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                // Re-check user_loop after the tick — /loop stop may have fired between the
                // interval firing and this arm executing. Returning Ok(None) causes the caller
                // to `continue` without injecting a stale or empty prompt.
                let Some(ls) = self.runtime.lifecycle.user_loop.as_ref() else {
                    return Ok(None);
                };
                if ls.cancel_tx.is_cancelled() {
                    self.runtime.lifecycle.user_loop = None;
                    return Ok(None);
                }
                let prompt = ls.prompt.clone();
                LoopEvent::TaskInjected(task_injection::TaskInjection { prompt })
            }
            Some(event) = recv_optional(&mut self.runtime.lifecycle.file_changed_rx) => {
                LoopEvent::FileChanged(event)
            }
        };
        Ok(Some(event))
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
            has_stt = self.runtime.providers.stt.is_some(),
            "resolve_message attachments"
        );

        let text = if !audio_attachments.is_empty()
            && let Some(stt) = self.runtime.providers.stt.as_ref()
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

    /// Create a new [`Turn`] for the given input and advance the turn counter.
    ///
    /// Clears per-turn state that must not carry over between turns:
    /// - per-turn `CancellationToken` (new token for each turn)
    /// - per-turn URL set in `SecurityState` (cleared here; re-populated in
    ///   `process_user_message_inner` after security checks)
    fn begin_turn(&mut self, input: turn::TurnInput) -> turn::Turn {
        let id = turn::TurnId(self.runtime.debug.iteration_counter as u64);
        self.runtime.debug.iteration_counter += 1;
        let cancel_token = CancellationToken::new();
        // keep agent-wide token in sync with per-turn token — TODO(#3498): consolidate in Phase 2
        self.runtime.lifecycle.cancel_token = cancel_token.clone();
        self.services.security.user_provided_urls.write().clear();
        // Reset per-turn LLM request counter for the notification gate.
        self.runtime.lifecycle.turn_llm_requests = 0;

        // Spec 050 §2: drain pending risk signals from executor layers before advancing.
        {
            let pending: Vec<u8> = {
                let mut q = self.services.security.trajectory_signal_queue.lock();
                std::mem::take(&mut *q)
            };
            for code in pending {
                self.services
                    .security
                    .trajectory
                    .record(crate::agent::trajectory::RiskSignal::from_code(code));
            }
        }
        // Spec 050 Invariant 2: advance trajectory sentinel BEFORE any gate evaluation.
        // F5: write auto-recover audit entry when sentinel hard-resets.
        if self.services.security.trajectory.advance_turn()
            && let Some(logger) = self.tool_orchestrator.audit_logger.clone()
        {
            let entry = zeph_tools::AuditEntry {
                timestamp: zeph_tools::chrono_now(),
                tool: "<sentinel>".to_owned().into(),
                command: String::new(),
                result: zeph_tools::AuditResult::Success,
                duration_ms: 0,
                error_category: Some("trajectory_auto_recover".to_owned()),
                error_domain: Some("security".to_owned()),
                error_phase: None,
                claim_source: None,
                mcp_server_id: None,
                injection_flagged: false,
                embedding_anomalous: false,
                cross_boundary_mcp_to_acp: false,
                adversarial_policy_decision: None,
                exit_code: None,
                truncated: false,
                caller_id: None,
                policy_match: None,
                correlation_id: None,
                vigil_risk: None,
                execution_env: None,
                resolved_cwd: None,
                scope_at_definition: None,
                scope_at_dispatch: None,
            };
            self.runtime.lifecycle.supervisor.spawn(
                crate::agent::agent_supervisor::TaskClass::Telemetry,
                "trajectory-auto-recover-audit",
                async move { logger.log(&entry).await },
            );
        }
        // Publish updated risk level to the shared slot so PolicyGateExecutor can read it.
        let risk_level = self.services.security.trajectory.current_risk();
        *self.services.security.trajectory_risk_slot.write() = u8::from(risk_level);
        // TUI/CLI: emit a status indicator when risk reaches High or Critical (NFR-CG-006).
        if let Some(alert) = self.services.security.trajectory.poll_alert() {
            let msg = format!(
                "[trajectory] Risk level: {:?} (score={:.2})",
                alert.level, alert.score
            );
            tracing::warn!(
                level = ?alert.level,
                score = alert.score,
                "trajectory sentinel alert"
            );
            if let Some(ref tx) = self.services.session.status_tx {
                let _ = tx.send(msg);
            }
        }

        let context = turn::TurnContext::new(id, cancel_token, self.runtime.config.timeouts);
        turn::Turn::new(context, input)
    }

    /// Finalise a turn: copy accumulated timings into `MetricsState` and flush.
    ///
    /// Must be called exactly once per turn, after `process_user_message_inner` returns
    /// (regardless of success or error). Corresponds to the M2 resolution in the spec:
    /// `TurnMetrics.timings` is the single source of truth; `MetricsState.pending_timings`
    /// is populated from it here so the rest of the pipeline is unchanged.
    fn end_turn(&mut self, turn: turn::Turn) {
        self.runtime.metrics.pending_timings = turn.metrics.timings;
        self.flush_turn_timings();
        // Clear per-turn intent (FR-008): must not persist across turns.
        self.services.session.current_turn_intent = None;
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "agent.turn", skip_all, fields(turn_id))
    )]
    async fn process_user_message(
        &mut self,
        text: String,
        image_parts: Vec<zeph_llm::provider::MessagePart>,
    ) -> Result<(), error::AgentError> {
        let input = turn::TurnInput::new(text, image_parts);
        let mut t = self.begin_turn(input);

        let turn_idx = usize::try_from(t.id().0).unwrap_or(usize::MAX);
        tracing::Span::current().record("turn_id", t.id().0);
        // Record iteration start in trace collector (C-02: owned guard, no borrow held).
        self.runtime
            .debug
            .start_iteration_span(turn_idx, t.input.text.trim());

        let result = Box::pin(self.process_user_message_inner(&mut t)).await;

        // Close iteration span regardless of outcome (partial trace preserved on error).
        let span_status = if result.is_ok() {
            crate::debug_dump::trace::SpanStatus::Ok
        } else {
            crate::debug_dump::trace::SpanStatus::Error {
                message: "iteration failed".to_owned(),
            }
        };
        self.runtime.debug.end_iteration_span(turn_idx, span_status);

        self.end_turn(t);
        result
    }

    async fn process_user_message_inner(
        &mut self,
        turn: &mut turn::Turn,
    ) -> Result<(), error::AgentError> {
        self.reap_background_tasks_and_update_metrics();

        let tokens_before_turn = self
            .runtime
            .metrics
            .metrics_tx
            .as_ref()
            .map_or(0, |tx| tx.borrow().total_tokens);

        // Drain any background shell completions that arrived since the last turn.
        // They are buffered in `pending_background_completions` and merged with the
        // real user message into a single user-role block below (N1 invariant).
        self.drain_background_completions();

        self.wire_cancel_bridge(turn.cancel_token());

        // Clone text out of Turn so we can hold both `&str` borrows and mutate turn.metrics.
        let text = turn.input.text.clone();
        let trimmed_owned = text.trim().to_owned();
        let trimmed = trimmed_owned.as_str();

        // Capture current-turn intent for VIGIL gate (FR-007). Truncated to 1024 chars.
        // Must be set BEFORE any tool call; cleared at end_turn (FR-008).
        if self.services.security.vigil.is_some() {
            let intent_len = trimmed.floor_char_boundary(1024.min(trimmed.len()));
            self.services.session.current_turn_intent = Some(trimmed[..intent_len].to_owned());
        }

        if let Some(result) = self.dispatch_slash_command(trimmed).await {
            return result;
        }

        self.check_pending_rollbacks().await;

        if self.pre_process_security(trimmed).await? {
            return Ok(());
        }

        let t_ctx = std::time::Instant::now();
        tracing::debug!("turn timing: prepare_context start");
        self.advance_context_lifecycle_guarded(&text, trimmed).await;
        turn.metrics_mut().timings.prepare_context_ms =
            u64::try_from(t_ctx.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::debug!(
            ms = turn.metrics_snapshot().timings.prepare_context_ms,
            "turn timing: prepare_context done"
        );

        let image_parts = std::mem::take(&mut turn.input.image_parts);
        // Prepend any background completion blocks to the user text. All completions and the
        // user message MUST be merged into a single user-role block to satisfy the strict
        // user/assistant alternation rule (Anthropic Messages API — N1 invariant).
        let merged_text = self.build_user_message_text_with_bg_completions(&text);
        let user_msg = self.build_user_message(&merged_text, image_parts);

        // Extract URLs from user input and add to user_provided_urls for grounding checks.
        // URL set was cleared in begin_turn; re-populate for this turn.
        let urls = zeph_sanitizer::exfiltration::extract_flagged_urls(trimmed);
        if !urls.is_empty() {
            self.services
                .security
                .user_provided_urls
                .write()
                .extend(urls);
        }

        // Capture raw user input as goal text for A-MAC goal-conditioned write gating (#2483).
        // Derived from the raw input text before context assembly to avoid timing dependencies.
        self.services.memory.extraction.goal_text = Some(text.clone());

        let t_persist = std::time::Instant::now();
        tracing::debug!("turn timing: persist_message(user) start");
        // Image parts intentionally excluded — base64 payloads too large for message history.
        self.persist_message(Role::User, &text, &[], false).await;
        turn.metrics_mut().timings.persist_message_ms =
            u64::try_from(t_persist.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::debug!(
            ms = turn.metrics_snapshot().timings.persist_message_ms,
            "turn timing: persist_message(user) done"
        );
        self.push_message(user_msg);

        // llm_chat_ms and tool_exec_ms are accumulated inside call_chat_with_tools and
        // handle_native_tool_calls respectively via metrics.pending_timings.
        tracing::debug!("turn timing: process_response start");
        let turn_had_error = if let Err(e) = self.process_response().await {
            // Detach any in-flight learning tasks before mutating message state.
            self.services.learning_engine.learning_tasks.detach_all();
            tracing::error!("Response processing failed: {e:#}");

            // Record provider failure timestamp so the next turn can skip
            // expensive context preparation while providers are known-down.
            if e.is_no_providers() {
                self.runtime.lifecycle.last_no_providers_at = Some(std::time::Instant::now());
                let backoff_secs = self.runtime.config.timeouts.no_providers_backoff_secs;
                tracing::warn!(
                    backoff_secs,
                    "no providers available; backing off before next turn"
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            }

            let user_msg = format!("Error: {e:#}");
            self.channel.send(&user_msg).await?;
            self.msg.messages.pop();
            self.recompute_prompt_tokens();
            self.channel.flush_chunks().await?;
            true
        } else {
            // Detach learning tasks spawned this turn — they are fire-and-forget and must not
            // leak into the next turn's context.
            self.services.learning_engine.learning_tasks.detach_all();
            self.truncate_old_tool_results();
            // MagicDocs: spawn background doc updates if any are due (#2702).
            self.maybe_update_magic_docs();
            // Compression spectrum: fire-and-forget promotion scan (#3305).
            self.maybe_spawn_promotion_scan();
            false
        };
        tracing::debug!("turn timing: process_response done");

        // MARCH self-check hook: runs after every successful response, including cache-hit path.
        #[cfg(feature = "self-check")]
        if let Some(pipeline) = self.services.quality.clone() {
            self.run_self_check_for_turn(pipeline, turn.id().0).await;
        }
        // Flush pending response chunks and emit ResponseEnd exactly once per turn.
        // send() no longer emits ResponseEnd — flush_chunks() is the sole emitter.
        // When self-check appends a flag_marker chunk, this single call covers both
        // the main response and the marker, preventing the double response_end of #3243.
        let _ = self.channel.flush_chunks().await;

        self.maybe_fire_completion_notification(turn, turn_had_error);

        self.flush_goal_accounting(tokens_before_turn);

        // Collect llm_chat_ms and tool_exec_ms from MetricsState.pending_timings (accumulated
        // by the tool execution chain) into turn.metrics so end_turn can flush them.
        // This is the Phase 1 bridging: existing code writes to pending_timings directly;
        // we harvest those values into Turn before end_turn overwrites pending_timings.
        turn.metrics_mut().timings.llm_chat_ms = self.runtime.metrics.pending_timings.llm_chat_ms;
        turn.metrics_mut().timings.tool_exec_ms = self.runtime.metrics.pending_timings.tool_exec_ms;

        Ok(())
    }

    /// Wire the per-turn cancellation token into the cancel bridge.
    ///
    /// The bridge translates `cancel_signal` (Notify) into a `CancellationToken` cancel so that
    /// channel-level abort requests propagate to the in-flight LLM call. The previous bridge task
    /// is aborted before a new one is spawned to prevent unbounded accumulation (#2737).
    fn wire_cancel_bridge(&mut self, turn_token: &tokio_util::sync::CancellationToken) {
        let signal = Arc::clone(&self.runtime.lifecycle.cancel_signal);
        let token = turn_token.clone();
        // Keep lifecycle.cancel_token in sync so existing code that reads it still works.
        self.runtime.lifecycle.cancel_token = turn_token.clone();
        if let Some(prev) = self.runtime.lifecycle.cancel_bridge_handle.take() {
            prev.abort();
        }
        self.runtime.lifecycle.cancel_bridge_handle =
            Some(self.runtime.lifecycle.task_supervisor.spawn_oneshot(
                std::sync::Arc::from("agent.lifecycle.cancel_bridge"),
                move || async move {
                    signal.notified().await;
                    token.cancel();
                },
            ));
    }

    /// Reap completed background tasks, apply summarization signal, and update supervisor metrics.
    ///
    /// Called at the top of each turn, before any user message processing.
    fn reap_background_tasks_and_update_metrics(&mut self) {
        let bg_signal = self.runtime.lifecycle.supervisor.reap();
        if bg_signal.did_summarize {
            self.services.memory.persistence.unsummarized_count = 0;
            tracing::debug!("background summarization completed; unsummarized_count reset");
        }
        let snap = self.runtime.lifecycle.supervisor.metrics_snapshot();
        self.update_metrics(|m| {
            m.bg_inflight = snap.inflight as u64;
            m.bg_dropped = snap.total_dropped();
            m.bg_completed = snap.total_completed();
            m.bg_enrichment_inflight = snap.class_inflight[0] as u64;
            m.bg_telemetry_inflight = snap.class_inflight[1] as u64;
        });

        // Update shell background run rows for TUI panel.
        if self.runtime.lifecycle.shell_executor_handle.is_some() {
            let shell_rows: Vec<crate::metrics::ShellBackgroundRunRow> = self
                .runtime
                .lifecycle
                .shell_executor_handle
                .as_ref()
                .map(|e| e.background_runs_snapshot())
                .unwrap_or_default()
                .into_iter()
                .map(|s| crate::metrics::ShellBackgroundRunRow {
                    run_id: truncate_shell_run_id(&s.run_id),
                    command: truncate_shell_command(&s.command),
                    elapsed_secs: s.elapsed_ms / 1000,
                })
                .collect();
            self.update_metrics(|m| {
                m.shell_background_runs = shell_rows;
            });
        }

        // Intentional ordering: reap() runs before abort_class() so completed tasks are
        // accounted in the snapshot above.
        if self
            .runtime
            .config
            .supervisor_config
            .abort_enrichment_on_turn
        {
            self.runtime
                .lifecycle
                .supervisor
                .abort_class(agent_supervisor::TaskClass::Enrichment);
        }
    }

    /// Fire completion notifications and `turn_complete` hooks after each turn.
    ///
    /// Builds [`crate::notifications::TurnSummary`] once and reuses it for both the
    /// [`crate::notifications::Notifier`] and any `[[hooks.turn_complete]]` entries. The
    /// `preview` field is already redacted by [`Self::last_assistant_preview`], so hook
    /// env vars carry no raw assistant output.
    ///
    /// Gating:
    /// - When a `Notifier` is configured, both the notifier and hooks share its
    ///   `should_fire` gate (`min_turn_duration_ms`, `only_on_error`, `enabled`).
    /// - When no `Notifier` is configured, hooks fire on every turn completion (the
    ///   notifier path is simply skipped).
    fn maybe_fire_completion_notification(&mut self, turn: &turn::Turn, is_error: bool) {
        let snap = turn.metrics_snapshot().timings.clone();
        let duration_ms = snap
            .prepare_context_ms
            .saturating_add(snap.llm_chat_ms)
            .saturating_add(snap.tool_exec_ms);
        let summary = crate::notifications::TurnSummary {
            duration_ms,
            preview: self.last_assistant_preview(160),
            // TODO: wire turn_tool_calls counter once LifecycleState tracks it (Phase 2).
            tool_calls: 0,
            llm_requests: self.runtime.lifecycle.turn_llm_requests,
            exit_status: if is_error {
                crate::notifications::TurnExitStatus::Error
            } else {
                crate::notifications::TurnExitStatus::Success
            },
        };

        // Gate evaluation: notifier's should_fire result (or unconditional when absent).
        let gate_ok = self
            .runtime
            .lifecycle
            .notifier
            .as_ref()
            .is_none_or(|n| n.should_fire(&summary));

        // 1) Existing notifier path — unchanged semantics.
        if let Some(ref notifier) = self.runtime.lifecycle.notifier
            && gate_ok
        {
            notifier.fire(&summary);
        }

        // 2) turn_complete hooks — fire-and-forget, matching the Notifier::fire pattern.
        // MCP dispatch is omitted: turn_complete hooks are expected to be Command-type
        // (shell scripts for desktop notifications, status-bar updates, etc.). McpTool
        // hooks in this context would require a Send + 'static MCP handle; defer that
        // extension if needed via a follow-up.
        let hooks = self.services.session.hooks_config.turn_complete.clone();
        if !hooks.is_empty() && gate_ok {
            let mut env = std::collections::HashMap::new();
            env.insert(
                "ZEPH_TURN_DURATION_MS".to_owned(),
                summary.duration_ms.to_string(),
            );
            env.insert(
                "ZEPH_TURN_STATUS".to_owned(),
                if is_error { "error" } else { "success" }.to_owned(),
            );
            env.insert("ZEPH_TURN_PREVIEW".to_owned(), summary.preview.clone());
            env.insert(
                "ZEPH_TURN_LLM_REQUESTS".to_owned(),
                summary.llm_requests.to_string(),
            );
            let _span = tracing::info_span!("core.agent.turn_hooks").entered();
            let _accepted = self.runtime.lifecycle.supervisor.spawn(
                agent_supervisor::TaskClass::Telemetry,
                "turn-complete-hooks",
                async move {
                    // Explicitly 'static so the future satisfies tokio::spawn's bound.
                    // None holds no actual reference; the annotation is vacuously satisfied.
                    let no_mcp: Option<&'static dyn zeph_subagent::McpDispatch> = None;
                    if let Err(e) = zeph_subagent::hooks::fire_hooks(&hooks, &env, no_mcp).await {
                        tracing::warn!(error = %e, "turn_complete hook failed");
                    }
                },
            );
        }
    }

    /// Publish the active goal snapshot to `MetricsSnapshot` and fire `on_turn_complete`
    /// accounting as a tracked background task.
    fn flush_goal_accounting(&mut self, tokens_before: u64) {
        let goal_snap = self
            .services
            .goal_accounting
            .as_ref()
            .and_then(|a| a.snapshot());
        self.update_metrics(|m| m.active_goal = goal_snap);

        if let Some(ref accounting) = self.services.goal_accounting {
            let tokens_after = self
                .runtime
                .metrics
                .metrics_tx
                .as_ref()
                .map_or(0, |tx| tx.borrow().total_tokens);
            let turn_tokens = tokens_after.saturating_sub(tokens_before);
            let mut spawned: Option<
                std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
            > = None;
            accounting.on_turn_complete(turn_tokens, |fut| {
                spawned = Some(fut);
            });
            if let Some(fut) = spawned {
                let _ = self.runtime.lifecycle.supervisor.spawn(
                    agent_supervisor::TaskClass::Telemetry,
                    "goal-accounting",
                    fut,
                );
            }
        }
    }

    // Returns true if the input was blocked and the caller should return Ok(()) immediately.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "agent.security_prescreen", skip_all)
    )]
    async fn pre_process_security(&mut self, trimmed: &str) -> Result<bool, error::AgentError> {
        // Guardrail: LLM-based prompt injection pre-screening at the user input boundary.
        if let Some(ref guardrail) = self.services.security.guardrail {
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
        if self.services.security.sanitizer.scan_user_input() {
            match self
                .services
                .security
                .sanitizer
                .classify_injection(trimmed)
                .await
            {
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

    /// Run `advance_context_lifecycle` with provider-health gating and a wall-clock timeout.
    ///
    /// Skips context preparation entirely when providers failed on the previous turn and the
    /// `no_providers_backoff_secs` window has not yet elapsed. When providers are available,
    /// wraps the call with `context_prep_timeout_secs` to prevent a stall when embed backends
    /// are rate-limited or unavailable (#3357).
    async fn advance_context_lifecycle_guarded(&mut self, text: &str, trimmed: &str) {
        let backoff_secs = self.runtime.config.timeouts.no_providers_backoff_secs;
        let prep_timeout_secs = self.runtime.config.timeouts.context_prep_timeout_secs;

        // Skip expensive memory recall / embedding when providers are known-down.
        let providers_recently_failed = self
            .runtime
            .lifecycle
            .last_no_providers_at
            .is_some_and(|t| t.elapsed().as_secs() < backoff_secs);

        if providers_recently_failed {
            tracing::warn!(
                backoff_secs,
                "skipping context preparation: providers were unavailable on last turn"
            );
            return;
        }

        let timeout_dur = std::time::Duration::from_secs(prep_timeout_secs);
        match tokio::time::timeout(timeout_dur, self.advance_context_lifecycle(text, trimmed)).await
        {
            Ok(()) => {}
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = prep_timeout_secs,
                    "context preparation timed out; proceeding with degraded context"
                );
            }
        }
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "agent.prepare_context", skip_all)
    )]
    async fn advance_context_lifecycle(&mut self, text: &str, trimmed: &str) {
        // Reset per-message pruning cache at the start of each turn (#2298).
        self.services.mcp.pruning_cache.reset();

        // Extract before rebuild_system_prompt so the value is not tainted
        // by the secrets-bearing system prompt (ConversationId is just an i64).
        let conv_id = self.services.memory.persistence.conversation_id;
        self.rebuild_system_prompt(text).await;

        self.detect_and_record_corrections(trimmed, conv_id).await;
        self.services.learning_engine.tick();
        self.analyze_and_learn().await;
        self.sync_graph_counts().await;

        // Reset per-turn compaction guard FIRST so SideQuest sees a clean slate (C2 fix).
        // complete_focus and maybe_sidequest_eviction set this flag when they run (C1 fix).
        // advance_turn() transitions CompactedThisTurn → Cooling/Ready; all other states
        // pass through unchanged. See CompactionState::advance_turn for ordering guarantees.
        self.context_manager.compaction = self.context_manager.compaction.advance_turn();

        // Tick Focus Agent and SideQuest turn counters (#1850, #1885).
        {
            self.services.focus.tick();

            // SideQuest eviction: runs every N user turns when enabled.
            // Skipped when is_compacted_this_turn (focus truncation or prior eviction ran).
            let sidequest_should_fire = self.services.sidequest.tick();
            if sidequest_should_fire && !self.context_manager.compaction.is_compacted_this_turn() {
                self.maybe_sidequest_eviction();
            }
        }

        // Experience memory: evolution sweep (fire-and-forget). Runs every N user turns,
        // gated on graph + experience config, and only when both stores are attached.
        {
            let cfg = &self.services.memory.extraction.graph_config.experience;
            if cfg.enabled
                && cfg.evolution_sweep_enabled
                && cfg.evolution_sweep_interval > 0
                && self
                    .services
                    .sidequest
                    .turn_counter
                    .checked_rem(cfg.evolution_sweep_interval as u64)
                    == Some(0)
                && let Some(memory) = self.services.memory.persistence.memory.as_ref()
                && let (Some(exp), Some(graph)) =
                    (memory.experience.as_ref(), memory.graph_store.as_ref())
            {
                let exp = std::sync::Arc::clone(exp);
                let graph = std::sync::Arc::clone(graph);
                let threshold = cfg.confidence_prune_threshold;
                let turn = self.services.sidequest.turn_counter;
                let accepted = self.runtime.lifecycle.supervisor.spawn(
                    agent_supervisor::TaskClass::Telemetry,
                    "experience-sweep",
                    async move {
                        match exp.evolution_sweep(graph.as_ref(), threshold).await {
                            Ok(stats) => tracing::info!(
                                turn,
                                self_loops = stats.pruned_self_loops,
                                low_confidence = stats.pruned_low_confidence,
                                "evolution sweep complete",
                            ),
                            Err(e) => tracing::warn!(
                                turn,
                                error = %e,
                                "evolution sweep failed",
                            ),
                        }
                    },
                );
                if !accepted {
                    tracing::warn!(
                        turn = self.services.sidequest.turn_counter,
                        "experience-sweep dropped (telemetry class at capacity)",
                    );
                }
            }
        }

        // Cache-expiry warning (#2715): notify user when prompt cache has likely expired.
        if let Some(warning) = self.cache_expiry_warning() {
            tracing::info!(warning, "cache expiry warning");
            let _ = self.channel.send_status(&warning).await;
        }

        // Time-based microcompact (#2699): strip stale low-value tool outputs before compaction.
        // Zero-LLM-cost; runs only when session gap exceeds configured threshold.
        self.maybe_time_based_microcompact();

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
            .set_memory_confidence(self.services.memory.persistence.last_recall_confidence);

        self.services.learning_engine.reset_reflection();
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

    /// Drain any ready [`zeph_tools::BackgroundCompletion`]s from the channel into
    /// `pending_background_completions`. Bounded by `BACKGROUND_COMPLETION_BUFFER_CAP`;
    /// on overflow the oldest entry is evicted and a placeholder is inserted.
    fn drain_background_completions(&mut self) {
        const BACKGROUND_COMPLETION_BUFFER_CAP: usize = 16;

        let Some(ref mut rx) = self.runtime.lifecycle.background_completion_rx else {
            return;
        };
        // Non-blocking drain: collect all completions that are already ready.
        while let Ok(completion) = rx.try_recv() {
            if self.runtime.lifecycle.pending_background_completions.len()
                >= BACKGROUND_COMPLETION_BUFFER_CAP
            {
                tracing::warn!(
                    run_id = %completion.run_id,
                    "background completion buffer full; dropping run result"
                );
                // Buffer is full: drop the oldest queued completion and push a sentinel
                // for the new (incoming) run so the LLM is informed its result was lost.
                self.runtime
                    .lifecycle
                    .pending_background_completions
                    .pop_front();
                self.runtime
                    .lifecycle
                    .pending_background_completions
                    .push_back(zeph_tools::BackgroundCompletion {
                        run_id: completion.run_id,
                        exit_code: -1,
                        success: false,
                        elapsed_ms: 0,
                        command: completion.command,
                        output: format!(
                            "[background result for run {} dropped: buffer overflow]",
                            completion.run_id
                        ),
                    });
            } else {
                self.runtime
                    .lifecycle
                    .pending_background_completions
                    .push_back(completion);
            }
        }
    }

    /// Format and drain `pending_background_completions` into a prefix string, then
    /// return the final merged text (prefix + user message). When there are no pending
    /// completions the original text is returned unchanged.
    fn build_user_message_text_with_bg_completions(&mut self, user_text: &str) -> String {
        if self
            .runtime
            .lifecycle
            .pending_background_completions
            .is_empty()
        {
            return user_text.to_owned();
        }
        let mut parts = String::new();
        for completion in self
            .runtime
            .lifecycle
            .pending_background_completions
            .drain(..)
        {
            let _ = write!(
                parts,
                "[Background task {} completed]\nexit_code: {}\nsuccess: {}\nelapsed_ms: {}\ncommand: {}\n\n{}\n\n",
                completion.run_id,
                completion.exit_code,
                completion.success,
                completion.elapsed_ms,
                completion.command,
                completion.output,
            );
        }
        parts.push_str(user_text);
        parts
    }

    /// Poll a sub-agent until it reaches a terminal state, bridging secret requests to the
    /// channel. Returns a human-readable status string suitable for sending to the user.
    async fn poll_subagent_until_done(&mut self, task_id: &str, label: &str) -> Option<String> {
        use zeph_subagent::SubAgentState;
        let result = loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // Bridge secret requests from sub-agent to channel.confirm().
            // Fetch the pending request first, then release the borrow before
            // calling channel.confirm() (which requires &mut self).
            #[allow(clippy::redundant_closure_for_method_calls)]
            let pending = self
                .services
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
                if let Some(mgr) = self.services.orchestration.subagent_manager.as_mut() {
                    if approved {
                        let ttl = std::time::Duration::from_mins(5);
                        let key = req.secret_key.clone();
                        if mgr.approve_secret(&req_task_id, &key, ttl).is_ok() {
                            let _ = mgr.deliver_secret(&req_task_id, key);
                        }
                    } else {
                        let _ = mgr.deny_secret(&req_task_id);
                    }
                }
            }

            let mgr = self.services.orchestration.subagent_manager.as_ref()?;
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
                            self.services
                                .orchestration
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
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
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
        let mgr = self.services.orchestration.subagent_manager.as_ref()?;
        let defs = mgr.definitions();
        if defs.is_empty() {
            return Some("No sub-agent definitions found.".into());
        }
        let mut out = String::from("Available sub-agents:\n");
        for d in defs {
            let memory_label = match d.memory {
                Some(zeph_subagent::MemoryScope::User) => " [memory:user]",
                Some(zeph_subagent::MemoryScope::Project) => " [memory:project]",
                Some(zeph_subagent::MemoryScope::Local) => " [memory:local]",
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
        let mgr = self.services.orchestration.subagent_manager.as_ref()?;
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
                && let Ok(dir) = zeph_subagent::memory::resolve_memory_dir(scope, &def.name)
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
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        if let Some((tid, req)) = mgr.try_recv_secret_request()
            && tid == full_id
        {
            let key = req.secret_key.clone();
            let ttl = std::time::Duration::from_mins(5);
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
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        match mgr.deny_secret(&full_id) {
            Ok(()) => Some(format!("Secret request denied for sub-agent '{full_id}'.")),
            Err(e) => Some(format!("Deny failed: {e}")),
        }
    }

    async fn handle_agent_command(&mut self, cmd: zeph_subagent::AgentCommand) -> Option<String> {
        use zeph_subagent::AgentCommand;

        match cmd {
            AgentCommand::List => self.handle_agent_list(),
            AgentCommand::Background { name, prompt } => {
                self.handle_agent_background(&name, &prompt)
            }
            AgentCommand::Spawn { name, prompt }
            | AgentCommand::Mention {
                agent: name,
                prompt,
            } => self.handle_agent_spawn_foreground(&name, &prompt).await,
            AgentCommand::Status => self.handle_agent_status(),
            AgentCommand::Cancel { id } => self.handle_agent_cancel(&id),
            AgentCommand::Approve { id } => self.handle_agent_approve(&id),
            AgentCommand::Deny { id } => self.handle_agent_deny(&id),
            AgentCommand::Resume { id, prompt } => self.handle_agent_resume(&id, &prompt).await,
        }
    }

    fn handle_agent_background(&mut self, name: &str, prompt: &str) -> Option<String> {
        let provider = self.provider.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        let skills = self.filtered_skills_for(name);
        let cfg = self.services.orchestration.subagent_config.clone();
        let spawn_ctx = self.build_spawn_context(&cfg);
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        match mgr.spawn(
            name,
            prompt,
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

    async fn handle_agent_spawn_foreground(&mut self, name: &str, prompt: &str) -> Option<String> {
        let provider = self.provider.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        let skills = self.filtered_skills_for(name);
        let cfg = self.services.orchestration.subagent_config.clone();
        let spawn_ctx = self.build_spawn_context(&cfg);
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        let task_id = match mgr.spawn(
            name,
            prompt,
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

    fn handle_agent_cancel(&mut self, id: &str) -> Option<String> {
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        // Accept prefix match on task_id.
        let ids: Vec<String> = mgr
            .statuses()
            .into_iter()
            .map(|(task_id, _)| task_id)
            .filter(|task_id| task_id.starts_with(id))
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

    async fn handle_agent_resume(&mut self, id: &str, prompt: &str) -> Option<String> {
        let cfg = self.services.orchestration.subagent_config.clone();
        // Resolve definition name from transcript meta before spawning so we can
        // look up skills by definition name rather than the UUID prefix (S1 fix).
        let def_name = {
            let mgr = self.services.orchestration.subagent_manager.as_ref()?;
            match mgr.def_name_for_resume(id, &cfg) {
                Ok(name) => name,
                Err(e) => return Some(format!("Failed to resume sub-agent: {e}")),
            }
        };
        let skills = self.filtered_skills_for(&def_name);
        let provider = self.provider.clone();
        let tool_executor = Arc::clone(&self.tool_executor);
        let mgr = self.services.orchestration.subagent_manager.as_mut()?;
        let (task_id, _) = match mgr.resume(id, prompt, provider, tool_executor, skills, &cfg) {
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

    fn filtered_skills_for(&self, agent_name: &str) -> Option<Vec<String>> {
        let mgr = self.services.orchestration.subagent_manager.as_ref()?;
        let def = mgr.definitions().iter().find(|d| d.name == agent_name)?;
        let reg = self.services.skill.registry.read();
        match zeph_subagent::filter_skills(&reg, &def.skills) {
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
    ) -> zeph_subagent::SpawnContext {
        zeph_subagent::SpawnContext {
            parent_messages: self.extract_parent_messages(cfg),
            parent_cancel: Some(self.runtime.lifecycle.cancel_token.clone()),
            parent_provider_name: {
                let name = &self.runtime.config.active_provider_name;
                if name.is_empty() {
                    None
                } else {
                    Some(name.clone())
                }
            },
            spawn_depth: self.runtime.config.spawn_depth,
            mcp_tool_names: self.extract_mcp_tool_names(),
            // F3 spec 050 §4: propagate seeded score when parent is >= Elevated.
            seed_trajectory_score: {
                let child = self.services.security.trajectory.spawn_child();
                let score = child.score_now();
                if score > 0.0 { Some(score) } else { None }
            },
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

    /// Classify a skill directory's source kind using on-disk markers and the bundled allowlist.
    ///
    /// Must be called from a blocking context (uses synchronous FS I/O).
    fn classify_source_kind(
        skill_dir: &std::path::Path,
        managed_dir: Option<&std::path::PathBuf>,
        bundled_names: &std::collections::HashSet<String>,
    ) -> zeph_memory::store::SourceKind {
        if managed_dir.is_some_and(|d| skill_dir.starts_with(d)) {
            let skill_name = skill_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let has_marker = skill_dir.join(".bundled").exists();
            if has_marker && bundled_names.contains(skill_name) {
                zeph_memory::store::SourceKind::Bundled
            } else {
                if has_marker {
                    tracing::warn!(
                        skill = %skill_name,
                        "skill has .bundled marker but is not in the bundled skill \
                         allowlist — classifying as Hub"
                    );
                }
                zeph_memory::store::SourceKind::Hub
            }
        } else {
            zeph_memory::store::SourceKind::Local
        }
    }

    /// Update trust DB records for all reloaded skills.
    async fn update_trust_for_reloaded_skills(
        &mut self,
        all_meta: &[zeph_skills::loader::SkillMeta],
    ) {
        // Clone Arc before any .await so no &self fields are held across suspension points.
        let memory = self.services.memory.persistence.memory.clone();
        let Some(memory) = memory else {
            return;
        };
        let trust_cfg = self.services.skill.trust_config.clone();
        let managed_dir = self.services.skill.managed_dir.clone();
        let bundled_names: std::collections::HashSet<String> =
            zeph_skills::bundled_skill_names().into_iter().collect();
        for meta in all_meta {
            // Compute hash and classify source_kind in spawn_blocking — both are blocking FS calls
            // (.bundled marker .exists() and compute_skill_hash both do std::fs I/O).
            let skill_dir = meta.skill_dir.clone();
            let managed_dir_ref = managed_dir.clone();
            let bundled_names_ref = bundled_names.clone();
            let fs_result: Option<(String, zeph_memory::store::SourceKind)> =
                tokio::task::spawn_blocking(move || {
                    let hash = zeph_skills::compute_skill_hash(&skill_dir).ok()?;
                    let source_kind = Self::classify_source_kind(
                        &skill_dir,
                        managed_dir_ref.as_ref(),
                        &bundled_names_ref,
                    );
                    Some((hash, source_kind))
                })
                .await
                .unwrap_or(None);

            let Some((current_hash, source_kind)) = fs_result else {
                tracing::warn!("failed to compute hash for '{}'", meta.name);
                continue;
            };
            let initial_level = match source_kind {
                zeph_memory::store::SourceKind::Bundled => &trust_cfg.bundled_level,
                zeph_memory::store::SourceKind::Hub => &trust_cfg.default_level,
                zeph_memory::store::SourceKind::Local | zeph_memory::store::SourceKind::File => {
                    &trust_cfg.local_level
                }
            };
            let existing = memory
                .sqlite()
                .load_skill_trust(&meta.name)
                .await
                .ok()
                .flatten();
            let trust_level_str = if let Some(ref row) = existing {
                if row.blake3_hash != current_hash {
                    trust_cfg.hash_mismatch_level.to_string()
                } else if row.source_kind != source_kind {
                    // source_kind changed (e.g., hub → bundled on upgrade).
                    // Never override an explicit operator block. For active trust levels,
                    // adopt the source-kind initial level when it grants more trust.
                    let stored = row
                        .trust_level
                        .parse::<zeph_common::SkillTrustLevel>()
                        .unwrap_or_else(|_| {
                            tracing::warn!(
                                skill = %meta.name,
                                raw = %row.trust_level,
                                "unrecognised trust_level in DB, treating as quarantined"
                            );
                            zeph_common::SkillTrustLevel::Quarantined
                        });
                    if !stored.is_active() || stored.severity() <= initial_level.severity() {
                        row.trust_level.clone()
                    } else {
                        initial_level.to_string()
                    }
                } else {
                    row.trust_level.clone()
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
    }

    /// Rebuild or sync the in-memory skill matcher and BM25 index after a registry update.
    async fn rebuild_skill_matcher(&mut self, all_meta: &[&zeph_skills::loader::SkillMeta]) {
        let provider = self.embedding_provider.clone();
        let embed_timeout =
            std::time::Duration::from_secs(self.runtime.config.timeouts.embedding_seconds);
        let embed_fn = move |text: &str| -> zeph_skills::matcher::EmbedFuture {
            let owned = text.to_owned();
            let p = provider.clone();
            Box::pin(async move {
                if let Ok(result) = tokio::time::timeout(embed_timeout, p.embed(&owned)).await {
                    result
                } else {
                    tracing::warn!(
                        timeout_secs = embed_timeout.as_secs(),
                        "skill matcher: embedding timed out"
                    );
                    Err(zeph_llm::LlmError::Timeout)
                }
            })
        };

        let needs_inmemory_rebuild = !self
            .services
            .skill
            .matcher
            .as_ref()
            .is_some_and(SkillMatcherBackend::is_qdrant);

        if needs_inmemory_rebuild {
            self.services.skill.matcher = SkillMatcher::new(all_meta, embed_fn)
                .await
                .map(SkillMatcherBackend::InMemory);
        } else if let Some(ref mut backend) = self.services.skill.matcher {
            let _ = self.channel.send_status("syncing skill index...").await;
            let on_progress: Option<Box<dyn Fn(usize, usize) + Send>> =
                self.services.session.status_tx.clone().map(
                    |tx| -> Box<dyn Fn(usize, usize) + Send> {
                        Box::new(move |completed, total| {
                            let msg = format!("Syncing skills: {completed}/{total}");
                            let _ = tx.send(msg);
                        })
                    },
                );
            if let Err(e) = backend
                .sync(
                    all_meta,
                    &self.services.skill.embedding_model,
                    embed_fn,
                    on_progress,
                )
                .await
            {
                tracing::warn!("failed to sync skill embeddings: {e:#}");
            }
        }

        if self.services.skill.hybrid_search {
            let descs: Vec<&str> = all_meta.iter().map(|m| m.description.as_str()).collect();
            let _ = self.channel.send_status("rebuilding search index...").await;
            self.services.skill.rebuild_bm25(&descs);
        }
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "skill.hot_reload", skip_all)
    )]
    async fn reload_skills(&mut self) {
        let old_fp = self.services.skill.fingerprint();
        let reload_paths = if let Some(ref supplier) = self.services.skill.plugin_dirs_supplier {
            let plugin_dirs = supplier();
            let mut paths = self.services.skill.skill_paths.clone();
            for dir in plugin_dirs {
                if !paths.contains(&dir) {
                    paths.push(dir);
                }
            }
            paths
        } else {
            self.services.skill.skill_paths.clone()
        };
        self.services.skill.registry.write().reload(&reload_paths);
        if self.services.skill.fingerprint() == old_fp {
            return;
        }
        let _ = self.channel.send_status("reloading skills...").await;

        let all_meta = self
            .services
            .skill
            .registry
            .read()
            .all_meta()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();

        self.update_trust_for_reloaded_skills(&all_meta).await;

        let all_meta_refs = all_meta.iter().collect::<Vec<_>>();
        self.rebuild_skill_matcher(&all_meta_refs).await;

        let all_skills: Vec<Skill> = {
            let reg = self.services.skill.registry.read();
            reg.all_meta()
                .iter()
                .filter_map(|m| reg.skill(&m.name).ok())
                .collect()
        };
        let trust_map = self.build_skill_trust_map().await;
        let empty_health: HashMap<String, (f64, u32)> = HashMap::new();
        let skills_prompt =
            state::SkillState::rebuild_prompt(&all_skills, &trust_map, &empty_health);
        self.services
            .skill
            .last_skills_prompt
            .clone_from(&skills_prompt);
        let system_prompt = build_system_prompt(&skills_prompt, None);
        if let Some(msg) = self.msg.messages.first_mut() {
            msg.content = system_prompt;
        }

        let _ = self.channel.send_status("").await;
        tracing::info!(
            "reloaded {} skill(s)",
            self.services.skill.registry.read().all_meta().len()
        );
    }

    fn reload_instructions(&mut self) {
        // Drain any additional queued events before reloading to avoid redundant reloads.
        if let Some(ref mut rx) = self.runtime.instructions.reload_rx {
            while rx.try_recv().is_ok() {}
        }
        let Some(ref state) = self.runtime.instructions.reload_state else {
            return;
        };
        let new_blocks = crate::instructions::load_instructions(
            &state.base_dir,
            &state.provider_kinds,
            &state.explicit_files,
            state.auto_detect,
        );
        let old_sources: std::collections::HashSet<_> = self
            .runtime
            .instructions
            .blocks
            .iter()
            .map(|b| &b.source)
            .collect();
        let new_sources: std::collections::HashSet<_> =
            new_blocks.iter().map(|b| &b.source).collect();
        for added in new_sources.difference(&old_sources) {
            tracing::info!(path = %added.display(), "instruction file added");
        }
        for removed in old_sources.difference(&new_sources) {
            tracing::info!(path = %removed.display(), "instruction file removed");
        }
        tracing::info!(
            old_count = self.runtime.instructions.blocks.len(),
            new_count = new_blocks.len(),
            "reloaded instruction files"
        );
        self.runtime.instructions.blocks = new_blocks;
    }

    fn reload_config(&mut self) {
        let Some(path) = self.runtime.lifecycle.config_path.clone() else {
            return;
        };
        let Some(config) = self.load_config_with_overlay(&path) else {
            return;
        };
        let budget_tokens = resolve_context_budget(&config, &self.provider);
        self.runtime.config.security = config.security;
        self.runtime.config.timeouts = config.timeouts;
        self.runtime.config.redact_credentials = config.memory.redact_credentials;
        self.services.memory.persistence.history_limit = config.memory.history_limit;
        self.services.memory.persistence.recall_limit = config.memory.semantic.recall_limit;
        self.services.memory.compaction.summarization_threshold =
            config.memory.summarization_threshold;
        self.services.skill.max_active_skills = config.skills.max_active_skills.get();
        self.services.skill.disambiguation_threshold = config.skills.disambiguation_threshold;
        self.services.skill.min_injection_score = config.skills.min_injection_score;
        self.services.skill.cosine_weight = config.skills.cosine_weight.clamp(0.0, 1.0);
        self.services.skill.hybrid_search = config.skills.hybrid_search;
        self.services.skill.two_stage_matching = config.skills.two_stage_matching;
        self.services.skill.confusability_threshold =
            config.skills.confusability_threshold.clamp(0.0, 1.0);
        config
            .skills
            .generation_provider
            .as_str()
            .clone_into(&mut self.services.skill.generation_provider_name);
        self.services.skill.generation_output_dir =
            config.skills.generation_output_dir.as_deref().map(|p| {
                if let Some(stripped) = p.strip_prefix("~/") {
                    dirs::home_dir()
                        .map_or_else(|| std::path::PathBuf::from(p), |h| h.join(stripped))
                } else {
                    std::path::PathBuf::from(p)
                }
            });

        self.context_manager.budget = Some(
            ContextBudget::new(budget_tokens, 0.20).with_graph_enabled(config.memory.graph.enabled),
        );

        {
            let graph_cfg = &config.memory.graph;
            if graph_cfg.rpe.enabled {
                // Re-create router only if it doesn't exist yet; preserve state on hot-reload.
                if self.services.memory.extraction.rpe_router.is_none() {
                    self.services.memory.extraction.rpe_router =
                        Some(std::sync::Mutex::new(zeph_memory::RpeRouter::new(
                            graph_cfg.rpe.threshold,
                            graph_cfg.rpe.max_skip_turns,
                        )));
                }
            } else {
                self.services.memory.extraction.rpe_router = None;
            }
            self.services.memory.extraction.graph_config = graph_cfg.clone();
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
        self.services
            .memory
            .persistence
            .cross_session_score_threshold = config.memory.cross_session_score_threshold;

        self.services.index.repo_map_tokens = config.index.repo_map_tokens;
        self.services.index.repo_map_ttl =
            std::time::Duration::from_secs(config.index.repo_map_ttl_secs);

        tracing::info!("config reloaded");
    }

    /// Load config from disk, apply plugin overlays, and warn on shell divergence.
    ///
    /// Returns `None` when loading or overlay merge fails (caller keeps prior runtime state).
    fn load_config_with_overlay(&mut self, path: &std::path::Path) -> Option<Config> {
        let mut config = match Config::load(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("config reload failed: {e:#}");
                return None;
            }
        };

        // Re-apply plugin overlays. On error, keep previous runtime state intact.
        let new_overlay = if self.runtime.lifecycle.plugins_dir.as_os_str().is_empty() {
            None
        } else {
            match zeph_plugins::apply_plugin_config_overlays(
                &mut config,
                &self.runtime.lifecycle.plugins_dir,
            ) {
                Ok(o) => Some(o),
                Err(e) => {
                    tracing::warn!(
                        "plugin overlay merge failed during reload: {e:#}; \
                         keeping previous runtime state"
                    );
                    return None;
                }
            }
        };

        // M4: detect shell-level divergence from the baked-in executor and warn loudly.
        // ShellExecutor is not rebuilt on hot-reload; only skill threshold is live.
        // A follow-up P2 issue tracks live-rebuild of ShellExecutor.
        if let Some(ref overlay) = new_overlay {
            self.warn_on_shell_overlay_divergence(overlay, &config);
        }
        Some(config)
    }

    /// React to shell policy divergence detected on hot-reload.
    ///
    /// `blocked_commands` is rebuilt live via `ShellPolicyHandle::rebuild` — no restart needed.
    /// `allowed_commands` cannot be rebuilt (feeds sandbox path intersection at construction time)
    /// — emit a warn + status banner when it changes.
    fn warn_on_shell_overlay_divergence(
        &self,
        new_overlay: &zeph_plugins::ResolvedOverlay,
        config: &Config,
    ) {
        let new_blocked: Vec<String> = {
            let mut v = config.tools.shell.blocked_commands.clone();
            v.sort();
            v
        };
        let new_allowed: Vec<String> = {
            let mut v = config.tools.shell.allowed_commands.clone();
            v.sort();
            v
        };

        let startup = &self.runtime.lifecycle.startup_shell_overlay;
        let blocked_changed = new_blocked != startup.blocked;
        let allowed_changed = new_allowed != startup.allowed;

        // blocked_commands IS rebuilt live — emit info-level confirmation only.
        if blocked_changed && let Some(ref h) = self.runtime.lifecycle.shell_policy_handle {
            h.rebuild(&config.tools.shell);
            tracing::info!(
                blocked_count = h.snapshot_blocked().len(),
                "shell blocked_commands rebuilt from hot-reload"
            );
        }

        // allowed_commands cannot be rebuilt — sandbox path intersection is computed at
        // executor construction time. Warn loudly so the user restarts.
        //
        // Note: when base `allowed_commands` is empty (the default), the overlay's
        // intersection semantics keep it empty, so this branch is silently unreachable
        // for users who do not set a non-empty base list.
        if allowed_changed {
            let msg = "plugin config overlay changed shell allowed_commands; RESTART REQUIRED \
                 for sandbox path recomputation (blocked_commands was rebuilt live)";
            tracing::warn!("{msg}");
            if let Some(ref tx) = self.services.session.status_tx {
                let _ = tx.send(msg.to_owned());
            }
        }

        let _ = new_overlay;
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
    fn maybe_sidequest_eviction(&mut self) {
        // S1 runtime guard: warn when SideQuest is enabled alongside a non-Reactive pruning
        // strategy — the two systems share the same pool of evictable tool outputs and can
        // interfere. Disable sidequest.enabled when pruning_strategy != Reactive.
        if self.services.sidequest.config.enabled {
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
        if self.services.focus.is_active() {
            tracing::debug!("sidequest: skipping — focus session active");
            // Drop any pending result — cursors may be stale relative to focus truncation.
            self.services.compression.pending_sidequest_result = None;
            return;
        }

        // Phase 1: apply pending result from last turn's background LLM call.
        self.sidequest_apply_pending();

        // Phase 2: rebuild cursors and schedule the next background eviction LLM call.
        self.sidequest_schedule_next();
    }

    fn sidequest_apply_pending(&mut self) {
        let Some(handle) = self.services.compression.pending_sidequest_result.take() else {
            return;
        };
        // `try_join` is non-blocking: if the task isn't done yet, `Err(handle)` is returned
        // and we reschedule below.
        let result = match handle.try_join() {
            Ok(result) => result,
            Err(_handle) => {
                // Task still running — drop it; a fresh one is scheduled below.
                tracing::debug!("sidequest: background LLM task not yet complete, rescheduling");
                return;
            }
        };
        match result {
            Ok(Some(evicted_indices)) if !evicted_indices.is_empty() => {
                let cursors_snapshot = self.services.sidequest.tool_output_cursors.clone();
                let freed = self.services.sidequest.apply_eviction(
                    &mut self.msg.messages,
                    &evicted_indices,
                    &self.runtime.metrics.token_counter,
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
                        pass = self.services.sidequest.passes_run,
                        "sidequest eviction complete"
                    );
                    if let Some(ref d) = self.runtime.debug.debug_dumper {
                        d.dump_sidequest_eviction(&cursors_snapshot, &evicted_indices, freed);
                    }
                    if let Some(ref tx) = self.services.session.status_tx {
                        let _ = tx.send(format!("SideQuest evicted {freed} tokens"));
                    }
                } else {
                    // apply_eviction returned 0 — clear spinner so it doesn't dangle.
                    if let Some(ref tx) = self.services.session.status_tx {
                        let _ = tx.send(String::new());
                    }
                }
            }
            Ok(None | Some(_)) => {
                tracing::debug!("sidequest: pending result: no cursors to evict");
                if let Some(ref tx) = self.services.session.status_tx {
                    let _ = tx.send(String::new());
                }
            }
            Err(e) => {
                tracing::debug!("sidequest: background task error: {e}");
                if let Some(ref tx) = self.services.session.status_tx {
                    let _ = tx.send(String::new());
                }
            }
        }
    }

    fn sidequest_schedule_next(&mut self) {
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        self.services
            .sidequest
            .rebuild_cursors(&self.msg.messages, &self.runtime.metrics.token_counter);

        if self.services.sidequest.tool_output_cursors.is_empty() {
            tracing::debug!("sidequest: no eligible cursors");
            return;
        }

        let prompt = self.services.sidequest.build_eviction_prompt();
        let max_eviction_ratio = self.services.sidequest.config.max_eviction_ratio;
        let n_cursors = self.services.sidequest.tool_output_cursors.len();
        // Clone the provider so the spawn closure owns it without borrowing self.
        let provider = self.summary_or_primary_provider().clone();

        let eviction_future = async move {
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
        };
        let handle = self.runtime.lifecycle.task_supervisor.spawn_oneshot(
            std::sync::Arc::from("agent.sidequest.eviction"),
            move || eviction_future,
        );
        self.services.compression.pending_sidequest_result = Some(handle);
        tracing::debug!("sidequest: background LLM eviction task spawned");
        if let Some(ref tx) = self.services.session.status_tx {
            let _ = tx.send("SideQuest: scoring tool outputs...".into());
        }
    }

    /// Return an `McpDispatch` adapter backed by the agent's MCP manager, if present.
    fn mcp_dispatch(&self) -> Option<McpManagerDispatch> {
        self.services
            .mcp
            .manager
            .as_ref()
            .map(|m| McpManagerDispatch(Arc::clone(m)))
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
        if current == self.runtime.lifecycle.last_known_cwd {
            return;
        }
        let old_cwd =
            std::mem::replace(&mut self.runtime.lifecycle.last_known_cwd, current.clone());
        self.services.session.env_context.working_dir = current.display().to_string();

        tracing::info!(
            old = %old_cwd.display(),
            new = %current.display(),
            "working directory changed"
        );

        let _ = self
            .channel
            .send_status("Working directory changed\u{2026}")
            .await;

        let hooks = self.services.session.hooks_config.cwd_changed.clone();
        if !hooks.is_empty() {
            let mut env = std::collections::HashMap::new();
            env.insert("ZEPH_OLD_CWD".to_owned(), old_cwd.display().to_string());
            env.insert("ZEPH_NEW_CWD".to_owned(), current.display().to_string());
            let dispatch = self.mcp_dispatch();
            let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                .as_ref()
                .map(|d| d as &dyn zeph_subagent::McpDispatch);
            if let Err(e) = zeph_subagent::hooks::fire_hooks(&hooks, &env, mcp).await {
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

        let hooks = self
            .services
            .session
            .hooks_config
            .file_changed_hooks
            .clone();
        if !hooks.is_empty() {
            let mut env = std::collections::HashMap::new();
            env.insert(
                "ZEPH_CHANGED_PATH".to_owned(),
                event.path.display().to_string(),
            );
            let dispatch = self.mcp_dispatch();
            let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                .as_ref()
                .map(|d| d as &dyn zeph_subagent::McpDispatch);
            if let Err(e) = zeph_subagent::hooks::fire_hooks(&hooks, &env, mcp).await {
                tracing::warn!(error = %e, "FileChanged hook failed");
            }
        }

        let _ = self.channel.send_status("").await;
    }

    /// If the compression spectrum is enabled and a promotion engine is wired, spawn a
    /// background scan task.
    ///
    /// The task loads the most-recent episodic window from `SemanticMemory`, runs the
    /// greedy clustering scan, and calls `promote` for each qualifying candidate.
    ///
    /// Supervised via [`agent_supervisor::BackgroundSupervisor`] under
    /// [`agent_supervisor::TaskClass::Enrichment`] — dropped under high load rather than
    /// blocking the turn.
    pub(super) fn maybe_spawn_promotion_scan(&mut self) {
        let Some(engine) = self.services.promotion_engine.clone() else {
            return;
        };

        let Some(memory) = self.services.memory.persistence.memory.clone() else {
            return;
        };

        // Use a conservative window cap. The engine's own PromotionConfig thresholds
        // determine whether a cluster actually qualifies; this is just the DB scan limit.
        let promotion_window = 200usize;

        let accepted = self.runtime.lifecycle.supervisor.spawn(
            agent_supervisor::TaskClass::Enrichment,
            "compression_spectrum.promotion_scan",
            async move {
                let span = tracing::info_span!("memory.compression.promote.background");
                let _enter = span.enter();

                let window = match memory.load_promotion_window(promotion_window).await {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::warn!(error = %e, "promotion scan: failed to load window");
                        return;
                    }
                };

                if window.is_empty() {
                    return;
                }

                let candidates = match engine.scan(&window).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "promotion scan: clustering failed");
                        return;
                    }
                };

                for candidate in &candidates {
                    if let Err(e) = engine.promote(candidate).await {
                        tracing::warn!(
                            signature = %candidate.signature,
                            error = %e,
                            "promotion scan: promote failed"
                        );
                    }
                }

                tracing::info!(candidates = candidates.len(), "promotion scan: complete");
            },
        );

        if accepted {
            tracing::debug!("compression_spectrum: promotion scan task enqueued");
        }
    }
}
/// Thin wrapper that implements [`zeph_subagent::McpDispatch`] over an [`Arc<zeph_mcp::McpManager>`].
///
/// Used to pass MCP tool dispatch capability into `fire_hooks` without coupling
/// `zeph-subagent` to `zeph-mcp`.
struct McpManagerDispatch(Arc<zeph_mcp::McpManager>);

impl zeph_subagent::McpDispatch for McpManagerDispatch {
    fn call_tool<'a>(
        &'a self,
        server: &'a str,
        tool: &'a str,
        args: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.0
                .call_tool(server, tool, args)
                .await
                .map(|result| {
                    // Extract text content from the MCP response as a JSON value.
                    let texts: Vec<serde_json::Value> = result
                        .content
                        .iter()
                        .filter_map(|c| {
                            if let rmcp::model::RawContent::Text(t) = &c.raw {
                                Some(serde_json::Value::String(t.text.clone()))
                            } else {
                                None
                            }
                        })
                        .collect();
                    serde_json::Value::Array(texts)
                })
                .map_err(|e| e.to_string())
        })
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

/// Resolve the effective context budget from config, applying the `auto_budget` fallback.
///
/// Mirrors `AppBuilder::auto_budget_tokens` so hot-reload and initial startup use the same
/// logic: if `auto_budget = true` and `context_budget_tokens == 0`, query the provider's
/// context window; if still 0, fall back to 128 000 tokens.
/// Truncate a background run command to at most 80 characters for TUI display.
fn truncate_shell_command(cmd: &str) -> String {
    if cmd.len() <= 80 {
        return cmd.to_owned();
    }
    let end = cmd.floor_char_boundary(79);
    format!("{}…", &cmd[..end])
}

/// Take the first 8 characters of a run-id hex string for compact TUI display.
fn truncate_shell_run_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub(crate) fn resolve_context_budget(config: &Config, provider: &AnyProvider) -> usize {
    let tokens = if config.memory.auto_budget && config.memory.context_budget_tokens == 0 {
        if let Some(ctx_size) = provider.context_window() {
            tracing::info!(
                model_context = ctx_size,
                "auto-configured context budget on reload"
            );
            ctx_size
        } else {
            0
        }
    } else {
        config.memory.context_budget_tokens
    };
    if tokens == 0 {
        tracing::warn!(
            "context_budget_tokens resolved to 0 on reload — using fallback of 128000 tokens"
        );
        128_000
    } else {
        tokens
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use tests::agent_tests;

#[cfg(test)]
mod test_stubs {
    use std::pin::Pin;

    use zeph_commands::{
        CommandContext, CommandError, CommandHandler, CommandOutput, SlashCategory,
    };

    /// Stub slash command registered only in `#[cfg(test)]` builds.
    ///
    /// Triggers the `Some(Err(CommandError))` arm in the session/debug registry
    /// dispatch block so the non-fatal error path can be tested without production
    /// command validation logic.
    pub(super) struct TestErrorCommand;

    impl CommandHandler<CommandContext<'_>> for TestErrorCommand {
        fn name(&self) -> &'static str {
            "/test-error"
        }

        fn description(&self) -> &'static str {
            "Test stub: always returns CommandError"
        }

        fn category(&self) -> SlashCategory {
            SlashCategory::Session
        }

        fn handle<'a>(
            &'a self,
            _ctx: &'a mut CommandContext<'_>,
            _args: &'a str,
        ) -> Pin<
            Box<dyn std::future::Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>,
        > {
            Box::pin(async { Err(CommandError::new("boom")) })
        }
    }
}
