// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ContextService`] — stateless façade for agent context-assembly operations.

use zeph_context::budget::ContextBudget;
use zeph_context::fidelity::FidelityScorer;
use zeph_llm::LlmProvider;
use zeph_llm::provider::{Message, MessagePart, Role};

use crate::error::ContextError;
use crate::helpers::{
    CODE_CONTEXT_PREFIX, CORRECTIONS_PREFIX, CROSS_SESSION_PREFIX, DOCUMENT_RAG_PREFIX,
    GRAPH_FACTS_PREFIX, LSP_NOTE_PREFIX, PERSONA_PREFIX, REASONING_PREFIX, RECALL_PREFIX,
    SESSION_DIGEST_PREFIX, SUMMARY_PREFIX, TRAJECTORY_PREFIX, TREE_MEMORY_PREFIX,
};
use crate::state::{
    ContextAssemblyView, ContextDelta, ContextSummarizationView, MessageWindowView,
    ProviderHandles, StatusSink,
};

/// Configuration parameters for semantic recall injection.
///
/// Collects the 8 config-like arguments shared between the tiered and flat recall paths so
/// callers do not need to pass them positionally to [`ContextService::inject_semantic_recall_bare`].
///
/// `window` and `memory` are kept as direct parameters on the method because they are
/// mutable/output args rather than configuration.
pub struct SemanticRecallParams<'a> {
    /// Query string used for retrieval.
    pub query: &'a str,
    /// Maximum number of tokens the injected recall may consume.
    pub token_budget: usize,
    /// Maximum number of memories to retrieve (flat path only).
    pub recall_limit: usize,
    /// Format applied when serialising recalled memories.
    pub context_format: zeph_config::ContextFormat,
    /// Conversation scope used for tiered retrieval.
    pub conversation_id: Option<zeph_memory::ConversationId>,
    /// Optional LLM provider for intent classification (tiered path).
    pub tiered_classifier: Option<&'a std::sync::Arc<zeph_llm::any::AnyProvider>>,
    /// Optional LLM provider for result validation (tiered path).
    pub tiered_validator: Option<&'a std::sync::Arc<zeph_llm::any::AnyProvider>>,
    /// Tiered retrieval configuration controlling whether the tiered path is active.
    pub tiered_config: &'a zeph_config::memory::TieredRetrievalConfig,
}

/// Stateless façade for agent context-assembly operations.
///
/// This struct has no fields. All state flows through method parameters, which allows the
/// borrow checker to see disjoint `&mut` borrows at the call site without hiding them
/// inside an opaque bundle.
///
/// Methods are `&self` — the type exists only to namespace the operations and give callers
/// a single import.
///
/// # Examples
///
/// ```no_run
/// use zeph_agent_context::service::ContextService;
///
/// let svc = ContextService::new();
/// // call svc.prepare_context(...) or svc.clear_history(...)
/// ```
#[derive(Debug, Default)]
pub struct ContextService;

impl ContextService {
    /// Create a new stateless `ContextService`.
    ///
    /// This is a zero-cost constructor — the struct has no fields.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    // ── Trivial message-window mutators (PR1) ─────────────────────────────────

    /// Clear the message history, preserving the system prompt.
    ///
    /// Keeps the first message (system prompt), clears the rest, and clears
    /// `completed_tool_ids` — session-scoped dependency state resets with the history.
    /// Recomputes `cached_prompt_tokens` inline after clearing.
    pub fn clear_history(&self, window: &mut MessageWindowView<'_>) {
        let system_prompt = window.messages.first().cloned();
        window.messages.clear();
        if let Some(sp) = system_prompt {
            window.messages.push(sp);
        }
        window.completed_tool_ids.clear();
        recompute_prompt_tokens(window);
    }

