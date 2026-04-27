// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ContextService`] — stateless façade for agent context-assembly operations.

use zeph_context::budget::ContextBudget;
use zeph_llm::LlmProvider;
use zeph_llm::provider::{MessagePart, Role};

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
    pub async fn inject_semantic_recall(
        &self,
        query: &str,
        token_budget: usize,
        window: &mut MessageWindowView<'_>,
        view: &ContextAssemblyView<'_>,
    ) -> Result<(), ContextError> {
        self.remove_recall_messages(window);

        let (msg, _score) = crate::helpers::fetch_semantic_recall_raw(
            view.memory.as_deref(),
            view.recall_limit,
            view.context_format,
            query,
            token_budget,
            &view.token_counter,
            None,
        )
        .await?;

        if let Some(msg) = msg
            && window.messages.len() > 1
        {
            window.messages.insert(1, msg);
        }

        Ok(())
    }

    /// Inject cross-session context messages into the window for the given query.
    ///
    /// Removes any existing cross-session messages first, fetches fresh cross-session
    /// context for the current conversation, and inserts the result at position 1.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if the memory backend returns an error.
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
            .primary
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
    pub async fn prepare_context(
        &self,
        query: &str,
        window: &mut MessageWindowView<'_>,
        view: &mut ContextAssemblyView<'_>,
        _providers: &ProviderHandles,
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

        let memory_view = zeph_context::input::ContextMemoryView {
            memory: view.memory.clone(),
            conversation_id: view.conversation_id,
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
            tree_config: view.tree_config.clone(),
        };

        #[cfg(feature = "index")]
        let index_access = view.index;
        #[cfg(not(feature = "index"))]
        let index_access: Option<&dyn zeph_context::input::IndexAccess> = None;

        let input = zeph_context::input::ContextAssemblyInput {
            memory: &memory_view,
            context_manager: view.context_manager,
            token_counter: &view.token_counter,
            skills_prompt: view.last_skills_prompt,
            index: index_access,
            correction_config: view.correction_config,
            sidequest_turn_counter: view.sidequest_turn_counter,
            messages: window.messages,
            query,
            scrub: view.scrub,
            active_levels,
        };

        let prepared = zeph_context::assembler::ContextAssembler::gather(&input).await?;

        let delta = self.apply_prepared_context(window, view, prepared).await;
        Ok(delta)
    }

    /// Apply a [`PreparedContext`] to the message window.
    ///
    /// Injects all fetched messages in insertion order (`doc_rag` → corrections → recall →
    /// cross-session → summaries → persona → trajectory → tree → reasoning), handles
    /// `MemoryFirst` history drain, sanitizes memory content, trims to budget, and injects
    /// the session digest. Returns a [`ContextDelta`] whose `code_context` field the caller
    /// must apply via `inject_code_context`.
    #[allow(clippy::too_many_lines)] // sequential message injection: order matters, cannot split
    async fn apply_prepared_context(
        &self,
        window: &mut MessageWindowView<'_>,
        view: &mut ContextAssemblyView<'_>,
        prepared: zeph_context::assembler::PreparedContext,
    ) -> ContextDelta {
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

        // Insert memory messages at position 1 (all sanitized before insertion — CRIT-02).
        if let Some(msg) = prepared.graph_facts.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
            tracing::debug!("injected knowledge graph facts into context");
        }
        if let Some(msg) = prepared.doc_rag.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
            tracing::debug!("injected document RAG context");
        }
        if let Some(msg) = prepared.corrections.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ConversationHistory, view)
                .await;
            window.messages.insert(1, sanitized);
            tracing::debug!("injected past corrections into context");
        }
        if let Some(msg) = prepared.recall.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ConversationHistory, view)
                .await;
            window.messages.insert(1, sanitized);
        }
        if let Some(msg) = prepared.cross_session.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::LlmSummary, view)
                .await;
            window.messages.insert(1, sanitized);
        }
        if let Some(msg) = prepared.summaries.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::LlmSummary, view)
                .await;
            window.messages.insert(1, sanitized);
            tracing::debug!("injected summaries into context");
        }
        if let Some(msg) = prepared.persona_facts.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
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
            tracing::debug!("injected trajectory hints into context");
        }
        if let Some(msg) = prepared.tree_memory.filter(|_| window.messages.len() > 1) {
            let sanitized = self
                .sanitize_memory_message(msg, MemorySourceHint::ExternalContent, view)
                .await;
            window.messages.insert(1, sanitized);
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

        ContextDelta { code_context }
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
    pub async fn reset_conversation(
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
        clippy::cast_sign_loss
    )]
    pub async fn maybe_compact(
        &self,
        summ: &mut ContextSummarizationView<'_>,
        _providers: &ProviderHandles,
        status: &(impl StatusSink + ?Sized),
    ) -> Result<(), ContextError> {
        use zeph_context::manager::{CompactionState, CompactionTier};

        // Increment turn counter unconditionally (tracks pressure regardless of guards).
        if let Some(ref mut count) = summ.context_manager.turns_since_last_hard_compaction {
            *count += 1;
        }

        // Guard: exhaustion — warn once, then no-op permanently.
        if let CompactionState::Exhausted { ref mut warned } = summ.context_manager.compaction
            && !*warned
        {
            *warned = true;
            tracing::warn!("compaction exhausted: context budget too tight for this session");
        }
        if summ.context_manager.compaction.is_exhausted() {
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
        if summ.context_manager.compaction.is_compacted_this_turn() {
            return Ok(());
        }

        // Decrement cooldown counter; record whether we are in cooldown.
        let in_cooldown = summ.context_manager.compaction.cooldown_remaining() > 0;
        if in_cooldown
            && let CompactionState::Cooling {
                ref mut turns_remaining,
            } = summ.context_manager.compaction
        {
            *turns_remaining -= 1;
            if *turns_remaining == 0 {
                summ.context_manager.compaction = CompactionState::Ready;
            }
        }

        match summ
            .context_manager
            .compaction_tier(*summ.cached_prompt_tokens)
        {
            CompactionTier::None => Ok(()),
            CompactionTier::Soft => {
                self.do_soft_compaction(summ, status).await;
                Ok(())
            }
            CompactionTier::Hard => self.do_hard_compaction(summ, status, in_cooldown).await,
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
            .turns_since_last_hard_compaction
            .map(|t| u32::try_from(t).unwrap_or(u32::MAX));
        summ.context_manager.turns_since_last_hard_compaction = Some(0);
        if let Some(metrics) = summ.metrics {
            metrics.record_hard_compaction(turns_since_last);
        }

        if in_cooldown {
            tracing::debug!(
                turns_remaining = summ.context_manager.compaction.cooldown_remaining(),
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
            summ.context_manager.compaction = CompactionState::CompactedThisTurn {
                cooldown: summ.context_manager.compaction_cooldown_turns,
            };
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
            summ.context_manager.compaction = CompactionState::Exhausted { warned: false };
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
            summ.context_manager.compaction = CompactionState::Exhausted { warned: false };
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
            summ.context_manager.compaction = CompactionState::Exhausted { warned: false };
            status.send_status("").await;
            return Ok(());
        }

        summ.context_manager.compaction = CompactionState::CompactedThisTurn {
            cooldown: summ.context_manager.compaction_cooldown_turns,
        };

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
    pub async fn maybe_proactive_compress(
        &self,
        summ: &mut ContextSummarizationView<'_>,
        _providers: &ProviderHandles,
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
                summ.context_manager.compaction =
                    zeph_context::manager::CompactionState::CompactedThisTurn { cooldown: 0 };
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

/// Remove system messages that match either a typed `MessagePart` or a content prefix.
///
/// Typed-part matching takes priority — a message is removed if its **first** part
/// satisfies `part_matches`. As a fallback, messages that start with `prefix` are also
/// removed. Non-system messages are always retained.
pub(crate) fn remove_by_part_or_prefix(
    messages: &mut Vec<zeph_llm::provider::Message>,
    prefix: &str,
    part_matches: impl Fn(&MessagePart) -> bool,
) {
    messages.retain(|m| {
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
}
