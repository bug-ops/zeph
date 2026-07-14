// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod acp_commands;
mod agent_access_impl;
pub(crate) mod agent_supervisor;
mod autodream;
mod autonomous_turn;
mod builder;
pub use builder::SkillConfigParams;
#[cfg(feature = "cocoon")]
mod cocoon_cmd;
mod command_context_impls;
pub(super) mod compression_feedback;
mod config_reload;
mod context;
mod context_impls;
pub(crate) mod context_manager;
mod corrections;
mod durable_bootstrap;
pub mod error;
mod experiment_cmd;
pub(crate) mod focus;
mod heuristic_promotion;
mod hooks_dispatch;
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
mod quality_hook;
pub(crate) mod rate_limiter;
#[cfg(feature = "scheduler")]
mod scheduler_commands;
#[cfg(feature = "scheduler")]
mod scheduler_loop;
mod scope_commands;
pub mod session_config;
mod session_digest;
pub mod shadow_sentinel;
mod shutdown;
pub(crate) mod sidequest;
mod skill_management;
mod skill_reload;
pub mod slash_commands;
pub mod speculative;
pub(crate) mod state;
mod subagent_commands;
pub(crate) mod task_injection;
pub(crate) mod tool_execution;
pub(crate) mod tool_orchestrator;
mod trace_extraction;
pub mod trajectory;
mod trajectory_commands;
mod trust_commands;
pub mod turn;
mod utils;
pub(crate) mod vigil;
mod worktree_commands;

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
use zeph_skills::matcher::SkillMatcherBackend;
use zeph_skills::prompt::format_skills_prompt;
use zeph_skills::registry::SkillRegistry;
use zeph_tools::executor::{ErasedToolExecutor, ToolExecutor};

use tracing::Instrument as _;

use crate::channel::Channel;
use crate::config::Config;
use crate::context::build_system_prompt;
use zeph_common::text::estimate_tokens;

use loop_event::LoopEvent;
use message_queue::{MAX_AUDIO_BYTES, MAX_IMAGE_BYTES, detect_image_mime};
use state::MessageState;