    /// Remove semantic recall messages from the window.
    pub fn remove_recall_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_part_or_prefix(window.messages, RECALL_PREFIX, |p| {
            matches!(p, MessagePart::Recall { .. })
        });
    }

    /// Remove past-correction messages from the window.
    pub fn remove_correction_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, CORRECTIONS_PREFIX);
    }

    /// Remove knowledge-graph fact messages from the window.
    pub fn remove_graph_facts_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, GRAPH_FACTS_PREFIX);
    }

    /// Remove persona-facts messages from the window.
    pub fn remove_persona_facts_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, PERSONA_PREFIX);
    }

    /// Remove trajectory-hint messages from the window.
    pub fn remove_trajectory_hints_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, TRAJECTORY_PREFIX);
    }

    /// Remove tree-memory summary messages from the window.
    pub fn remove_tree_memory_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, TREE_MEMORY_PREFIX);
    }

    /// Remove reasoning-strategy messages from the window.
    pub fn remove_reasoning_strategies_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, REASONING_PREFIX);
    }

    /// Remove previously injected LSP context notes from the window.
    ///
    /// Called before injecting fresh notes each turn so stale diagnostics/hover
    /// data from the previous tool call do not accumulate across iterations.
    pub fn remove_lsp_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, LSP_NOTE_PREFIX);
    }

    /// Remove code-context (repo-map / file context) messages from the window.
    pub fn remove_code_context_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_part_or_prefix(window.messages, CODE_CONTEXT_PREFIX, |p| {
            matches!(p, MessagePart::CodeContext { .. })
        });
    }

    /// Remove session-summary messages from the window.
    pub fn remove_summary_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_part_or_prefix(window.messages, SUMMARY_PREFIX, |p| {
            matches!(p, MessagePart::Summary { .. })
        });
    }

    /// Remove cross-session context messages from the window.
    pub fn remove_cross_session_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_part_or_prefix(window.messages, CROSS_SESSION_PREFIX, |p| {
            matches!(p, MessagePart::CrossSession { .. })
        });
    }

    /// Remove the session-digest user message from the window.
    pub fn remove_session_digest_message(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::User, SESSION_DIGEST_PREFIX);
    }

    /// Remove document-RAG messages from the window.
    pub fn remove_document_rag_messages(&self, window: &mut MessageWindowView<'_>) {
        remove_by_prefix(window.messages, Role::System, DOCUMENT_RAG_PREFIX);
    }

    /// Trim the non-system message tail to fit within `token_budget` tokens.
    ///
    /// Keeps the system prefix intact and the most recent messages, removing
    /// older messages from the start of the conversation history until the
    /// token count fits the budget. Recomputes `cached_prompt_tokens` after trimming.
    ///
    /// No-op when `token_budget` is zero.
    pub fn trim_messages_to_budget(&self, window: &mut MessageWindowView<'_>, token_budget: usize) {
        if token_budget == 0 {
            return;
        }

        // Find the first non-system message index (skip system prefix).
        let history_start = window
            .messages
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(window.messages.len());

        if history_start >= window.messages.len() {
            return;
        }

        let mut total = 0usize;
        let mut keep_from = window.messages.len();

        for i in (history_start..window.messages.len()).rev() {
            let msg_tokens = window
                .token_counter
                .count_message_tokens(&window.messages[i]);
            if total + msg_tokens > token_budget {
                break;
            }
            total += msg_tokens;
            keep_from = i;
        }

        if keep_from > history_start {
            let removed = keep_from - history_start;
            window.messages.drain(history_start..keep_from);
            recompute_prompt_tokens(window);
            tracing::info!(
                removed,
                token_budget,
                "trimmed messages to fit context budget"
            );
        }
    }

    // ── prepare_context family (PR2) ─────────────────────────────────────────

    /// Inject semantic recall messages into the window for the given query.
    ///
    /// Removes any existing recall messages first, fetches fresh recall up to
    /// `token_budget` tokens, and inserts the result at position 1 (immediately
    /// after the system prompt).
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if the recall backend returns an error.
    #[tracing::instrument(name = "agent_context.service.inject_semantic_recall", skip_all, err)]
    pub async fn inject_semantic_recall(
        &self,
        query: &str,
        token_budget: usize,
        window: &mut MessageWindowView<'_>,
        view: &ContextAssemblyView<'_>,
    ) -> Result<(), ContextError> {
        self.remove_recall_messages(window);

        let params = SemanticRecallParams {
            query,
            token_budget,
            recall_limit: view.recall_limit,
            context_format: view.context_format,
            conversation_id: view.conversation_id,
            tiered_classifier: view.tiered_retrieval_classifier.as_ref(),
            tiered_validator: view.tiered_retrieval_validator.as_ref(),
            tiered_config: &view.tiered_retrieval_config,
        };
        let msg = self
            .run_tiered_recall(&params, window, view.memory.as_deref())
            .await?;

        if let Some(msg) = msg
            && window.messages.len() > 1
        {
            window.messages.insert(1, msg);
        }

        Ok(())
    }

    /// Inject semantic recall without a full [`ContextAssemblyView`].
    ///
    /// This variant is called from `Agent::inject_semantic_recall` in `zeph-core`, where
    /// constructing a full `ContextAssemblyView` would require duplicating all of
    /// `prepare_context`'s setup. It carries only the fields that
    /// `inject_semantic_recall` actually reads, enabling tiered retrieval on the
    /// hot-path turn loop without the overhead of the full view.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if the recall backend returns an error.
    #[tracing::instrument(
        name = "agent_context.service.inject_semantic_recall_bare",
        skip_all,
        err
    )]
    pub async fn inject_semantic_recall_bare(
        &self,
        params: SemanticRecallParams<'_>,
        window: &mut MessageWindowView<'_>,
        memory: Option<&zeph_memory::semantic::SemanticMemory>,
    ) -> Result<(), ContextError> {
        self.remove_recall_messages(window);

        let msg = self.run_tiered_recall(&params, window, memory).await?;

        if let Some(msg) = msg
            && window.messages.len() > 1
        {
            window.messages.insert(1, msg);
        }

        Ok(())
    }

    /// Execute tiered or flat semantic recall and return the message to inject, if any.
    ///
    /// Both `inject_semantic_recall` and `inject_semantic_recall_bare` share identical
    /// retrieval logic; this method holds the single implementation.
    async fn run_tiered_recall(
        &self,
        params: &SemanticRecallParams<'_>,
        window: &MessageWindowView<'_>,
        memory: Option<&zeph_memory::semantic::SemanticMemory>,
    ) -> Result<Option<Message>, ContextError> {
        if params.tiered_config.enabled {
            use tracing::Instrument as _;
            let Some(mem) = memory else {
                return Ok(None);
            };
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                zeph_memory::recall_tiered(
                    mem,
                    params.query,
                    params.conversation_id,
                    params.tiered_classifier,
                    params.tiered_validator,
                    params.tiered_config,
                    Some(params.token_budget),
                )
                .instrument(tracing::info_span!("agent_context.tiered_retrieval.recall")),
            )
            .await
            .map_err(|_| {
                tracing::warn!("tiered_retrieval: recall_tiered timed out after 30s");
                ContextError::Memory(zeph_memory::MemoryError::Timeout(
                    "recall_tiered timed out".to_owned(),
                ))
            })?
            .map_err(ContextError::Memory)?;

            tracing::debug!(
                intent = %result.intent,
                tokens_used = result.tokens_used,
                tier_escalated = result.tier_escalated,
                count = result.messages.len(),
                "tiered_retrieval: recall complete"
            );

            if result.messages.is_empty() {
                return Ok(None);
            }

            let recalled_text = result
                .messages
                .iter()
                .map(|m| m.message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n---\n");
            Ok(Some(Message::from_legacy(
                Role::User,
                format!("{RECALL_PREFIX}{recalled_text}"),
            )))
        } else {
            let (msg, _score) = crate::helpers::fetch_semantic_recall_raw(
                memory,
                params.recall_limit,
                params.context_format,
                params.query,
                params.token_budget,
                &window.token_counter,
                None,
                None,
            )
            .await?;
            Ok(msg)
        }
    }

    /// Inject cross-session context messages into the window for the given query.
    ///
    /// Removes any existing cross-session messages first, fetches fresh cross-session
    /// context for the current conversation, and inserts the result at position 1.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if the memory backend returns an error.
    #[tracing::instrument(
        name = "agent_context.service.inject_cross_session_context",
        skip_all,
        err
    )]
    pub async fn inject_cross_session_context(
        &self,
        query: &str,
        token_budget: usize,
        window: &mut MessageWindowView<'_>,
        view: &ContextAssemblyView<'_>,
    ) -> Result<(), ContextError> {
        self.remove_cross_session_messages(window);

        if let Some(msg) = crate::helpers::fetch_cross_session_raw(
            view.memory.as_deref(),
            view.conversation_id,
            view.cross_session_score_threshold,
            query,
            token_budget,
            &view.token_counter,
        )
        .await?
            && window.messages.len() > 1
        {
            window.messages.insert(1, msg);
            tracing::debug!("injected cross-session context");
        }

        Ok(())
    }

    /// Inject conversation-summary messages into the window.
    ///
    /// Removes any existing summary messages first, fetches stored summaries for the
    /// current conversation, and inserts the result at position 1.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if the memory backend returns an error.
    #[tracing::instrument(name = "agent_context.service.inject_summaries", skip_all, err)]
    pub async fn inject_summaries(
        &self,
        token_budget: usize,
        window: &mut MessageWindowView<'_>,
        view: &ContextAssemblyView<'_>,
    ) -> Result<(), ContextError> {
        self.remove_summary_messages(window);

        if let Some(msg) = crate::helpers::fetch_summaries_raw(
            view.memory.as_deref(),
            view.conversation_id,
            token_budget,
            &view.token_counter,
        )
        .await?
            && window.messages.len() > 1
        {
            window.messages.insert(1, msg);
            tracing::debug!("injected summaries into context");
        }

        Ok(())
    }

    /// Select the best-matching skill among ambiguous candidates via an LLM classification call.
    ///
    /// Returns the reordered index list with the most likely skill first, or `None` if the
    /// LLM call fails (caller falls back to original score order).
    #[tracing::instrument(name = "agent_context.service.disambiguate_skills", skip_all)]
    pub async fn disambiguate_skills(
        &self,
        query: &str,
        all_meta: &[&zeph_skills::loader::SkillMeta],
        scored: &[zeph_skills::ScoredMatch],
        providers: &ProviderHandles,
    ) -> Option<Vec<usize>> {
        use std::fmt::Write as _;

        let mut candidates = String::new();
        for sm in scored {
            if let Some(meta) = all_meta.get(sm.index) {
                let _ = writeln!(
                    candidates,
                    "- {} (score: {:.3}): {}",
                    meta.name, sm.score, meta.description
                );
            }
        }

        let prompt = format!(
            "The user said: \"{query}\"\n\n\
             These skills matched with similar scores:\n{candidates}\n\
             Which skill best matches the user's intent? \
             Return the skill_name, your confidence (0-1), and any extracted parameters."
        );

        let messages = vec![zeph_llm::provider::Message::from_legacy(
            zeph_llm::provider::Role::User,
            prompt,
        )];
        match providers
            .disambiguate
            .chat_typed::<zeph_skills::IntentClassification>(&messages)
            .await
        {
            Ok(classification) => {
                tracing::info!(
                    skill = %classification.skill_name,
                    confidence = classification.confidence,
                    "disambiguation selected skill"
                );
                let mut indices: Vec<usize> = scored.iter().map(|s| s.index).collect();
                if let Some(pos) = indices.iter().position(|&i| {
                    all_meta
                        .get(i)
                        .is_some_and(|m| m.name == classification.skill_name)
                }) {
                    indices.swap(0, pos);
                }
                Some(indices)
            }
            Err(e) => {
                tracing::warn!("disambiguation failed, using original order: {e:#}");
                None
            }
        }
    }

    /// Prepare the context window for the current turn.
    ///
    /// Removes stale injection messages, runs proactive skill exploration, gathers
    /// semantic recall and graph facts via the concurrent assembler, applies the
    /// retrieval policy, and injects fresh context. Returns a [`ContextDelta`] whose
    /// `code_context` field must be applied by the caller (via `inject_code_context`).
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if recall fails or [`ContextError::Assembler`]
    /// if the context assembler encounters an internal error.
    #[allow(clippy::too_many_lines)] // sequential context-assembly pipeline; splitting would reduce readability
    #[tracing::instrument(name = "agent_context.service.prepare_context", skip_all, err)]
    pub async fn prepare_context(
        &self,
        query: &str,
        window: &mut MessageWindowView<'_>,
        view: &mut ContextAssemblyView<'_>,
    ) -> Result<ContextDelta, ContextError> {
        if view.context_manager.budget.is_none() {
            return Ok(ContextDelta::default());
        }

        // Remove stale injected messages before concurrent fetch.
        self.remove_session_digest_message(window);
        self.remove_summary_messages(window);
        self.remove_cross_session_messages(window);
        self.remove_recall_messages(window);
        self.remove_document_rag_messages(window);
        self.remove_correction_messages(window);
        self.remove_code_context_messages(window);
        self.remove_graph_facts_messages(window);
        self.remove_persona_facts_messages(window);
        self.remove_trajectory_hints_messages(window);
        self.remove_tree_memory_messages(window);
        if view.reasoning_config.enabled {
            self.remove_reasoning_strategies_messages(window);
        }

        // Proactive world-knowledge exploration (feature-gated, #3320).
        if let Some(explorer) = view.proactive_explorer.clone()
            && let Some(domain) = explorer.classify(query)
        {
            let already_known = {
                let registry_guard = view.skill_registry.read();
                explorer.has_knowledge(&registry_guard, &domain)
            };
            let excluded = explorer.is_excluded(&domain);

            if !already_known && !excluded {
                tracing::debug!(domain = %domain.0, query_len = query.len(), "proactive.explore triggered");
                let timeout_ms = explorer.timeout_ms();
                let result = tokio::time::timeout(
                    std::time::Duration::from_millis(timeout_ms),
                    explorer.explore(&domain),
                )
                .await;
                match result {
                    Ok(Ok(())) => {
                        view.skill_registry.write().reload(view.skill_paths);
                        tracing::debug!(domain = %domain.0, "proactive.explore complete, registry reloaded");
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(domain = %domain.0, error = %e, "proactive exploration failed");
                    }
                    Err(_) => {
                        tracing::warn!(domain = %domain.0, timeout_ms, "proactive exploration timed out");
                    }
                }
            }
        }

        // Compression-spectrum retrieval policy (#3305, #3455).
        let active_levels: &'static [zeph_memory::compression::CompressionLevel] =
            if let Some(ref budget) = view.context_manager.budget {
                let used = view.cached_prompt_tokens;
                let max = budget.max_tokens();
                #[allow(clippy::cast_precision_loss)]
                let remaining_ratio = if max == 0 {
                    1.0_f32
                } else {
                    1.0 - (used as f32 / max as f32).clamp(0.0, 1.0)
                };
                let levels =
                    zeph_memory::compression::RetrievalPolicy::default().select(remaining_ratio);
                tracing::debug!(
                    remaining_ratio,
                    active_levels = ?levels,
                    "compression_spectrum: retrieval policy selected"
                );
                levels
            } else {
                &[]
            };

        let memory_backend: Option<std::sync::Arc<dyn zeph_common::memory::ContextMemoryBackend>> =
            view.memory.clone().map(
                |m| -> std::sync::Arc<dyn zeph_common::memory::ContextMemoryBackend> {
                    std::sync::Arc::new(crate::memory_backend::SemanticMemoryBackend::new(m))
                },
            );

        let memory_view = zeph_context::input::ContextMemoryView {
            memory: memory_backend,
            conversation_id: view.conversation_id.map(|c| c.0),
            recall_limit: view.recall_limit,
            cross_session_score_threshold: view.cross_session_score_threshold,
            context_strategy: view.context_strategy,
            crossover_turn_threshold: view.crossover_turn_threshold,
            cached_session_digest: view.cached_session_digest.clone(),
            graph_config: view.graph_config.clone(),
            document_config: view.document_config.clone(),
            persona_config: view.persona_config.clone(),
            trajectory_config: view.trajectory_config.clone(),
            reasoning_config: view.reasoning_config.clone(),
            memcot_config: view.memcot_config.clone(),
            memcot_state: view.memcot_state.clone(),
            tree_config: view.tree_config.clone(),
        };

        #[cfg(feature = "index")]
        let index_access = view.index;
        #[cfg(not(feature = "index"))]
        let index_access: Option<&dyn zeph_context::input::IndexAccess> = None;

        let router = crate::memory_backend::build_memory_router(view.context_manager);

        let input = zeph_context::input::ContextAssemblyInput {
            memory: &memory_view,
            context_manager: view.context_manager,
            token_counter: &*view.token_counter,
            skills_prompt: view.last_skills_prompt,
            index: index_access,
            correction_config: view.correction_config,
            sidequest_turn_counter: view.sidequest_turn_counter,
            messages: window.messages,
            query,
            scrub: view.scrub,
            active_levels,
            router,
            planned_next_tools: view.planned_next_tools,
        };

        let mut prepared = zeph_context::assembler::ContextAssembler::gather(&input).await?;

        // When tiered retrieval is enabled, suppress the flat recall assembled above and
        // replace it with the tiered result injected directly into the window.  The span
        // `agent_context.tiered_retrieval.recall` will appear in traces for every enabled
        // turn, satisfying the observability requirement in issue #3996.
        if view.tiered_retrieval_config.enabled {
            prepared.recall = None;
        }

        // Drain background handles produced during assembly (e.g. mark_reasoning_used) and
        // register them with the supervisor so they are tracked and abortable.  Must happen
        // before `apply_prepared_context` consumes `prepared` to avoid silent drops.
        for handle in prepared.background_tasks.drain(..) {
            let task_supervisor = std::sync::Arc::clone(&view.task_supervisor);
            task_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                name: "context.assembly.background",
                restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                factory: {
                    let cell = std::sync::Arc::new(std::sync::Mutex::new(Some(async move {
                        let _ = handle.await;
                    })));
                    move || {
                        let f = cell.lock().ok().and_then(|mut g| g.take());
                        async move {
                            if let Some(f) = f {
                                f.await;
                            }
                        }
                    }
                },
            });
        }

        let (delta, inserted_count) = self.apply_prepared_context(window, view, prepared).await;

        if view.tiered_retrieval_config.enabled {
            self.inject_semantic_recall(query, usize::MAX, window, view)
                .await?;
        }

        // T-06: Fidelity scoring (INV-01: AFTER apply_prepared_context returns).
        // Guard: skip when MemoryFirst is active (INV-11 / AC-09) or config absent/disabled.
        // Spec AC-09: when memory_first=true the scorer MUST NOT run — the caller (here) is
        // responsible for this bypass; FidelityScorer itself is stateless and has no memory of it.
        let memory_first_active =
            view.context_strategy == zeph_config::ContextStrategy::MemoryFirst;
        if let Some(fidelity_cfg) = view.fidelity_config
            && fidelity_cfg.enabled
            && !memory_first_active
        {
            use tracing::Instrument as _;
            if let Some(ref tx) = view.status_tx {
                let _ = tx.send("Scoring context fidelity\u{2026}".into());
            }
            let embed_provider = view
                .fidelity_semantic_provider
                .as_deref()
                .map(|p| p as &dyn zeph_llm::LlmProviderDyn);
            let compress_provider = view
                .fidelity_compress_provider
                .as_deref()
                .map(|p| p as &dyn zeph_llm::LlmProviderDyn);
            let fidelity_span = tracing::info_span!(
                "context.fidelity.score",
                message_count = window.messages.len(),
                query_len = query.len(),
            );
            FidelityScorer
                .score_and_apply(
                    window.messages,
                    query,
                    view.planned_next_tools,
                    fidelity_cfg,
                    &*view.token_counter,
                    inserted_count,
                    false, // floor invariant enforced on normal scoring path
                    embed_provider,
                    compress_provider,
                )
                .instrument(fidelity_span)
                .await;
            // Persist fidelity tags so subsequent turns see the floor invariant.
            persist_fidelity_tags(window.messages, view.memory.as_deref()).await;
            recompute_prompt_tokens(window);
            if let Some(ref tx) = view.status_tx {
                let _ = tx.send(String::new());
            }
        }

        Ok(delta)
    }

    /// Apply a [`PreparedContext`] to the message window.
    ///
    /// Injects all fetched messages in insertion order (`doc_rag` → corrections → recall →
    /// cross-session → summaries → persona → trajectory → tree → reasoning), handles
    /// `MemoryFirst` history drain, sanitizes memory content, trims to budget, and injects
    /// the session digest. Returns a [`ContextDelta`] whose `code_context` field the caller
    /// must apply via `inject_code_context`, plus the count of messages freshly inserted at
    /// indices `1..1+inserted_count` (used by the fidelity scorer as the exempt range — INV-10).
    #[allow(clippy::too_many_lines)] // sequential message injection: order matters, cannot split
    async fn apply_prepared_context(
        &self,
        window: &mut MessageWindowView<'_>,
        view: &mut ContextAssemblyView<'_>,
        prepared: zeph_context::assembler::PreparedContext,
    ) -> (ContextDelta, usize) {
        use std::borrow::Cow;
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_sanitizer::{ContentSource, ContentSourceKind, MemorySourceHint};

        // Store top-1 recall score for MAR routing signal.
        *view.last_recall_confidence = prepared.recall_confidence;

        // MemoryFirst: drain conversation history BEFORE inserting memory messages.
        if prepared.memory_first {
            let history_start = 1usize;
            let len = window.messages.len();
            let keep_tail =
                zeph_context::assembler::memory_first_keep_tail(window.messages, history_start);
            if len > history_start + keep_tail {
                window.messages.drain(history_start..len - keep_tail);
                recompute_prompt_tokens(window);
                tracing::debug!(
                    strategy = "memory_first",
                    keep_tail,
                    "dropped conversation history, kept last {keep_tail} messages"
                );
            }
        }

        // Tracks how many memory messages were freshly inserted at positions 1..1+inserted_count
        // so the fidelity scorer can exempt them (INV-10). Incremented at every insertion path.
        let mut inserted_count: usize = 0;

        // Insert memory messages at position 1 (all sanitized before insertion — CRIT-02).
        if let Some(msg) = prepared.graph_facts.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
            tracing::debug!("injected knowledge graph facts into context");
        }
        if let Some(msg) = prepared.doc_rag.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
            tracing::debug!("injected document RAG context");
        }
        if let Some(msg) = prepared.corrections.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ConversationHistory, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
            tracing::debug!("injected past corrections into context");
        }
        if let Some(msg) = prepared.recall.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ConversationHistory, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
        }
        if let Some(msg) = prepared.cross_session.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::LlmSummary, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
        }
        if let Some(msg) = prepared.summaries.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::LlmSummary, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
            tracing::debug!("injected summaries into context");
        }
        if let Some(msg) = prepared.persona_facts.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
            tracing::debug!("injected persona facts into context");
        }
        if let Some(msg) = prepared
            .trajectory_hints
            .filter(|_| window.messages.len() > 1)
        {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
            tracing::debug!("injected trajectory hints into context");
        }
        if let Some(msg) = prepared.tree_memory.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
            tracing::debug!("injected tree memory summary into context");
        }
        if let Some(msg) = prepared
            .reasoning_hints
            .filter(|_| window.messages.len() > 1)
        {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
            tracing::debug!("injected reasoning strategies into context");
        }

        // Code context: sanitize inline, return body to caller via ContextDelta.
        let code_context = if let Some(text) = prepared.code_context {
            let sanitized = view
                .sanitizer
                .sanitize(&text, ContentSource::new(ContentSourceKind::ToolResult));
            view.metrics.sanitizer_runs += 1;
            if !sanitized.injection_flags.is_empty() {
                tracing::warn!(
                    flags = sanitized.injection_flags.len(),
                    "injection patterns detected in code RAG context"
                );
                view.metrics.sanitizer_injection_flags += sanitized.injection_flags.len() as u64;
                let detail = sanitized
                    .injection_flags
                    .first()
                    .map_or_else(String::new, |f| {
                        format!("Detected pattern: {}", f.pattern_name)
                    });
                view.security_events.push(
                    zeph_common::SecurityEventCategory::InjectionFlag,
                    "code_rag",
                    detail,
                );
            }
            if sanitized.was_truncated {
                view.metrics.sanitizer_truncations += 1;
                view.security_events.push(
                    zeph_common::SecurityEventCategory::Truncation,
                    "code_rag",
                    "Content truncated to max_content_size".to_string(),
                );
            }
            Some(sanitized.body)
        } else {
            None
        };

        if !prepared.memory_first {
            self.trim_messages_to_budget(window, prepared.recent_history_budget);
        }

        // Session digest injected AFTER all other memory inserts (closest to system prompt).
        if view.digest_enabled
            && let Some((digest_text, _)) = view
                .cached_session_digest
                .clone()
                .filter(|_| window.messages.len() > 1)
        {
            let digest_msg = Message {
                role: Role::User,
                content: format!("{}{digest_text}", crate::helpers::SESSION_DIGEST_PREFIX),
                parts: vec![],
                metadata: MessageMetadata::default(),
            };
            let sanitized = self
                .sanitize_memory_message(digest_msg, MemorySourceHint::LlmSummary, view)
                .await;
            window.messages.insert(1, sanitized);
            inserted_count += 1;
            tracing::debug!("injected session digest into context");
        }

        // Credential scrubbing pass.
        if view.redact_credentials {
            for msg in &mut *window.messages {
                if msg.role == Role::System {
                    continue;
                }
                if let Cow::Owned(s) = (view.scrub)(&msg.content) {
                    msg.content = s;
                }
            }
        }

        recompute_prompt_tokens(window);

        (ContextDelta { code_context }, inserted_count)
    }

    /// Sanitize a memory retrieval message before inserting it into the context window.
    ///
    /// This is the sole sanitization point for the six memory retrieval paths (`doc_rag`,
    /// corrections, recall, `cross_session`, summaries, `graph_facts`). The `hint` parameter
    /// modulates injection-detection sensitivity — `ConversationHistory` and `LlmSummary`
    /// skip detection to suppress false positives; `ExternalContent` enables full detection.
    ///
    /// Truncation, control-char stripping, delimiter escaping, and spotlighting are active
    /// for all hints (defense-in-depth invariant).
    async fn sanitize_memory_message(
        &self,
        mut msg: zeph_llm::provider::Message,
        hint: zeph_sanitizer::MemorySourceHint,
        view: &mut ContextAssemblyView<'_>,
    ) -> zeph_llm::provider::Message {
        use zeph_sanitizer::{ContentSource, ContentSourceKind};

        let source = ContentSource::new(ContentSourceKind::MemoryRetrieval).with_memory_hint(hint);
        let sanitized = view.sanitizer.sanitize(&msg.content, source);
        view.metrics.sanitizer_runs += 1;
        if !sanitized.injection_flags.is_empty() {
            tracing::warn!(
                flags = sanitized.injection_flags.len(),
                "injection patterns detected in memory retrieval"
            );
            view.metrics.sanitizer_injection_flags += sanitized.injection_flags.len() as u64;
            let detail = sanitized
                .injection_flags
                .first()
                .map_or_else(String::new, |f| {
                    format!("Detected pattern: {}", f.pattern_name)
                });
            view.security_events.push(
                zeph_common::SecurityEventCategory::InjectionFlag,
                "memory_retrieval",
                detail,
            );
        }
        if sanitized.was_truncated {
            view.metrics.sanitizer_truncations += 1;
            view.security_events.push(
                zeph_common::SecurityEventCategory::Truncation,
                "memory_retrieval",
                "Content truncated to max_content_size".to_string(),
            );
        }

        // Quarantine step: route high-risk sources through an isolated LLM (defense-in-depth).
        if view.sanitizer.is_enabled()
            && let Some(qs) = view.quarantine_summarizer
            && qs.should_quarantine(ContentSourceKind::MemoryRetrieval)
        {
            match qs.extract_facts(&sanitized, view.sanitizer).await {
                Ok((facts, flags)) => {
                    view.metrics.quarantine_invocations += 1;
                    view.security_events.push(
                        zeph_common::SecurityEventCategory::Quarantine,
                        "memory_retrieval",
                        "Content quarantined, facts extracted".to_string(),
                    );
                    let escaped = zeph_sanitizer::ContentSanitizer::escape_delimiter_tags(&facts);
                    msg.content = zeph_sanitizer::ContentSanitizer::apply_spotlight(
                        &escaped,
                        &sanitized.source,
                        &flags,
                    );
                    return msg;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "quarantine failed for memory retrieval, using original sanitized content"
                    );
                    view.metrics.quarantine_failures += 1;
                    view.security_events.push(
                        zeph_common::SecurityEventCategory::Quarantine,
                        "memory_retrieval",
                        format!("Quarantine failed: {e}"),
                    );
                }
            }
        }

        msg.content = sanitized.body;
        msg
    }

    /// Reset the conversation history.
    ///
    /// Clears all messages except the system prompt and resets the cached token count.
    /// The caller (`Agent<C>`) is responsible for resetting compaction state, orchestration,
    /// focus, and sidequest state — those fields are outside the context-service scope.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if creating a new conversation in `SQLite` fails.
    pub fn reset_conversation(
        &self,
        window: &mut MessageWindowView<'_>,
        _view: &mut ContextAssemblyView<'_>,
    ) -> Result<(), ContextError> {
        self.clear_history(window);
        Ok(())
    }

    /// Run tiered compaction if the token budget is exhausted.
    ///
    /// Dispatches to the appropriate compaction tier based on the current
    /// context manager state:
    ///
    /// - **None** — context is within budget; no-op.
    /// - **Soft** — apply deferred summaries + prune tool outputs (no LLM).
    /// - **Hard** — Soft steps first, then LLM full summarization if pruning is insufficient.
    ///
    /// Increments the `turns_since_last_hard_compaction` counter unconditionally so pressure
    /// is tracked regardless of whether compaction fires. Respects the cooldown guard: when
    /// cooling, Hard-tier LLM summarization is skipped.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if `SQLite` persistence fails during Hard compaction.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    #[tracing::instrument(name = "agent_context.service.maybe_compact", skip_all, err)]
    pub async fn maybe_compact(
        &self,
        summ: &mut ContextSummarizationView<'_>,
        status: &(impl StatusSink + ?Sized),
    ) -> Result<(), ContextError> {
        use zeph_context::manager::{CompactionState, CompactionTier};

        // Increment turn counter unconditionally (tracks pressure regardless of guards).
        if let Some(count) = summ.context_manager.turns_since_last_hard_compaction_mut() {
            *count += 1;
        }

        // Guard: exhaustion — warn once, then no-op permanently.
        if let CompactionState::Exhausted { warned } = summ.context_manager.compaction_state()
            && !warned
        {
            summ.context_manager
                .set_compaction_state(CompactionState::Exhausted { warned: true });
            tracing::warn!("compaction exhausted: context budget too tight for this session");
        }
        if summ.context_manager.compaction_state().is_exhausted() {
            return Ok(());
        }

        // Guard: server compaction active — skip unless above 95% budget (safety fallback).
        if summ.server_compaction_active {
            let budget = summ
                .context_manager
                .budget
                .as_ref()
                .map_or(0, ContextBudget::max_tokens);
            if budget > 0 {
                let fallback = (budget * 95 / 100) as u64;
                if *summ.cached_prompt_tokens < fallback {
                    return Ok(());
                }
                tracing::warn!(
                    "server compaction active but context at 95%+ — falling back to client-side"
                );
            } else {
                return Ok(());
            }
        }

        // Guard: already compacted this turn.
        if summ
            .context_manager
            .compaction_state()
            .is_compacted_this_turn()
        {
            return Ok(());
        }

        // Decrement cooldown counter; record whether we are in cooldown.
        let in_cooldown = summ.context_manager.compaction_state().cooldown_remaining() > 0;
        if in_cooldown
            && let CompactionState::Cooling { turns_remaining } =
                summ.context_manager.compaction_state()
        {
            let next = turns_remaining - 1;
            summ.context_manager.set_compaction_state(if next == 0 {
                CompactionState::Ready
            } else {
                CompactionState::Cooling {
                    turns_remaining: next,
                }
            });
        }

        // T-07: AgeMem proactive regrade — fires before tier dispatch (INV-06, INV-11).
        // Skip when MemoryFirst is active; ContextSummarizationView does not carry
        // context_strategy, so we check the budget ratio directly via should_proactively_regrade.
        if let Some(ref fidelity_cfg) = summ.fidelity_config.clone()
            && fidelity_cfg.enabled
            && summ.context_manager.should_proactively_regrade(
                *summ.cached_prompt_tokens,
                fidelity_cfg.regrade_threshold,
                summ.server_compaction_active,
            )
        {
            use tracing::Instrument as _;
            let regrade_embed_provider = summ
                .fidelity_semantic_provider
                .as_deref()
                .map(|p| p as &dyn zeph_llm::LlmProviderDyn);
            let regrade_compress_provider = summ
                .fidelity_compress_provider
                .as_deref()
                .map(|p| p as &dyn zeph_llm::LlmProviderDyn);
            FidelityScorer
                .score_and_apply(
                    summ.messages,
                    &summ.current_query,
                    &[],
                    fidelity_cfg,
                    &*summ.token_counter,
                    0,
                    true, // proactive regrade: allow upgrading past the persisted floor
                    regrade_embed_provider,
                    regrade_compress_provider,
                )
                .instrument(tracing::info_span!(
                    "context.fidelity.regrade",
                    budget_ratio = tracing::field::Empty,
                ))
                .await;
            // Persist upgraded fidelity tags so the new levels survive the next turn (F-3).
            persist_fidelity_tags(summ.messages, summ.memory.as_deref()).await;
            recompute_prompt_tokens_summ(summ);
            summ.context_manager.set_regraded_this_turn(true);
            tracing::debug!(
                cached_tokens = *summ.cached_prompt_tokens,
                "AgeMem proactive regrade complete"
            );
        }

        match summ
            .context_manager
            .compaction_tier(*summ.cached_prompt_tokens)
        {
            CompactionTier::Soft => {
                self.do_soft_compaction(summ, status).await;
                Ok(())
            }
            CompactionTier::Hard => self.do_hard_compaction(summ, status, in_cooldown).await,
            _ => Ok(()),
        }
    }

    /// Execute the Soft compaction tier: apply deferred summaries and prune tool outputs.
    ///
    /// Does not trigger an LLM call. Does not set `compacted_this_turn` so Hard tier
    /// may still fire in the same turn if context remains above the hard threshold.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    async fn do_soft_compaction(
        &self,
        summ: &mut ContextSummarizationView<'_>,
        status: &(impl StatusSink + ?Sized),
    ) {
        status.send_status("soft compacting context...").await;

        // Step 0: refresh task goal / subgoal for scored pruning.
        match &summ.context_manager.compression.pruning_strategy {
            zeph_config::PruningStrategy::Subgoal | zeph_config::PruningStrategy::SubgoalMig => {
                crate::summarization::scheduling::maybe_refresh_subgoal(summ);
            }
            _ => crate::summarization::scheduling::maybe_refresh_task_goal(summ),
        }

        // Step 1: apply deferred summaries (free tokens without LLM).
        let applied = crate::summarization::deferred::apply_deferred_summaries(summ);

        // Step 1b: rebuild subgoal index if deferred summaries were applied (S5 fix).
        if applied > 0
            && summ
                .context_manager
                .compression
                .pruning_strategy
                .is_subgoal()
        {
            summ.subgoal_registry
                .rebuild_after_compaction(summ.messages, 0);
        }

        // Step 2: prune tool outputs down to soft threshold.
        let budget = summ
            .context_manager
            .budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        let soft_threshold =
            (budget as f32 * summ.context_manager.soft_compaction_threshold) as usize;
        let cached = usize::try_from(*summ.cached_prompt_tokens).unwrap_or(usize::MAX);
        let min_to_free = cached.saturating_sub(soft_threshold);
        if min_to_free > 0 {
            crate::summarization::pruning::prune_tool_outputs(summ, min_to_free);
        }

        status.send_status("").await;
        tracing::info!(
            cached_tokens = *summ.cached_prompt_tokens,
            soft_threshold,
            "soft compaction complete"
        );
    }

    /// Execute the Hard compaction tier: soft pass first, then LLM summarization if needed.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    async fn do_hard_compaction(
        &self,
        summ: &mut ContextSummarizationView<'_>,
        status: &(impl StatusSink + ?Sized),
        in_cooldown: bool,
    ) -> Result<(), ContextError> {
        use zeph_context::manager::CompactionState;

        // Track hard compaction event for pressure metrics.
        let turns_since_last = summ
            .context_manager
            .turns_since_last_hard_compaction()
            .map(|t| u32::try_from(t).unwrap_or(u32::MAX));
        summ.context_manager
            .set_turns_since_last_hard_compaction(Some(0));
        if let Some(metrics) = summ.metrics {
            metrics.record_hard_compaction(turns_since_last);
        }

        if in_cooldown {
            tracing::debug!(
                turns_remaining = summ.context_manager.compaction_state().cooldown_remaining(),
                "hard compaction skipped: cooldown active"
            );
            return Ok(());
        }

        let budget = summ
            .context_manager
            .budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        let hard_threshold =
            (budget as f32 * summ.context_manager.hard_compaction_threshold) as usize;
        let cached = usize::try_from(*summ.cached_prompt_tokens).unwrap_or(usize::MAX);
        let min_to_free = cached.saturating_sub(hard_threshold);

        status.send_status("compacting context...").await;

        // Step 1: apply deferred summaries.
        crate::summarization::deferred::apply_deferred_summaries(summ);

        // Step 2: attempt pruning-only.
        let freed = crate::summarization::pruning::prune_tool_outputs(summ, min_to_free);
        if freed >= min_to_free {
            tracing::info!(freed, "hard compaction: pruning sufficient");
            summ.context_manager
                .set_compaction_state(CompactionState::CompactedThisTurn {
                    cooldown: summ.context_manager.compaction_cooldown_turns(),
                });
            if let Err(e) = crate::summarization::deferred::flush_deferred_summaries(summ).await {
                tracing::warn!(%e, "flush_deferred_summaries failed after hard compaction");
            }
            status.send_status("").await;
            return Ok(());
        }

        // Step 3: Guard — too few messages to compact.
        let preserve_tail = summ.context_manager.compaction_preserve_tail;
        let compactable = summ.messages.len().saturating_sub(preserve_tail + 1);
        if compactable <= 1 {
            tracing::warn!(
                compactable,
                "hard compaction: too few messages, marking exhausted"
            );
            summ.context_manager
                .set_compaction_state(CompactionState::Exhausted { warned: false });
            status.send_status("").await;
            return Ok(());
        }

        // Step 4: LLM summarization.
        tracing::info!(
            min_to_free,
            "hard compaction: falling back to LLM summarization"
        );
        let tokens_before = *summ.cached_prompt_tokens;
        let outcome = crate::summarization::compaction::compact_context(summ, None).await?;

        let freed_tokens = tokens_before.saturating_sub(*summ.cached_prompt_tokens);

        if !outcome.is_compacted() || freed_tokens == 0 {
            tracing::warn!("hard compaction: no net reduction, marking exhausted");
            summ.context_manager
                .set_compaction_state(CompactionState::Exhausted { warned: false });
            status.send_status("").await;
            return Ok(());
        }

        if matches!(
            summ.context_manager
                .compaction_tier(*summ.cached_prompt_tokens),
            zeph_context::manager::CompactionTier::Hard
        ) {
            tracing::warn!(
                freed_tokens,
                "hard compaction: still above hard threshold after compaction, marking exhausted"
            );
            summ.context_manager
                .set_compaction_state(CompactionState::Exhausted { warned: false });
            status.send_status("").await;
            return Ok(());
        }

        summ.context_manager
            .set_compaction_state(CompactionState::CompactedThisTurn {
                cooldown: summ.context_manager.compaction_cooldown_turns(),
            });

        if tokens_before > *summ.cached_prompt_tokens {
            tracing::info!(
                tokens_before,
                tokens_after = *summ.cached_prompt_tokens,
                saved = freed_tokens,
                "context compaction complete"
            );
        }

        status.send_status("").await;
        Ok(())
    }

    /// Summarize the most recent tool-use/result pair if it exceeds the cutoff.
    ///
    /// Drains the backlog of unsummarized tool-use/result pairs in a single pass,
    /// storing results as `deferred_summary` on message metadata. Applied lazily
    /// by [`Self::maybe_apply_deferred_summaries`] when context pressure rises.
    #[tracing::instrument(name = "agent_context.service.maybe_summarize_tool_pair", skip_all)]
    pub async fn maybe_summarize_tool_pair(
        &self,
        summ: &mut ContextSummarizationView<'_>,
        providers: &ProviderHandles,
    ) {
        crate::summarization::deferred::maybe_summarize_tool_pair(
            summ,
            providers,
            &TxStatusSink(summ.status_tx.clone()),
        )
        .await;
    }

    /// Apply any deferred tool-pair summaries to the message window.
    ///
    /// Processes all pending deferred summaries in reverse order so insertions do not
    /// invalidate lower indices. Returns the number of summaries applied.
    #[must_use]
    pub fn apply_deferred_summaries(&self, summ: &mut ContextSummarizationView<'_>) -> usize {
        crate::summarization::deferred::apply_deferred_summaries(summ)
    }

    /// Flush all deferred summary IDs to the database.
    ///
    /// Calls `apply_tool_pair_summaries` to soft-delete the original tool pairs and
    /// persist the summaries. Always clears both deferred queues regardless of outcome.
    #[tracing::instrument(name = "agent_context.service.flush_deferred_summaries", skip_all)]
    pub async fn flush_deferred_summaries(&self, summ: &mut ContextSummarizationView<'_>) {
        if let Err(e) = crate::summarization::deferred::flush_deferred_summaries(summ).await {
            tracing::warn!(%e, "flush_deferred_summaries failed");
        }
    }

    /// Apply deferred summaries if context usage exceeds the soft compaction threshold.
    ///
    /// Two triggers: token pressure (above the soft threshold) and count pressure (pending
    /// summaries >= `tool_call_cutoff`). This is Tier 0 — no LLM call. Does NOT set
    /// `compacted_this_turn` so proactive/reactive compaction may still fire.
    pub fn maybe_apply_deferred_summaries(&self, summ: &mut ContextSummarizationView<'_>) {
        crate::summarization::deferred::maybe_apply_deferred_summaries(summ);
    }

    /// Run unconditional LLM-based context compaction with an optional token budget.
    ///
    /// Bypasses tier and cooldown checks — always drains the oldest messages and inserts
    /// a compact summary. Use this in tests or when the caller has already determined that
    /// compaction is warranted. Production code should prefer [`Self::maybe_compact`].
    ///
    /// Invokes the optional callbacks wired into `summ` in this order:
    /// archive → LLM summarization → probe → finalize → persistence.
    ///
    /// Returns [`crate::state::CompactionOutcome::NoChange`] when there is nothing to compact.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError`] if summarization fails (LLM error or timeout).
    #[tracing::instrument(name = "agent_context.service.compact_context", skip_all, err)]
    pub async fn compact_context(
        &self,
        summ: &mut ContextSummarizationView<'_>,
        max_summary_tokens: Option<usize>,
    ) -> Result<crate::state::CompactionOutcome, crate::error::ContextError> {
        crate::summarization::compaction::compact_context(summ, max_summary_tokens).await
    }

    /// Apply a soft compaction pass mid-iteration if required.
    ///
    /// Applies deferred summaries and prunes tool outputs down to the soft threshold.
    /// Never triggers a Hard tier LLM call. Returns immediately if `compacted_this_turn`
    /// is set or context is below the soft threshold.
    pub fn maybe_soft_compact_mid_iteration(&self, summ: &mut ContextSummarizationView<'_>) {
        crate::summarization::scheduling::maybe_soft_compact_mid_iteration(summ);
    }

    /// Run proactive compression if token usage crosses the configured threshold.
    ///
    /// Uses the `compact_context_with_budget` path (LLM summarization with an optional
    /// token cap). Skips when server compaction is active unless context exceeds 95% of
    /// the budget. Does not impose a post-compaction cooldown.
    #[tracing::instrument(name = "agent_context.service.maybe_proactive_compress", skip_all)]
    pub async fn maybe_proactive_compress(
        &self,
        summ: &mut ContextSummarizationView<'_>,
        status: &(impl StatusSink + ?Sized),
    ) {
        let Some((_threshold, max_summary_tokens)) = summ
            .context_manager
            .should_proactively_compress(*summ.cached_prompt_tokens)
        else {
            return;
        };

        if summ.server_compaction_active {
            let budget = summ
                .context_manager
                .budget
                .as_ref()
                .map_or(0, ContextBudget::max_tokens);
            if budget > 0 {
                let fallback = (budget * 95 / 100) as u64;
                if *summ.cached_prompt_tokens <= fallback {
                    return;
                }
                tracing::warn!(
                    cached_prompt_tokens = *summ.cached_prompt_tokens,
                    fallback_threshold = fallback,
                    "server compaction active but context at 95%+ — falling back to proactive"
                );
            } else {
                return;
            }
        }

        status.send_status("compressing context...").await;
        tracing::info!(
            max_summary_tokens,
            cached_tokens = *summ.cached_prompt_tokens,
            "proactive compression triggered"
        );

        match crate::summarization::compaction::compact_context(summ, Some(max_summary_tokens))
            .await
        {
            Ok(outcome) if outcome.is_compacted() => {
                summ.context_manager.set_compaction_state(
                    zeph_context::manager::CompactionState::CompactedThisTurn { cooldown: 0 },
                );
                tracing::info!("proactive compression complete");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(%e, "proactive compression failed"),
        }

        status.send_status("").await;
    }

    /// Refresh the task goal when the last user message has changed.
    ///
    /// Two-phase non-blocking: applies any completed background result from the previous
    /// turn, then schedules a new extraction if the user message hash has changed.
    /// Only active for `TaskAware` and `Mig` pruning strategies.
    pub fn maybe_refresh_task_goal(&self, summ: &mut ContextSummarizationView<'_>) {
        crate::summarization::scheduling::maybe_refresh_task_goal(summ);
    }

    /// Refresh the subgoal registry when the last user message has changed.
    ///
    /// Mirrors the two-phase `maybe_refresh_task_goal` pattern.
    /// Only active for `Subgoal` and `SubgoalMig` pruning strategies.
    pub fn maybe_refresh_subgoal(&self, summ: &mut ContextSummarizationView<'_>) {
        crate::summarization::scheduling::maybe_refresh_subgoal(summ);
    }
}

// ── StatusSink adapters ───────────────────────────────────────────────────────

/// `StatusSink` adapter over an optional `UnboundedSender<String>`.
///
/// Sends status strings when the sender is present; silently drops them otherwise.
struct TxStatusSink(Option<tokio::sync::mpsc::UnboundedSender<String>>);

impl StatusSink for TxStatusSink {
    fn send_status(&self, msg: &str) -> impl std::future::Future<Output = ()> + Send + '_ {
        if let Some(ref tx) = self.0 {
            let _ = tx.send(msg.to_owned());
        }
        std::future::ready(())
    }
}

// ── Free functions (helpers shared across service methods) ────────────────────

/// Recompute `cached_prompt_tokens` from the current message list.
///
/// Called after every mutation that changes the message count or content, so the
/// provider call path always sees an accurate token count.
pub(crate) fn recompute_prompt_tokens(window: &mut MessageWindowView<'_>) {
    *window.cached_prompt_tokens = window
        .messages
        .iter()
        .map(|m| window.token_counter.count_message_tokens(m) as u64)
        .sum();
}

/// Persist fidelity tags for all scored messages to `SQLite`.
///
/// Collects `(db_id, tag as u8)` pairs for messages that have both a `db_id` and a
/// non-None `fidelity_tag`, then calls [`SqliteStore::update_fidelity_tags`] inline.
/// The await is cheap — `SQLite` UPDATE is a sub-millisecond local I/O operation.
///
/// A warn-level log is emitted on failure; the next turn will recompute from scratch,
/// which is safe (the floor invariant simply won't apply until persistence succeeds).
async fn persist_fidelity_tags(
    messages: &[zeph_llm::provider::Message],
    memory: Option<&zeph_memory::semantic::SemanticMemory>,
) {
    let Some(mem) = memory else { return };
    let updates: Vec<(zeph_memory::MessageId, u8)> = messages
        .iter()
        .filter_map(|m| {
            let db_id = m.metadata.db_id?;
            let tag = m.metadata.fidelity_tag?;
            Some((zeph_memory::MessageId(db_id), tag as u8))
        })
        .collect();
    if updates.is_empty() {
        return;
    }
    if let Err(e) = mem.sqlite().update_fidelity_tags(&updates).await {
        tracing::warn!(
            count = updates.len(),
            error = %e,
            "failed to persist fidelity tags; floor invariant will not apply next turn"
        );
    }
}