pub(crate) const DOOM_LOOP_WINDOW: usize = 3;
/// Circuit breaker for the utility gate's `Retrieve` action (#5774): once this many
/// "you MUST call it again" mandates have been issued in a single turn, the gate stops
/// demanding further retrieval detours and lets the requested tool call proceed directly.
pub(crate) const MAX_RETRIEVE_MANDATES_PER_TURN: usize = 3;
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
            quality: None,
            proactive_explorer: None,
            promotion_engine: None,
            taco_compressor: None,
            speculation_engine: None,
            autonomous: crate::goal::AutonomousDriver::new(tokio::time::Duration::from_millis(500)),
            autonomous_registry: crate::goal::AutonomousRegistry::new(),
        };

        let runtime = AgentRuntime {
            config: RuntimeConfig::default(),
            lifecycle: LifecycleState::new(),
            providers: ProviderState::new(initial_prompt_tokens),
            metrics: MetricsState::new(token_counter),
            debug: DebugState::default(),
            instructions: InstructionState::default(),
            ephemeral_plugins: Vec::new(),
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
                history_preloaded: false,
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

    /// Run the agent main loop.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel, LLM provider, or tool execution encounters a fatal error.
    #[tracing::instrument(name = "core.agent.run", skip_all, level = "debug", err)]
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

        // AutoSkill A6: start periodic heuristic promotion task at session startup so it runs
        // even when the main loop exits early due to an error (spec 061). The function guards
        // against double-spawn via a heuristic_promotion_handle.is_some() check.
        self.maybe_start_heuristic_promotion();

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
                        is_guest_context: false,
                        is_from_bot: false,
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
                        self.reload_instructions().await;
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
                        self.services.experiments.handle = None;
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
                            is_guest_context: false,
                            is_from_bot: false,
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
                            is_guest_context: false,
                            is_from_bot: false,
                        };
                        self.drain_channel();
                        self.resolve_message(msg).await
                    }
                    Some(LoopEvent::FileChanged(event)) => {
                        self.handle_file_changed(event).await;
                        continue;
                    }
                    Some(LoopEvent::AutonomousTick) => {
                        if let Err(e) = self.run_autonomous_turn().await {
                            tracing::warn!(error = %e, "autonomous turn error");
                        }
                        continue;
                    }
                    Some(LoopEvent::BgMetricsTick) => {
                        self.reap_background_tasks_and_update_metrics();
                        continue;
                    }
                    Some(LoopEvent::Message(msg)) => {
                        self.services.session.is_guest_context = msg.is_guest_context;
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
            let trusted = self.channel.supports_exit();
            let session_impl = command_context_impls::SessionAccessImpl {
                supports_exit: trusted,
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
                let reg = slash_commands::build_session_debug_registry();

                let mut ctx = zeph_commands::CommandContext {
                    sink: &mut sink_adapter,
                    debug: &mut self.runtime.debug,
                    messages: &mut messages_impl,
                    session: &session_impl,
                    agent: &mut null_agent,
                };
                reg.dispatch(&mut ctx, trimmed, trusted).await
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
                let agent_reg = slash_commands::build_agent_command_registry();

                let mut ctx = zeph_commands::CommandContext {
                    sink: &mut agent_null_sink,
                    debug: &mut agent_null_debug,
                    messages: &mut agent_null_messages,
                    session: &agent_null_session,
                    agent: self,
                };
                // self is reborrowed; ctx drops at end of this block.
                agent_reg.dispatch(&mut ctx, trimmed, trusted).await
            } else {
                None
            };
            // self.channel is available again here (ctx borrow dropped above).

            // S1 fix: drain any pending autonomous session start queued by handle_goal.
            // handle_goal runs inside Box::pin(async move) and cannot borrow &mut self directly,
            // so it writes to pending_start_arc. We consume it here where &mut self is free.
            if let Some((cancelled_id, new_id)) = self.services.autonomous.flush_pending_start() {
                if let Some(cid) = cancelled_id {
                    tracing::info!(
                        goal_id = cid,
                        "autonomous: previous session cancelled for new goal"
                    );
                }
                self.sync_registry_entry();
                tracing::info!(goal_id = new_id, "autonomous: session started");
            }

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

        // AutoSkill A1: extract skill candidates from the completed session trace (spec 056).
        self.maybe_extract_skills_from_trace().await;

        // Flush trace collector on normal exit (C-04: Drop handles error/panic paths). This is
        // the last write of the session's trace.json — nothing runs after it, so unlike the
        // mid-session format-switch site (`state/mod.rs`) there's no latency benefit to firing
        // it and forgetting; await the handle so the write is guaranteed to land instead of
        // racing process/runtime teardown (#6107 critic finding S1).
        if let Some(ref mut tc) = self.runtime.debug.trace_collector
            && let Some(handle) = tc.finish()
            && let Err(e) = handle.await
        {
            tracing::warn!(error = %e, "trace.json write task did not complete");
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
            Some(Ok(zeph_commands::CommandOutput::Message(msg))) => {
                let _ = self.channel.send(&msg).await;
                let _ = self.channel.flush_chunks().await;
                if with_learning {
                    self.maybe_trigger_post_command_learning(command).await;
                }
                DispatchFlow::Continue
            }
            Some(Ok(_)) => {
                let _ = self.channel.flush_chunks().await;
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
        let taken = self
            .runtime
            .providers
            .provider_override
            .as_ref()
            .and_then(|slot| slot.write().take());
        if let Some(new_provider) = taken {
            tracing::debug!(provider = new_provider.name(), "ACP model override applied");
            self.set_provider(new_provider);
        }
    }

    /// The single guarded path for reassigning `self.provider` after construction (#5437,
    /// recurrence guard — S1/M1 of the round-3 critique).
    ///
    /// Every runtime provider swap (`/provider` switch, ACP `set_session_config_option` via
    /// [`Agent::apply_provider_override`], and any future one) **must** go through this method
    /// instead of assigning `self.provider` directly. `Agent::with_secret_registry` masks
    /// `self.provider` once at construction time, but that one-time wrap cannot cover providers
    /// swapped in later — this method re-applies masking on every swap if it's missing, so a
    /// new call site literally cannot ship an unmasked provider by forgetting a step: it would
    /// have to bypass this method and assign the field directly, which is what the `debug_assert`
    /// below catches in every debug/test build.
    ///
    /// A caller that already resolved `provider` through a registry-aware path (e.g.
    /// `build_provider_for_switch` with the registry threaded in) passes an already-`Masked`
    /// value here; wrapping is skipped in that case (`AnyProvider::masked` nesting would be
    /// harmless but wasteful).
    fn set_provider(&mut self, provider: AnyProvider) {
        let provider = match self.services.security.secret_registry.clone() {
            Some(registry) if !matches!(provider, AnyProvider::Masked(_)) => {
                provider.masked(registry as Arc<dyn zeph_llm::masking::OutboundMasker>)
            }
            _ => provider,
        };
        debug_assert!(
            self.services.security.secret_registry.is_none()
                || matches!(provider, AnyProvider::Masked(_)),
            "set_provider invariant violated: secret masking is enabled but the new provider \
             is not wrapped via AnyProvider::masked — every self.provider reassignment must go \
             through Agent::set_provider, never assign the field directly"
        );
        self.provider = provider;
    }

    /// Poll all event sources and return the next [`LoopEvent`].
    ///
    /// Returns `None` when the inbound channel closes (graceful shutdown).
    ///
    /// # Errors
    ///
    /// Propagates channel receive errors.
    #[tracing::instrument(name = "core.agent.next_event", skip_all, level = "debug", err)]
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
            // Autonomous goal tick: fires when a running session is active.
            () = self.services.autonomous.next_tick(),
                if self.services.autonomous.should_tick() => {
                LoopEvent::AutonomousTick
            }
            // Periodic background-metrics refresh: keeps the TUI's bg status segment live
            // during idle time between turns (#6279). Lazily constructed here (not in
            // `LifecycleState::new()`) because `tokio::time::interval` requires an active Tokio
            // runtime, which plain `#[test]`-constructed agents do not have.
            //
            // `interval_at(now + INTERVAL, ...)` defers the *first* tick by a full interval.
            // Plain `tokio::time::interval()` fires its first tick immediately on construction,
            // which — since this is lazily built on the very first `next_event()` poll — raced
            // the pre-existing `self.channel.recv()`/shutdown branches on every agent startup:
            // both were simultaneously ready and `tokio::select!` (unbiased here) could pick
            // `BgMetricsTick`, forcing one spurious extra loop iteration before an
            // already-closed/closing channel was observed (tester-found race).
            _ = self
                .runtime
                .lifecycle
                .bg_metrics_tick
                .get_or_insert_with(|| {
                    let mut iv = tokio::time::interval_at(
                        tokio::time::Instant::now() + state::BG_METRICS_TICK_INTERVAL,
                        state::BG_METRICS_TICK_INTERVAL,
                    );
                    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    iv
                })
                .tick() => {
                LoopEvent::BgMetricsTick
            }
        };
        Ok(Some(event))
    }

    #[tracing::instrument(name = "core.agent.resolve_message", skip_all, level = "debug")]
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
        // Also advance MAGE accumulator (spec 004-16 FR-009) and ingest mapped signals.
        {
            use zeph_memory::shadow::{AuditSignalType as MageSignal, Severity as MageSev};
            let pending: Vec<u8> = {
                let mut q = self.services.security.trajectory_signal_queue.lock();
                std::mem::take(&mut *q)
            };
            self.services.security.mage_accumulator.advance_turn();
            for code in pending {
                self.services
                    .security
                    .trajectory
                    .record(crate::agent::trajectory::RiskSignal::from_code(code));
                // Map signal codes to MAGE AuditSignalType + Severity (spec 004-16 FR-002, FR-007).
                // Code 1=PolicyDeny, 6=VigilMedium, 7=VigilHigh, 2=ExfiltrationRedaction.
                let mage_signal: Option<(MageSignal, MageSev)> = match code {
                    1 => Some((MageSignal::PolicyViolation, MageSev::Medium)),
                    2 => Some((MageSignal::ToolChainAnomaly, MageSev::Medium)),
                    6 => Some((MageSignal::PromptInjectionPattern, MageSev::Medium)),
                    7 => Some((MageSignal::PromptInjectionPattern, MageSev::High)),
                    _ => None,
                };
                if let Some((sig, sev)) = mage_signal {
                    self.services.security.mage_accumulator.ingest(sig, sev);
                }
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
                skill_name: None,
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
        // Spec 050 Phase 2: reset per-turn probe counter before any tool dispatch.
        if let Some(ref sentinel) = self.services.security.shadow_sentinel {
            sentinel.advance_turn();
        }
        // Reset per-turn risk chain state so scores don't bleed across turns.
        if let Some(ref acc) = self.services.security.risk_chain_accumulator {
            acc.reset();
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

        let context = turn::TurnContext::new(id, cancel_token, self.runtime.config.timeouts)
            .with_tool_allowlist(self.runtime.config.channel_tool_allowlist.clone());
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
        // Clear guest context flag: each turn is independently classified.
        self.services.session.is_guest_context = false;
        // Cancel all in-flight speculative handles at turn boundary.
        if let Some(ref engine) = self.services.speculation_engine {
            let metrics = engine.end_turn();
            if metrics.committed > 0 || metrics.cancelled > 0 {
                tracing::debug!(
                    committed = metrics.committed,
                    cancelled = metrics.cancelled,
                    wasted_ms = metrics.wasted_ms,
                    "speculation: turn boundary metrics"
                );
            }
        }
    }

    #[tracing::instrument(
        name = "core.agent.process_user_message",
        skip_all,
        level = "debug",
        fields(turn_id),
        err
    )]
    async fn process_user_message(
        &mut self,
        text: String,
        image_parts: Vec<zeph_llm::provider::MessagePart>,
    ) -> Result<(), error::AgentError> {
        // Re-check for a pending provider override (#5548): the loop-top check in `run()`
        // happens before the potentially long block on `next_event()`, so an ACP
        // `session/set_config_option` model switch written while this iteration was
        // parked would otherwise miss this turn and only apply on the next one.
        self.apply_provider_override();

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

    #[allow(clippy::too_many_lines)] // turn pipeline is inherently sequential; each step is a single call
    #[tracing::instrument(
        name = "core.agent.process_user_message_inner",
        skip_all,
        level = "debug",
        err
    )]
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

        // #5460: sanitize only after both dispatch layers ran on unsanitized text.
        let text = self.sanitize_channel_text_if_untrusted(text);
        let trimmed_owned = text.trim().to_owned();
        let trimmed = trimmed_owned.as_str();

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
        // Emit projected token count so TUI can display it before the LLM call.
        let _ = self
            .channel
            .send_context_estimate(
                usize::try_from(self.runtime.providers.cached_prompt_tokens).unwrap_or(usize::MAX),
            )
            .await;

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

        // Emit pre-LLM context size so the TUI gauge is non-zero before the provider responds.
        let context_estimate = self.runtime.providers.cached_prompt_tokens;
        self.update_metrics(|m| m.context_tokens = context_estimate);

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
    /// Called at the top of each turn, before any user message processing, and — since #6279 —
    /// also on every `LoopEvent::BgMetricsTick` (a periodic idle-time tick), so the TUI's
    /// background-work status segment reflects real in-flight enrichment/telemetry tasks
    /// continuously, not only at turn boundaries.
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
            notifier.fire(&summary, &mut self.runtime.lifecycle.supervisor);
        }

        // 2) turn_complete hooks — fire-and-forget via supervisor.
        // McpManagerDispatch wraps Arc<McpManager> and is 'static, so it can be moved
        // into the async block satisfying tokio::spawn's bound. The &dyn McpDispatch
        // borrow is created inside the future from the owned dispatch value.
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
            let conv_id_str = self
                .services
                .memory
                .persistence
                .conversation_id
                .map(|id| id.0.to_string());
            crate::agent::hooks_dispatch::insert_main_agent_ctx(&mut env, conv_id_str.as_deref());
            let dispatch = self.mcp_dispatch();
            let _span = tracing::info_span!("core.agent.turn_hooks").entered();
            let _accepted = self.runtime.lifecycle.supervisor.spawn(
                agent_supervisor::TaskClass::Telemetry,
                "turn-complete-hooks",
                async move {
                    let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                        .as_ref()
                        .map(|d| d as &dyn zeph_subagent::McpDispatch);
                    if let Err(e) = zeph_subagent::hooks::fire_hooks(&hooks, &env, mcp, None).await
                    {
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

    /// Sanitize `text` when it originates from an untrusted channel (#5460).
    ///
    /// Telegram/Discord/Slack users are external and untrusted, but recognized commands must
    /// dispatch on raw text — by the time this is called, both dispatch layers
    /// (`Agent::run`'s registries and `dispatch_slash_command`) have already run on the
    /// unsanitized text and found no match. Only the residual text that actually reaches the
    /// LLM context is wrapped here, mirroring how gateway webhooks and A2A messages are already
    /// sanitized before they reach this function (`src/gateway_spawn.rs::forward_webhooks`,
    /// `src/daemon.rs::AgentTaskProcessor`) — `LoopbackChannel` (which carries both) reports
    /// `requires_input_sanitization() == false` so that pre-sanitized content isn't wrapped
    /// twice.
    fn sanitize_channel_text_if_untrusted(&self, text: String) -> String {
        if !self.channel.requires_input_sanitization() {
            return text;
        }
        self.services
            .security
            .sanitizer
            .sanitize(
                &text,
                zeph_sanitizer::ContentSource::new(
                    zeph_sanitizer::ContentSourceKind::ChannelMessage,
                ),
            )
            .body
    }

    // Returns true if the input was blocked and the caller should return Ok(()) immediately.
    #[tracing::instrument(
        name = "core.agent.pre_process_security",
        skip_all,
        level = "debug",
        err
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
                _ => {}
            }
        }

        // SONAR NLI: probabilistic entailment check at the user input boundary. Observe-only —
        // never blocks, mirrors the tool-output check in `sanitize_tool_output`.
        self.record_nli_verdict(trimmed, "user_input").await;

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
                _ => {}
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

    #[tracing::instrument(
        name = "core.agent.advance_context_lifecycle",
        skip_all,
        level = "debug"
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
        self.context_manager
            .set_compaction_state(self.context_manager.compaction_state().advance_turn());

        // Tick Focus Agent and SideQuest turn counters (#1850, #1885).
        {
            self.services.focus.tick();

            // SideQuest eviction: runs every N user turns when enabled.
            // Skipped when is_compacted_this_turn (focus truncation or prior eviction ran).
            let sidequest_should_fire = self.services.sidequest.tick();
            if sidequest_should_fire
                && !self
                    .context_manager
                    .compaction_state()
                    .is_compacted_this_turn()
            {
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
            self.channel.send_status_best_effort(&warning).await;
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
            }
            .instrument(tracing::info_span!("memory.compression.promote.background")),
        );

        if accepted {
            tracing::debug!("compression_spectrum: promotion scan task enqueued");
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

/// How the effective context-token budget was determined by [`resolve_context_budget_tokens`].
///
/// Callers use this to choose a diagnostic log message appropriate to their call site
/// (initial startup vs. config hot-reload) while sharing the same resolution algorithm.
pub enum ContextBudgetSource {
    /// Auto-detected from the provider's advertised context window.
    AutoDetected(usize),
    /// Explicit `memory.context_budget_tokens` config value, or `auto_budget` disabled.
    Configured,
    /// Neither the config nor the provider yielded a usable value; the hardcoded fallback was used.
    Fallback,
}

/// Resolve the effective context-token budget, shared by initial startup
/// (`AppBuilder::auto_budget_tokens`) and config hot-reload (`resolve_context_budget`).
///
/// If `auto_budget` is enabled and no explicit budget is configured, uses the provider's
/// reported context window. Falls back to a hardcoded 128 000 tokens if the resolved value
/// would otherwise be 0, to guarantee that compaction fires rather than being silently skipped.
pub fn resolve_context_budget_tokens(
    config: &Config,
    provider: &AnyProvider,
) -> (usize, ContextBudgetSource) {
    if config.memory.auto_budget && config.memory.context_budget_tokens == 0 {
        return match provider.context_window() {
            Some(ctx_size) if ctx_size > 0 => {
                (ctx_size, ContextBudgetSource::AutoDetected(ctx_size))
            }
            _ => (128_000, ContextBudgetSource::Fallback),
        };
    }
    if config.memory.context_budget_tokens == 0 {
        return (128_000, ContextBudgetSource::Fallback);
    }
    (
        config.memory.context_budget_tokens,
        ContextBudgetSource::Configured,
    )
}

pub(crate) fn resolve_context_budget(config: &Config, provider: &AnyProvider) -> usize {
    let (tokens, source) = resolve_context_budget_tokens(config, provider);
    match source {
        ContextBudgetSource::AutoDetected(ctx_size) => tracing::info!(
            model_context = ctx_size,
            "auto-configured context budget on reload"
        ),
        ContextBudgetSource::Fallback => tracing::warn!(
            "context_budget_tokens resolved to 0 on reload — using fallback of 128000 tokens"
        ),
        ContextBudgetSource::Configured => {}
    }
    tokens
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

        fn requires_auth(&self) -> bool {
            true
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