/// Recompute `cached_prompt_tokens` for a [`ContextSummarizationView`].
///
/// Used after the `AgeMem` proactive regrade modifies the message window in `maybe_compact`.
fn recompute_prompt_tokens_summ(summ: &mut crate::state::ContextSummarizationView<'_>) {
    *summ.cached_prompt_tokens = summ
        .messages
        .iter()
        .map(|m| summ.token_counter.count_message_tokens(m) as u64)
        .sum();
}

/// Remove all system/user messages whose `content` starts with `prefix` and whose
/// role matches `role`.
///
/// Operates on the raw `messages` slice to allow callers that don't hold a full
/// `MessageWindowView` to use this helper (e.g., from `zeph-core` shims).
pub(crate) fn remove_by_prefix(
    messages: &mut Vec<zeph_llm::provider::Message>,
    role: Role,
    prefix: &str,
) {
    messages.retain(|m| m.role != role || !m.content.starts_with(prefix));
}

/// Remove messages that match either a typed `MessagePart` or a content prefix.
///
/// For `Role::System` messages: typed-part matching takes priority — a message is removed
/// if its **first** part satisfies `part_matches`. As a fallback, messages that start with
/// `prefix` are also removed.
/// For `Role::User` messages: removed if their content starts with `prefix` (tiered-recall
/// cleanup).
/// All other roles are always retained.
pub(crate) fn remove_by_part_or_prefix(
    messages: &mut Vec<zeph_llm::provider::Message>,
    prefix: &str,
    part_matches: impl Fn(&MessagePart) -> bool,
) {
    messages.retain(|m| {
        // Role::User recall messages are produced by the tiered-retrieval path in
        // inject_semantic_recall. They must be cleaned up the same way as Role::System ones.
        if m.role == Role::User {
            return !m.content.starts_with(prefix);
        }
        if m.role != Role::System {
            return true;
        }
        if m.parts.first().is_some_and(&part_matches) {
            return false;
        }
        !m.content.starts_with(prefix)
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use zeph_llm::provider::{Message, MessagePart, Role};
    use zeph_memory::TokenCounter;

    use super::*;
    use crate::helpers::{GRAPH_FACTS_PREFIX, RECALL_PREFIX, SUMMARY_PREFIX};
    use crate::state::MessageWindowView;

    fn make_counter() -> Arc<TokenCounter> {
        Arc::new(TokenCounter::default())
    }

    fn make_window<'a>(
        messages: &'a mut Vec<Message>,
        cached: &'a mut u64,
        completed: &'a mut HashSet<String>,
    ) -> MessageWindowView<'a> {
        let last = Box::leak(Box::new(None::<i64>));
        let deferred_hide = Box::leak(Box::new(Vec::<i64>::new()));
        let deferred_summ = Box::leak(Box::new(Vec::<String>::new()));
        MessageWindowView {
            messages,
            last_persisted_message_id: last,
            deferred_db_hide_ids: deferred_hide,
            deferred_db_summaries: deferred_summ,
            cached_prompt_tokens: cached,
            token_counter: make_counter(),
            completed_tool_ids: completed,
        }
    }

    fn sys(text: &str) -> Message {
        Message::from_legacy(Role::System, text)
    }

    fn user(text: &str) -> Message {
        Message::from_legacy(Role::User, text)
    }

    fn assistant(text: &str) -> Message {
        Message::from_legacy(Role::Assistant, text)
    }

    #[test]
    fn clear_history_keeps_system_prompt() {
        let mut msgs = vec![sys("system"), user("hello"), assistant("hi")];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        completed.insert("tool_1".to_owned());
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().clear_history(&mut window);

        assert_eq!(window.messages.len(), 1);
        assert_eq!(window.messages[0].content, "system");
        assert!(
            window.completed_tool_ids.is_empty(),
            "completed_tool_ids must be cleared"
        );
    }

    #[test]
    fn clear_history_empty_messages_is_noop() {
        let mut msgs: Vec<Message> = vec![];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().clear_history(&mut window);

        assert!(window.messages.is_empty());
    }

    #[test]
    fn remove_recall_messages_removes_by_prefix() {
        let mut msgs = vec![
            sys("system"),
            sys(&format!("{RECALL_PREFIX}some recalled text")),
            user("hello"),
        ];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().remove_recall_messages(&mut window);

        assert_eq!(window.messages.len(), 2);
        assert!(
            window
                .messages
                .iter()
                .all(|m| !m.content.starts_with(RECALL_PREFIX))
        );
    }

    // Regression test for #4019: Role::User recall messages must be removed by
    // remove_recall_messages, not just Role::System ones.
    #[test]
    fn remove_recall_messages_removes_user_role_recall() {
        let mut msgs = vec![
            sys("system"),
            user(&format!("{RECALL_PREFIX}recalled via tiered path")),
            user("real user message"),
        ];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().remove_recall_messages(&mut window);

        assert_eq!(
            window.messages.len(),
            2,
            "Role::User recall message must be removed"
        );
        assert!(
            window
                .messages
                .iter()
                .all(|m| !m.content.starts_with(RECALL_PREFIX)),
            "no message with RECALL_PREFIX must remain"
        );
        assert!(
            window
                .messages
                .iter()
                .any(|m| m.content == "real user message"),
            "non-recall user message must survive"
        );
    }

    #[test]
    fn remove_graph_facts_messages_removes_matching() {
        let mut msgs = vec![
            sys("system"),
            sys(&format!("{GRAPH_FACTS_PREFIX}fact1")),
            user("hello"),
        ];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().remove_graph_facts_messages(&mut window);

        assert_eq!(window.messages.len(), 2);
    }

    #[test]
    fn remove_summary_messages_removes_by_part() {
        let mut msgs = vec![
            sys("system"),
            Message::from_parts(
                Role::System,
                vec![MessagePart::Summary {
                    text: format!("{SUMMARY_PREFIX}old summary"),
                }],
            ),
            user("hello"),
        ];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().remove_summary_messages(&mut window);

        assert_eq!(window.messages.len(), 2);
    }

    #[test]
    fn trim_messages_to_budget_zero_is_noop() {
        let mut msgs = vec![sys("system"), user("a"), assistant("b"), user("c")];
        let original_len = msgs.len();
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        ContextService::new().trim_messages_to_budget(&mut window, 0);

        assert_eq!(window.messages.len(), original_len);
    }

    #[test]
    fn trim_messages_to_budget_keeps_recent() {
        // With a very small budget only the most recent messages survive.
        let mut msgs = vec![
            sys("system"),
            user("message 1"),
            assistant("reply 1"),
            user("message 2"),
        ];
        let mut cached = 0u64;
        let mut completed = HashSet::new();
        let mut window = make_window(&mut msgs, &mut cached, &mut completed);

        // 1-token budget keeps the last user message only.
        ContextService::new().trim_messages_to_budget(&mut window, 1);

        // System prompt is always kept; at least one recent message should be present.
        assert!(
            window.messages.len() < 4,
            "trim should remove some messages"
        );
        assert_eq!(
            window.messages[0].role,
            Role::System,
            "system prompt must survive trim"
        );
    }

    // AC-12: inserted_count must equal the number of non-None memory fields injected.
    // Tests that every Some(msg) field in PreparedContext increments inserted_count by 1.
    mod inserted_count_tests {
        use parking_lot::RwLock;
        use std::borrow::Cow;
        use std::collections::HashSet;
        use std::sync::Arc;

        use zeph_common::SecurityEventCategory;
        use zeph_config::memory::TieredRetrievalConfig;
        use zeph_config::{
            ContextFormat, ContextStrategy, DocumentConfig, GraphConfig, PersonaConfig,
            ReasoningConfig, TrajectoryConfig, TreeConfig,
        };
        use zeph_context::assembler::PreparedContext;
        use zeph_context::manager::ContextManager;
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::TokenCounter;
        use zeph_sanitizer::ContentIsolationConfig;
        use zeph_sanitizer::ContentSanitizer;
        use zeph_skills::registry::SkillRegistry;

        use super::super::*;
        use crate::state::{
            ContextAssemblyView, MessageWindowView, MetricsCounters, SecurityEventSink,
        };

        fn make_task_supervisor() -> Arc<zeph_common::TaskSupervisor> {
            Arc::new(zeph_common::TaskSupervisor::new(
                tokio_util::sync::CancellationToken::new(),
            ))
        }

        struct NoopSink;
        impl SecurityEventSink for NoopSink {
            fn push(&mut self, _: SecurityEventCategory, _: &'static str, _: String) {}
        }

        fn make_counter() -> Arc<TokenCounter> {
            Arc::new(TokenCounter::default())
        }

        fn make_window<'a>(
            messages: &'a mut Vec<Message>,
            cached: &'a mut u64,
            completed: &'a mut HashSet<String>,
        ) -> MessageWindowView<'a> {
            let last = Box::leak(Box::new(None::<i64>));
            let deferred_hide = Box::leak(Box::new(Vec::<i64>::new()));
            let deferred_summ = Box::leak(Box::new(Vec::<String>::new()));
            MessageWindowView {
                messages,
                last_persisted_message_id: last,
                deferred_db_hide_ids: deferred_hide,
                deferred_db_summaries: deferred_summ,
                cached_prompt_tokens: cached,
                token_counter: make_counter(),
                completed_tool_ids: completed,
            }
        }

        fn mem_msg(content: &str) -> Message {
            Message {
                role: Role::User,
                content: content.to_string(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            }
        }

        fn scrub_noop(s: &str) -> Cow<'_, str> {
            Cow::Borrowed(s)
        }

        #[tokio::test]
        async fn inserted_count_incremented_for_all_paths() {
            // AC-12: each non-None field in PreparedContext increments inserted_count by 1.
            // 10 memory fields are tested here (session_digest is controlled by digest_enabled).
            let mut msgs = vec![
                Message::from_legacy(Role::System, "system"),
                Message::from_legacy(Role::User, "user turn"),
            ];
            let mut cached = 0u64;
            let mut completed = HashSet::new();
            let mut window = make_window(&mut msgs, &mut cached, &mut completed);

            let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
            let mut ctx_mgr = ContextManager::new();
            let mut sink = NoopSink;
            let mut last_confidence = None::<f32>;
            let mut last_skills_prompt = String::new();
            let mut active_skill_names = Vec::new();
            let registry = Arc::new(RwLock::new(SkillRegistry::default()));

            let mut view = ContextAssemblyView {
                memory: None,
                conversation_id: None,
                recall_limit: 10,
                cross_session_score_threshold: 0.5,
                context_format: ContextFormat::default(),
                last_recall_confidence: &mut last_confidence,
                context_strategy: ContextStrategy::default(),
                crossover_turn_threshold: 0,
                cached_session_digest: None,
                digest_enabled: false, // no session digest injection in this test
                graph_config: GraphConfig::default(),
                document_config: DocumentConfig::default(),
                persona_config: PersonaConfig::default(),
                trajectory_config: TrajectoryConfig::default(),
                reasoning_config: ReasoningConfig::default(),
                memcot_config: zeph_config::MemCotConfig::default(),
                memcot_state: None,
                tree_config: TreeConfig::default(),
                last_skills_prompt: &mut last_skills_prompt,
                active_skill_names: &mut active_skill_names,
                skill_registry: registry,
                skill_paths: &[],
                correction_config: None,
                sidequest_turn_counter: 0,
                proactive_explorer: None,
                sanitizer: &sanitizer,
                quarantine_summarizer: None,
                context_manager: &mut ctx_mgr,
                token_counter: make_counter(),
                metrics: MetricsCounters::default(),
                security_events: &mut sink,
                cached_prompt_tokens: 0,
                redact_credentials: false,
                channel_skills: &[],
                scrub: scrub_noop,
                tiered_retrieval_config: TieredRetrievalConfig {
                    enabled: false,
                    ..TieredRetrievalConfig::default()
                },
                tiered_retrieval_classifier: None,
                tiered_retrieval_validator: None,
                fidelity_config: None,
                fidelity_semantic_provider: None,
                fidelity_compress_provider: None,
                planned_next_tools: &[],
                status_tx: None,
                task_supervisor: make_task_supervisor(),
            };

            // Populate all 10 message-carrying fields.
            let prepared = PreparedContext {
                graph_facts: Some(mem_msg("graph_facts")),
                doc_rag: Some(mem_msg("doc_rag")),
                corrections: Some(mem_msg("corrections")),
                recall: Some(mem_msg("recall")),
                recall_confidence: Some(0.9),
                cross_session: Some(mem_msg("cross_session")),
                summaries: Some(mem_msg("summaries")),
                code_context: None, // code_context returns via ContextDelta, not inserted_count
                persona_facts: Some(mem_msg("persona_facts")),
                trajectory_hints: Some(mem_msg("trajectory_hints")),
                tree_memory: Some(mem_msg("tree_memory")),
                reasoning_hints: Some(mem_msg("reasoning_hints")),
                memory_first: false,
                recent_history_budget: 100_000,
                background_tasks: vec![],
            };

            let (_delta, inserted_count) = ContextService::new()
                .apply_prepared_context(&mut window, &mut view, prepared)
                .await;

            // 10 message fields were Some(msg): graph_facts, doc_rag, corrections, recall,
            // cross_session, summaries, persona_facts, trajectory_hints, tree_memory, reasoning_hints.
            assert_eq!(
                inserted_count, 10,
                "all 10 message-carrying PreparedContext fields must increment inserted_count"
            );
        }
    }

    mod inject_semantic_recall_tests {
        use parking_lot::RwLock;
        use std::borrow::Cow;
        use std::collections::HashSet;
        use std::sync::Arc;

        use zeph_config::memory::TieredRetrievalConfig;
        use zeph_config::{
            ContextFormat, ContextStrategy, DocumentConfig, GraphConfig, PersonaConfig,
            ReasoningConfig, TrajectoryConfig, TreeConfig,
        };
        use zeph_context::manager::ContextManager;
        use zeph_llm::provider::Message;
        use zeph_memory::TokenCounter;
        use zeph_sanitizer::ContentIsolationConfig;
        use zeph_sanitizer::ContentSanitizer;
        use zeph_skills::registry::SkillRegistry;

        use zeph_common::SecurityEventCategory;

        use super::super::*;
        use crate::helpers::RECALL_PREFIX;
        use crate::state::{
            ContextAssemblyView, MessageWindowView, MetricsCounters, SecurityEventSink,
        };

        fn make_task_supervisor() -> Arc<zeph_common::TaskSupervisor> {
            Arc::new(zeph_common::TaskSupervisor::new(
                tokio_util::sync::CancellationToken::new(),
            ))
        }

        struct NoopSink;
        impl SecurityEventSink for NoopSink {
            fn push(&mut self, _: SecurityEventCategory, _: &'static str, _: String) {}
        }

        fn make_counter() -> Arc<TokenCounter> {
            Arc::new(TokenCounter::default())
        }

        fn make_window<'a>(
            messages: &'a mut Vec<Message>,
            cached: &'a mut u64,
            completed: &'a mut HashSet<String>,
        ) -> MessageWindowView<'a> {
            let last = Box::leak(Box::new(None::<i64>));
            let deferred_hide = Box::leak(Box::new(Vec::<i64>::new()));
            let deferred_summ = Box::leak(Box::new(Vec::<String>::new()));
            MessageWindowView {
                messages,
                last_persisted_message_id: last,
                deferred_db_hide_ids: deferred_hide,
                deferred_db_summaries: deferred_summ,
                cached_prompt_tokens: cached,
                token_counter: make_counter(),
                completed_tool_ids: completed,
            }
        }

        fn scrub_noop(s: &str) -> Cow<'_, str> {
            Cow::Borrowed(s)
        }

        #[tokio::test]
        async fn tiered_recall_disabled_uses_flat_path() {
            // With tiered_retrieval disabled and no memory, inject_semantic_recall must
            // return Ok(()) without inserting any recall message (flat path returns empty).
            let mut msgs: Vec<Message> = vec![];
            let mut cached = 0u64;
            let mut completed = HashSet::new();
            let mut window = make_window(&mut msgs, &mut cached, &mut completed);

            let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
            let mut ctx_mgr = ContextManager::new();
            let mut sink = NoopSink;
            let mut last_confidence = None::<f32>;
            let mut last_skills_prompt = String::new();
            let mut active_skill_names = Vec::new();
            let registry = Arc::new(RwLock::new(SkillRegistry::default()));

            let view = ContextAssemblyView {
                memory: None,
                conversation_id: None,
                recall_limit: 10,
                cross_session_score_threshold: 0.5,
                context_format: ContextFormat::default(),
                last_recall_confidence: &mut last_confidence,
                context_strategy: ContextStrategy::default(),
                crossover_turn_threshold: 0,
                cached_session_digest: None,
                digest_enabled: false,
                graph_config: GraphConfig::default(),
                document_config: DocumentConfig::default(),
                persona_config: PersonaConfig::default(),
                trajectory_config: TrajectoryConfig::default(),
                reasoning_config: ReasoningConfig::default(),
                memcot_config: zeph_config::MemCotConfig::default(),
                memcot_state: None,
                tree_config: TreeConfig::default(),
                last_skills_prompt: &mut last_skills_prompt,
                active_skill_names: &mut active_skill_names,
                skill_registry: registry,
                skill_paths: &[],
                correction_config: None,
                sidequest_turn_counter: 0,
                proactive_explorer: None,
                sanitizer: &sanitizer,
                quarantine_summarizer: None,
                context_manager: &mut ctx_mgr,
                token_counter: make_counter(),
                metrics: MetricsCounters::default(),
                security_events: &mut sink,
                cached_prompt_tokens: 0,
                redact_credentials: false,
                channel_skills: &[],
                scrub: scrub_noop,
                tiered_retrieval_config: TieredRetrievalConfig {
                    enabled: false,
                    ..TieredRetrievalConfig::default()
                },
                tiered_retrieval_classifier: None,
                tiered_retrieval_validator: None,
                fidelity_config: None,
                fidelity_semantic_provider: None,
                fidelity_compress_provider: None,
                planned_next_tools: &[],
                status_tx: None,
                task_supervisor: make_task_supervisor(),
            };

            let result = ContextService::new()
                .inject_semantic_recall("test query", 1000, &mut window, &view)
                .await;

            assert!(result.is_ok(), "disabled tiered recall must return Ok(())");
            assert!(
                window
                    .messages
                    .iter()
                    .all(|m| !m.content.starts_with(RECALL_PREFIX)),
                "no recall message must be injected when memory is None"
            );
        }

        #[tokio::test]
        async fn tiered_recall_enabled_no_memory_returns_ok() {
            // With tiered_retrieval enabled but memory = None, inject_semantic_recall must
            // return Ok(()) via the early-return guard without inserting any recall message.
            let mut msgs: Vec<Message> = vec![];
            let mut cached = 0u64;
            let mut completed = HashSet::new();
            let mut window = make_window(&mut msgs, &mut cached, &mut completed);

            let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
            let mut ctx_mgr = ContextManager::new();
            let mut sink = NoopSink;
            let mut last_confidence = None::<f32>;
            let mut last_skills_prompt = String::new();
            let mut active_skill_names = Vec::new();
            let registry = Arc::new(RwLock::new(SkillRegistry::default()));

            let view = ContextAssemblyView {
                memory: None,
                conversation_id: None,
                recall_limit: 10,
                cross_session_score_threshold: 0.5,
                context_format: ContextFormat::default(),
                last_recall_confidence: &mut last_confidence,
                context_strategy: ContextStrategy::default(),
                crossover_turn_threshold: 0,
                cached_session_digest: None,
                digest_enabled: false,
                graph_config: GraphConfig::default(),
                document_config: DocumentConfig::default(),
                persona_config: PersonaConfig::default(),
                trajectory_config: TrajectoryConfig::default(),
                reasoning_config: ReasoningConfig::default(),
                memcot_config: zeph_config::MemCotConfig::default(),
                memcot_state: None,
                tree_config: TreeConfig::default(),
                last_skills_prompt: &mut last_skills_prompt,
                active_skill_names: &mut active_skill_names,
                skill_registry: registry,
                skill_paths: &[],
                correction_config: None,
                sidequest_turn_counter: 0,
                proactive_explorer: None,
                sanitizer: &sanitizer,
                quarantine_summarizer: None,
                context_manager: &mut ctx_mgr,
                token_counter: make_counter(),
                metrics: MetricsCounters::default(),
                security_events: &mut sink,
                cached_prompt_tokens: 0,
                redact_credentials: false,
                channel_skills: &[],
                scrub: scrub_noop,
                tiered_retrieval_config: TieredRetrievalConfig {
                    enabled: true,
                    ..TieredRetrievalConfig::default()
                },
                tiered_retrieval_classifier: None,
                tiered_retrieval_validator: None,
                fidelity_config: None,
                fidelity_semantic_provider: None,
                fidelity_compress_provider: None,
                planned_next_tools: &[],
                status_tx: None,
                task_supervisor: make_task_supervisor(),
            };

            let result = ContextService::new()
                .inject_semantic_recall("test query", 1000, &mut window, &view)
                .await;

            assert!(
                result.is_ok(),
                "enabled tiered recall with no memory must return Ok(())"
            );
            assert!(
                window.messages.is_empty(),
                "no recall message must be injected when memory is None"
            );
        }

        // Regression test for #3996: prepare_context must call inject_semantic_recall when
        // tiered_retrieval.enabled = true. When context_manager.budget is None the function
        // returns early with Ok(ContextDelta::default()); this test verifies that early-return
        // path compiles and does not panic with the new conditional blocks in place.
        #[tokio::test]
        async fn prepare_context_tiered_enabled_no_budget_returns_default() {
            let mut msgs: Vec<zeph_llm::provider::Message> = vec![];
            let mut cached = 0u64;
            let mut completed = HashSet::new();
            let mut window = make_window(&mut msgs, &mut cached, &mut completed);

            let sanitizer = zeph_sanitizer::ContentSanitizer::new(
                &zeph_sanitizer::ContentIsolationConfig::default(),
            );
            let mut ctx_mgr = zeph_context::manager::ContextManager::new();
            // budget = None → prepare_context returns Ok(ContextDelta::default()) immediately.
            assert!(ctx_mgr.budget.is_none());

            let mut sink = NoopSink;
            let mut last_confidence = None::<f32>;
            let mut last_skills_prompt = String::new();
            let mut active_skill_names = Vec::new();
            let registry = Arc::new(RwLock::new(zeph_skills::registry::SkillRegistry::default()));

            let mut view = ContextAssemblyView {
                memory: None,
                conversation_id: None,
                recall_limit: 10,
                cross_session_score_threshold: 0.5,
                context_format: ContextFormat::default(),
                last_recall_confidence: &mut last_confidence,
                context_strategy: ContextStrategy::default(),
                crossover_turn_threshold: 0,
                cached_session_digest: None,
                digest_enabled: false,
                graph_config: GraphConfig::default(),
                document_config: DocumentConfig::default(),
                persona_config: PersonaConfig::default(),
                trajectory_config: TrajectoryConfig::default(),
                reasoning_config: ReasoningConfig::default(),
                memcot_config: zeph_config::MemCotConfig::default(),
                memcot_state: None,
                tree_config: TreeConfig::default(),
                last_skills_prompt: &mut last_skills_prompt,
                active_skill_names: &mut active_skill_names,
                skill_registry: registry,
                skill_paths: &[],
                correction_config: None,
                sidequest_turn_counter: 0,
                proactive_explorer: None,
                sanitizer: &sanitizer,
                quarantine_summarizer: None,
                context_manager: &mut ctx_mgr,
                token_counter: make_counter(),
                metrics: MetricsCounters::default(),
                security_events: &mut sink,
                cached_prompt_tokens: 0,
                redact_credentials: false,
                channel_skills: &[],
                scrub: scrub_noop,
                tiered_retrieval_config: TieredRetrievalConfig {
                    enabled: true,
                    ..TieredRetrievalConfig::default()
                },
                tiered_retrieval_classifier: None,
                tiered_retrieval_validator: None,
                fidelity_config: None,
                fidelity_semantic_provider: None,
                fidelity_compress_provider: None,
                planned_next_tools: &[],
                status_tx: None,
                task_supervisor: make_task_supervisor(),
            };

            let result = ContextService::new()
                .prepare_context("test query", &mut window, &mut view)
                .await;

            assert!(
                result.is_ok(),
                "prepare_context with tiered enabled and no budget must return Ok"
            );
        }

        // Regression test for #4022: inject_semantic_recall_bare must be callable without a
        // full ContextAssemblyView and must return Ok(()) when memory is None.
        #[tokio::test]
        async fn inject_semantic_recall_bare_no_memory_returns_ok() {
            use zeph_config::memory::TieredRetrievalConfig;

            let mut msgs: Vec<Message> = vec![];
            let mut cached = 0u64;
            let mut completed = HashSet::new();
            let mut window = make_window(&mut msgs, &mut cached, &mut completed);

            let tiered_config = TieredRetrievalConfig {
                enabled: true,
                ..TieredRetrievalConfig::default()
            };
            let params = SemanticRecallParams {
                query: "test query",
                token_budget: 1000,
                recall_limit: 10,
                context_format: zeph_config::ContextFormat::default(),
                conversation_id: None,
                tiered_classifier: None,
                tiered_validator: None,
                tiered_config: &tiered_config,
            };
            let result = ContextService::new()
                .inject_semantic_recall_bare(params, &mut window, None)
                .await;

            assert!(
                result.is_ok(),
                "inject_semantic_recall_bare with memory=None must return Ok(())"
            );
            assert!(
                window.messages.is_empty(),
                "no recall message must be injected when memory is None"
            );
        }
    }
}
