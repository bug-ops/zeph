// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;

use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, MessagePart, Role};
use zeph_skills::ScoredMatch;
use zeph_skills::loader::SkillMeta;
use zeph_skills::prompt::{format_skills_catalog, format_skills_prompt_compact};

use super::super::LSP_NOTE_PREFIX;
use super::super::{
    Agent, CODE_CONTEXT_PREFIX, CORRECTIONS_PREFIX, CROSS_SESSION_PREFIX, DOCUMENT_RAG_PREFIX,
    GRAPH_FACTS_PREFIX, RECALL_PREFIX, SESSION_DIGEST_PREFIX, SUMMARY_PREFIX, Skill,
    format_skills_prompt,
};
use crate::channel::Channel;
use crate::context::build_system_prompt_with_instructions;
use crate::redact::scrub_content;
use zeph_sanitizer::{ContentSource, ContentSourceKind, MemorySourceHint};

impl<C: Channel> Agent<C> {
    pub(in crate::agent) fn clear_history(&mut self) {
        let system_prompt = self.msg.messages.first().cloned();
        self.msg.messages.clear();
        if let Some(sp) = system_prompt {
            self.msg.messages.push(sp);
        }
        // Clear completed tool IDs along with message history: dependency state is
        // session-scoped and should reset when the conversation resets.
        self.tool_state.completed_tool_ids.clear();
        self.recompute_prompt_tokens();
    }

    fn remove_by_prefix(&mut self, role: Role, prefix: &str) {
        self.msg
            .messages
            .retain(|m| m.role != role || !m.content.starts_with(prefix));
    }

    fn remove_by_part_or_prefix(
        &mut self,
        prefix: &str,
        part_matches: impl Fn(&MessagePart) -> bool,
    ) {
        self.msg.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts.first().is_some_and(&part_matches) {
                return false;
            }
            !m.content.starts_with(prefix)
        });
    }

    pub(in crate::agent) fn remove_recall_messages(&mut self) {
        self.remove_by_part_or_prefix(RECALL_PREFIX, |p| matches!(p, MessagePart::Recall { .. }));
    }

    pub(in crate::agent) fn remove_correction_messages(&mut self) {
        self.remove_by_prefix(Role::System, CORRECTIONS_PREFIX);
    }

    pub(in crate::agent) fn remove_graph_facts_messages(&mut self) {
        self.remove_by_prefix(Role::System, GRAPH_FACTS_PREFIX);
    }

    pub(in crate::agent) fn remove_persona_facts_messages(&mut self) {
        self.remove_by_prefix(Role::System, super::PERSONA_PREFIX);
    }

    pub(in crate::agent) fn remove_trajectory_hints_messages(&mut self) {
        self.remove_by_prefix(Role::System, super::TRAJECTORY_PREFIX);
    }

    pub(in crate::agent) fn remove_tree_memory_messages(&mut self) {
        self.remove_by_prefix(Role::System, super::TREE_MEMORY_PREFIX);
    }

    /// Remove previously injected LSP context notes from the message history.
    ///
    /// Called before injecting fresh notes each turn so stale diagnostics/hover
    /// data from the previous tool call do not accumulate across iterations.
    /// LSP notes use `Role::System` (consistent with graph facts and recall),
    /// so they are skipped by tool-pair summarization automatically.
    pub(in crate::agent) fn remove_lsp_messages(&mut self) {
        self.remove_by_prefix(Role::System, LSP_NOTE_PREFIX);
    }

    #[cfg(test)]
    pub(in crate::agent) async fn inject_semantic_recall(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> Result<(), super::super::error::AgentError> {
        self.remove_recall_messages();

        let (msg, _score) = super::assembler::fetch_semantic_recall(
            &self.memory_state,
            query,
            token_budget,
            &self.metrics.token_counter,
            None,
        )
        .await?;
        if let Some(msg) = msg
            && self.msg.messages.len() > 1
        {
            self.msg.messages.insert(1, msg);
        }

        Ok(())
    }

    pub(in crate::agent) fn remove_code_context_messages(&mut self) {
        self.remove_by_part_or_prefix(CODE_CONTEXT_PREFIX, |p| {
            matches!(p, MessagePart::CodeContext { .. })
        });
    }

    pub(super) fn remove_summary_messages(&mut self) {
        self.remove_by_part_or_prefix(SUMMARY_PREFIX, |p| matches!(p, MessagePart::Summary { .. }));
    }

    pub(super) fn remove_cross_session_messages(&mut self) {
        self.remove_by_part_or_prefix(CROSS_SESSION_PREFIX, |p| {
            matches!(p, MessagePart::CrossSession { .. })
        });
    }

    fn remove_document_rag_messages(&mut self) {
        self.remove_by_prefix(Role::System, DOCUMENT_RAG_PREFIX);
    }

    pub(in crate::agent) fn remove_session_digest_message(&mut self) {
        self.remove_by_prefix(Role::User, SESSION_DIGEST_PREFIX);
    }

    /// Spawn a fire-and-forget background task to generate and persist a session digest for
    /// `conversation_id`. No-op when digest is disabled or the conversation has no messages.
    fn spawn_outgoing_digest(&self, conversation_id: Option<zeph_memory::ConversationId>) {
        if !self.memory_state.compaction.digest_config.enabled {
            return;
        }
        let non_system: Vec<_> = self
            .msg
            .messages
            .iter()
            .skip(1)
            .filter(|m| m.role != zeph_llm::provider::Role::System)
            .cloned()
            .collect();
        if non_system.is_empty() {
            return;
        }
        let digest_config = self.memory_state.compaction.digest_config.clone();
        let memory = self.memory_state.persistence.memory.clone();
        let provider = self.provider.clone();
        let tc = self.metrics.token_counter.clone();
        let status_tx = self.session.status_tx.clone();
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send("Generating session digest...".to_string());
        }
        tokio::spawn(async move {
            if let (Some(mem), Some(cid)) = (memory, conversation_id) {
                super::super::session_digest::generate_and_store_digest(
                    &provider,
                    &mem,
                    cid,
                    &non_system,
                    &digest_config,
                    &tc,
                )
                .await;
            }
            if let Some(tx) = status_tx {
                let _ = tx.send(String::new());
            }
        });
    }

    /// Reset the conversation window for `/new`.
    ///
    /// Creates a new `ConversationId` in `SQLite` first (fail-fast: no state is mutated
    /// if the `DB` call fails). Then resets all session-scoped state while preserving
    /// cross-session state (memory, MCP, providers, skills).
    ///
    /// `keep_plan` — when `true`, `orchestration.pending_graph` is preserved.
    /// `no_digest` — when `true`, skip generating a session digest for the outgoing
    ///               conversation. Default behaviour: generate digest fire-and-forget.
    ///
    /// Returns the old and new `ConversationId` for the confirmation message.
    ///
    /// # Errors
    ///
    /// Returns an error if [`create_conversation`](zeph_memory::store::SqliteStore::create_conversation)
    /// fails. In that case no agent state is modified.
    pub(in crate::agent) async fn reset_conversation(
        &mut self,
        keep_plan: bool,
        no_digest: bool,
    ) -> Result<
        (
            Option<zeph_memory::ConversationId>,
            Option<zeph_memory::ConversationId>,
        ),
        super::super::error::AgentError,
    > {
        // --- Step 1: create new ConversationId FIRST (fail-fast) ---
        // Clone the Arc before .await so &mut self is not held across the await boundary.
        let memory_arc = self.memory_state.persistence.memory.clone();
        let new_conversation_id = if let Some(memory) = memory_arc {
            match memory.sqlite().create_conversation().await {
                Ok(id) => Some(id),
                Err(e) => return Err(super::super::error::AgentError::Memory(e)),
            }
        } else {
            None
        };

        let old_conversation_id = self.memory_state.persistence.conversation_id;

        // --- Step 2: fire-and-forget digest for outgoing conversation ---
        if !no_digest {
            self.spawn_outgoing_digest(old_conversation_id);
        }

        // --- Step 3: TUI status ---
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send("Resetting conversation...".to_string());
        }

        // --- Step 4: abort background compression tasks (context-compression) ---
        {
            if let Some(h) = self.compression.pending_task_goal.take() {
                h.abort();
            }
            if let Some(h) = self.compression.pending_sidequest_result.take() {
                h.abort();
            }
            if let Some(h) = self.compression.pending_subgoal.take() {
                h.abort();
            }
            self.compression.current_task_goal = None;
            self.compression.task_goal_user_msg_hash = None;
            self.compression.subgoal_registry =
                crate::agent::compaction_strategy::SubgoalRegistry::default();
            self.compression.subgoal_user_msg_hash = None;
        }

        // --- Step 5: cancel running plan and clear orchestration ---
        if !keep_plan {
            if let Some(token) = self.orchestration.plan_cancel_token.take() {
                token.cancel();
            }
            self.orchestration.pending_graph = None;
            self.orchestration.pending_goal_embedding = None;
        }
        // Cancel running sub-agents regardless of keep_plan.
        if let Some(ref mut mgr) = self.orchestration.subagent_manager {
            mgr.shutdown_all();
        }

        // --- Step 6: reset message history and caches ---
        self.clear_history();
        self.tool_orchestrator.clear_cache();

        // Drain message queue, logging discarded entries.
        let discarded = self.clear_queue();
        if discarded > 0 {
            tracing::debug!(
                discarded,
                "/new: discarded queued messages that arrived during reset"
            );
        }
        self.msg.pending_image_parts.clear();

        // --- Step 7: reset security URL sets ---
        self.security.user_provided_urls.write().clear();
        self.security.flagged_urls.clear();

        // --- Step 8: reset compaction and compression states ---
        self.context_manager.reset_compaction();
        self.focus.reset();
        self.sidequest.reset();

        // --- Step 9: reset misc session-scoped fields ---
        self.debug_state.iteration_counter = 0;
        self.msg.last_persisted_message_id = None;
        self.msg.deferred_db_hide_ids.clear();
        self.msg.deferred_db_summaries.clear();
        self.tool_state.cached_filtered_tool_ids = None;
        self.providers.cached_prompt_tokens = 0;

        // --- Step 10: update conversation ID and memory state ---
        self.memory_state.persistence.conversation_id = new_conversation_id;
        self.memory_state.persistence.unsummarized_count = 0;
        // Clear cached digest — the new conversation has no prior digest yet.
        self.memory_state.compaction.cached_session_digest = None;

        // --- Step 11: clear TUI status ---
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send(String::new());
        }

        Ok((old_conversation_id, new_conversation_id))
    }

    #[cfg(test)]
    pub(super) async fn inject_cross_session_context(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> Result<(), super::super::error::AgentError> {
        self.remove_cross_session_messages();

        if let Some(msg) = super::assembler::fetch_cross_session(
            &self.memory_state,
            query,
            token_budget,
            &self.metrics.token_counter,
        )
        .await?
            && self.msg.messages.len() > 1
        {
            self.msg.messages.insert(1, msg);
            tracing::debug!("injected cross-session context");
        }

        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn inject_summaries(
        &mut self,
        token_budget: usize,
    ) -> Result<(), super::super::error::AgentError> {
        self.remove_summary_messages();

        if let Some(msg) = super::assembler::fetch_summaries(
            &self.memory_state,
            token_budget,
            &self.metrics.token_counter,
        )
        .await?
            && self.msg.messages.len() > 1
        {
            self.msg.messages.insert(1, msg);
            tracing::debug!("injected summaries into context");
        }

        Ok(())
    }

    pub(super) fn trim_messages_to_budget(&mut self, token_budget: usize) {
        if token_budget == 0 {
            return;
        }

        let history_start = self
            .msg
            .messages
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(self.msg.messages.len());

        if history_start >= self.msg.messages.len() {
            return;
        }

        let mut total = 0usize;
        let mut keep_from = self.msg.messages.len();

        for i in (history_start..self.msg.messages.len()).rev() {
            let msg_tokens = self
                .metrics
                .token_counter
                .count_message_tokens(&self.msg.messages[i]);
            if total + msg_tokens > token_budget {
                break;
            }
            total += msg_tokens;
            keep_from = i;
        }

        if keep_from > history_start {
            let removed = keep_from - history_start;
            self.msg.messages.drain(history_start..keep_from);
            self.recompute_prompt_tokens();
            tracing::info!(
                removed,
                token_budget,
                "trimmed messages to fit context budget"
            );
        }
    }

    /// Gather context from all memory sources and inject into the message window.
    ///
    /// Delegates concurrent fetching to [`zeph_context::assembler::ContextAssembler::gather`] and
    /// then calls [`Self::apply_prepared_context`] to mutate the message window.
    pub(in crate::agent) async fn prepare_context(
        &mut self,
        query: &str,
    ) -> Result<(), super::super::error::AgentError> {
        if self.context_manager.budget.is_none() {
            return Ok(());
        }
        let _ = self.channel.send_status("recalling context...").await;

        // Remove stale injected messages before concurrent fetch.
        self.remove_session_digest_message();
        self.remove_summary_messages();
        self.remove_cross_session_messages();
        self.remove_recall_messages();
        self.remove_document_rag_messages();
        self.remove_correction_messages();
        self.remove_code_context_messages();
        self.remove_graph_facts_messages();
        self.remove_persona_facts_messages();
        self.remove_trajectory_hints_messages();
        self.remove_tree_memory_messages();

        let memory_view = zeph_context::input::ContextMemoryView {
            memory: self.memory_state.persistence.memory.clone(),
            conversation_id: self.memory_state.persistence.conversation_id,
            recall_limit: self.memory_state.persistence.recall_limit,
            cross_session_score_threshold: self
                .memory_state
                .persistence
                .cross_session_score_threshold,
            context_strategy: self.memory_state.compaction.context_strategy,
            crossover_turn_threshold: self.memory_state.compaction.crossover_turn_threshold,
            cached_session_digest: self.memory_state.compaction.cached_session_digest.clone(),
            graph_config: self.memory_state.extraction.graph_config.clone(),
            document_config: self.memory_state.extraction.document_config.clone(),
            persona_config: self.memory_state.extraction.persona_config.clone(),
            trajectory_config: self.memory_state.extraction.trajectory_config.clone(),
            tree_config: self.memory_state.subsystems.tree_config.clone(),
        };
        let correction_config =
            self.learning_engine
                .config
                .as_ref()
                .map(|c| zeph_context::input::CorrectionConfig {
                    correction_detection: c.correction_detection,
                    correction_recall_limit: c.correction_recall_limit,
                    correction_min_similarity: c.correction_min_similarity,
                });
        let index_access: Option<&dyn zeph_context::input::IndexAccess> =
            self.index.as_index_access();
        let input = zeph_context::input::ContextAssemblyInput {
            memory: &memory_view,
            context_manager: &self.context_manager,
            token_counter: &self.metrics.token_counter,
            skills_prompt: &self.skill_state.last_skills_prompt,
            index: index_access,
            correction_config,
            sidequest_turn_counter: self.sidequest.turn_counter,
            messages: &self.msg.messages,
            query,
            scrub: crate::redact::scrub_content,
        };

        let prepared = zeph_context::assembler::ContextAssembler::gather(&input)
            .await
            .map_err(|e| super::super::error::AgentError::Other(format!("{e:#}")))
            .inspect_err(|_| {
                // Status clear is best-effort; we drop the future intentionally.
                std::mem::drop(self.channel.send_status(""));
            })?;

        self.apply_prepared_context(prepared).await;
        let _ = self.channel.send_status("").await;
        Ok(())
    }

    /// Apply a [`zeph_context::assembler::PreparedContext`] to the agent's message window.
    ///
    /// Injects all fetched messages in order, handles `MemoryFirst` history drain, sanitizes
    /// memory content, trims to budget, and injects the session digest.
    #[allow(clippy::too_many_lines)] // sequential message injection: order matters, cannot split
    async fn apply_prepared_context(&mut self, prepared: zeph_context::assembler::PreparedContext) {
        use std::borrow::Cow;
        use zeph_sanitizer::{ContentSource, ContentSourceKind, MemorySourceHint};

        // Store top-1 recall score on agent state for MAR routing signal.
        self.memory_state.persistence.last_recall_confidence = prepared.recall_confidence;

        // MemoryFirst: drain conversation history BEFORE inserting memory messages so that the
        // memory inserts land into the shorter array and are not accidentally removed.
        if prepared.memory_first {
            let history_start = 1usize; // skip system prompt
            let len = self.msg.messages.len();
            let keep_tail = memory_first_keep_tail(&self.msg.messages, history_start);
            if len > history_start + keep_tail {
                self.msg.messages.drain(history_start..len - keep_tail);
                self.recompute_prompt_tokens();
                tracing::debug!(
                    strategy = "memory_first",
                    keep_tail,
                    "dropped conversation history, kept last {keep_tail} messages"
                );
            }
        }

        // Insert fetched messages (order: doc_rag, corrections, recall, cross-session, summaries at position 1)
        // All memory-sourced messages are sanitized before insertion (CRIT-02: memory poisoning defense).
        // Each path carries a MemorySourceHint that modulates injection detection sensitivity:
        //   ExternalContent  — full detection (graph facts, document RAG may hold adversarial content)
        //   ConversationHistory — detection skipped (user's own prior turns, false-positive suppression)
        //   LlmSummary       — detection skipped (generated by our model from already-sanitized content)
        if let Some(msg) = prepared.graph_facts.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ExternalContent)
                    .await,
            ); // lgtm[rust/cleartext-logging]
            tracing::debug!("injected knowledge graph facts into context");
        }
        if let Some(msg) = prepared.doc_rag.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ExternalContent)
                    .await,
            ); // lgtm[rust/cleartext-logging]
            tracing::debug!("injected document RAG context");
        }
        if let Some(msg) = prepared.corrections.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ConversationHistory)
                    .await,
            ); // lgtm[rust/cleartext-logging]
            tracing::debug!("injected past corrections into context");
        }
        if let Some(msg) = prepared.recall.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ConversationHistory)
                    .await,
            ); // lgtm[rust/cleartext-logging]
        }
        if let Some(msg) = prepared
            .cross_session
            .filter(|_| self.msg.messages.len() > 1)
        {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::LlmSummary)
                    .await,
            ); // lgtm[rust/cleartext-logging]
        }
        if let Some(msg) = prepared.summaries.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::LlmSummary)
                    .await,
            ); // lgtm[rust/cleartext-logging]
            tracing::debug!("injected summaries into context");
        }
        // Persona facts are inserted last so they land immediately after the system prompt (pos 1).
        if let Some(msg) = prepared
            .persona_facts
            .filter(|_| self.msg.messages.len() > 1)
        {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ExternalContent)
                    .await,
            ); // lgtm[rust/cleartext-logging]
            tracing::debug!("injected persona facts into context");
        }

        if let Some(msg) = prepared
            .trajectory_hints
            .filter(|_| self.msg.messages.len() > 1)
        {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ExternalContent)
                    .await,
            );
            tracing::debug!("injected trajectory hints into context");
        }

        if let Some(msg) = prepared.tree_memory.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ExternalContent)
                    .await,
            );
            tracing::debug!("injected tree memory summary into context");
        }

        if let Some(text) = prepared.code_context {
            // Sanitize before injection: indexed repo files can contain injection patterns
            // embedded in comments, docstrings, or string literals.
            let sanitized = self
                .security
                .sanitizer
                .sanitize(&text, ContentSource::new(ContentSourceKind::ToolResult));
            self.update_metrics(|m| m.sanitizer_runs += 1);
            if !sanitized.injection_flags.is_empty() {
                tracing::warn!(
                    flags = sanitized.injection_flags.len(),
                    "injection patterns detected in code RAG context"
                );
                self.update_metrics(|m| {
                    m.sanitizer_injection_flags += sanitized.injection_flags.len() as u64;
                });
                let detail = sanitized
                    .injection_flags
                    .first()
                    .map_or_else(String::new, |f| {
                        format!("Detected pattern: {}", f.pattern_name)
                    });
                self.push_security_event(
                    crate::metrics::SecurityEventCategory::InjectionFlag,
                    "code_rag",
                    detail,
                );
            }
            if sanitized.was_truncated {
                self.update_metrics(|m| m.sanitizer_truncations += 1);
                self.push_security_event(
                    crate::metrics::SecurityEventCategory::Truncation,
                    "code_rag",
                    "Content truncated to max_content_size",
                );
            }
            self.inject_code_context(&sanitized.body);
        }

        if !prepared.memory_first {
            self.trim_messages_to_budget(prepared.recent_history_budget);
        }

        // Inject session digest AFTER all other memory inserts so it lands at position 1
        // (closest to the system prompt). #2289
        if let Some((digest_text, _)) = self
            .memory_state
            .compaction
            .cached_session_digest
            .clone()
            .filter(|_| self.msg.messages.len() > 1)
        {
            let digest_msg = Message {
                role: Role::User,
                content: format!("{SESSION_DIGEST_PREFIX}{digest_text}"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            };
            let sanitized = self
                .sanitize_memory_message(digest_msg, MemorySourceHint::LlmSummary)
                .await;
            self.msg.messages.insert(1, sanitized);
            tracing::debug!("injected session digest into context");
        }

        if self.runtime.redact_credentials {
            for msg in &mut self.msg.messages {
                if msg.role == Role::System {
                    continue;
                }
                if let Cow::Owned(s) = scrub_content(&msg.content) {
                    msg.content = s;
                }
            }
        }

        self.recompute_prompt_tokens();
    }

    /// Apply spotlighting sanitization to a memory retrieval message before inserting it
    /// into the context. Memory content is `ExternalUntrusted` because prior sessions may
    /// have stored poisoned content retrieved from web scraping or MCP responses.
    ///
    /// This is the SOLE sanitization point for the 6 memory retrieval paths (`doc_rag`,
    /// corrections, recall, `cross_session`, summaries, `graph_facts`). Do not add redundant
    /// sanitization in zeph-memory or at other call sites.
    ///
    /// The `hint` parameter modulates injection detection sensitivity:
    /// - `ConversationHistory` / `LlmSummary`: detection skipped (false-positive suppression).
    /// - `ExternalContent`: full detection (document RAG, graph facts).
    ///
    /// Truncation, control-char stripping, delimiter escaping, and spotlighting remain active
    /// for all hints (defense-in-depth invariant).
    async fn sanitize_memory_message(&self, mut msg: Message, hint: MemorySourceHint) -> Message {
        let source = ContentSource::new(ContentSourceKind::MemoryRetrieval).with_memory_hint(hint);
        let sanitized = self.security.sanitizer.sanitize(&msg.content, source);
        self.update_metrics(|m| m.sanitizer_runs += 1);
        if !sanitized.injection_flags.is_empty() {
            tracing::warn!(
                flags = sanitized.injection_flags.len(),
                "injection patterns detected in memory retrieval"
            );
            self.update_metrics(|m| {
                m.sanitizer_injection_flags += sanitized.injection_flags.len() as u64;
            });
            let detail = sanitized
                .injection_flags
                .first()
                .map_or_else(String::new, |f| {
                    format!("Detected pattern: {}", f.pattern_name)
                });
            self.push_security_event(
                crate::metrics::SecurityEventCategory::InjectionFlag,
                "memory_retrieval",
                detail,
            );
        }
        if sanitized.was_truncated {
            self.update_metrics(|m| m.sanitizer_truncations += 1);
            self.push_security_event(
                crate::metrics::SecurityEventCategory::Truncation,
                "memory_retrieval",
                "Content truncated to max_content_size",
            );
        }

        // Quarantine step: route high-risk sources through an isolated LLM (defense-in-depth).
        if self.security.sanitizer.is_enabled()
            && let Some(ref qs) = self.security.quarantine_summarizer
            && qs.should_quarantine(ContentSourceKind::MemoryRetrieval)
        {
            match qs.extract_facts(&sanitized, &self.security.sanitizer).await {
                Ok((facts, flags)) => {
                    self.update_metrics(|m| m.quarantine_invocations += 1);
                    self.push_security_event(
                        crate::metrics::SecurityEventCategory::Quarantine,
                        "memory_retrieval",
                        "Content quarantined, facts extracted",
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
                    self.update_metrics(|m| m.quarantine_failures += 1);
                    self.push_security_event(
                        crate::metrics::SecurityEventCategory::Quarantine,
                        "memory_retrieval",
                        format!("Quarantine failed: {e}"),
                    );
                }
            }
        }

        msg.content = sanitized.body;
        msg
    }

    pub(super) async fn disambiguate_skills(
        &self,
        query: &str,
        all_meta: &[&SkillMeta],
        scored: &[ScoredMatch],
    ) -> Option<Vec<usize>> {
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

        let messages = vec![Message::from_legacy(Role::User, prompt)];
        match self
            .provider
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

    #[allow(clippy::too_many_lines)] // system prompt assembly: skills + tools + knowledge sections, tightly coupled formatting
    pub(in crate::agent) async fn rebuild_system_prompt(&mut self, query: &str) {
        let all_meta: Vec<zeph_skills::loader::SkillMeta> = self
            .skill_state
            .registry
            .read()
            .all_meta()
            .into_iter()
            .cloned()
            .collect();
        let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta.iter().collect();
        let all_meta = all_meta_refs;
        // Tracks only skills that were genuinely scored by the embedding matcher.
        // Stays empty when falling back to the full skill set (no matcher, embed failure).
        let mut skills_to_record: Vec<String> = Vec::new();

        let matched_indices: Vec<usize> = if let Some(matcher) = &self.skill_state.matcher {
            let provider = self.embedding_provider.clone();
            let _ = self.channel.send_status("matching skills...").await;
            let mut scored = matcher
                .match_skills(
                    &all_meta,
                    query,
                    self.skill_state.max_active_skills,
                    self.skill_state.two_stage_matching,
                    |text| {
                        let owned = text.to_owned();
                        let p = provider.clone();
                        Box::pin(async move { p.embed(&owned).await })
                    },
                )
                .await;

            if !scored.is_empty() {
                if self.skill_state.hybrid_search
                    && let Some(ref bm25) = self.skill_state.bm25_index
                {
                    let bm25_results = bm25.search(query, self.skill_state.max_active_skills);
                    scored = zeph_skills::bm25::rrf_fuse(
                        &scored,
                        &bm25_results,
                        self.skill_state.max_active_skills,
                    );
                }

                let metrics_map: std::collections::HashMap<String, (u32, u32)> =
                    if let Some(memory) = &self.memory_state.persistence.memory {
                        memory
                            .sqlite()
                            .load_skill_outcome_stats()
                            .await
                            .unwrap_or_default()
                            .into_iter()
                            .map(|m| {
                                let pair = (
                                    u32::try_from(m.successes).unwrap_or(0),
                                    u32::try_from(m.failures).unwrap_or(0),
                                );
                                (m.skill_name, pair)
                            })
                            .collect()
                    } else {
                        std::collections::HashMap::new()
                    };
                zeph_skills::trust_score::rerank(
                    &mut scored,
                    self.skill_state.cosine_weight,
                    |idx| {
                        all_meta
                            .get(idx)
                            .and_then(|m| metrics_map.get(&m.name))
                            .copied()
                            .unwrap_or((0, 0))
                    },
                );

                // SkillOrchestra: RL routing head re-rank (past warmup only).
                if let Some(rl_head) = &self.skill_state.rl_head
                    && let Ok(query_embed) = self.embedding_provider.embed(query).await
                    && {
                        let ok = query_embed.len() == rl_head.embed_dim();
                        if !ok {
                            tracing::warn!(
                                query_dim = query_embed.len(),
                                head_dim = rl_head.embed_dim(),
                                "rl_head: embed dim mismatch, skipping RL re-rank this turn"
                            );
                        }
                        ok
                    }
                {
                    let rl_weight = self.skill_state.rl_weight;
                    let warmup = self.skill_state.rl_warmup_updates;
                    // Build candidates: (skill_index, skill_embed, cosine_score).
                    // Skills without a stored embedding are skipped (Qdrant backend).
                    let candidates: Vec<(usize, &[f32], f32)> = scored
                        .iter()
                        .filter_map(|s| {
                            matcher
                                .skill_embedding(s.index)
                                .map(|emb| (s.index, emb, s.score))
                        })
                        .collect();
                    if candidates.len() == scored.len() {
                        let stats: Vec<(f32, u32)> = candidates
                            .iter()
                            .map(|&(idx, _, _)| {
                                let (succ, fail) = all_meta
                                    .get(idx)
                                    .and_then(|m| metrics_map.get(&m.name))
                                    .copied()
                                    .unwrap_or((0, 0));
                                let total = succ + fail;
                                let rate = if total == 0 {
                                    0.5
                                } else {
                                    #[allow(clippy::cast_precision_loss)]
                                    {
                                        succ as f32 / total as f32
                                    }
                                };
                                (rate, total)
                            })
                            .collect();
                        let reranked =
                            rl_head.rerank(&query_embed, &candidates, &stats, rl_weight, warmup);
                        // Apply new order to scored.
                        scored.sort_by(|a, b| {
                            let pos_a = reranked.iter().position(|(i, _)| *i == a.index);
                            let pos_b = reranked.iter().position(|(i, _)| *i == b.index);
                            pos_a.cmp(&pos_b)
                        });
                    } else {
                        tracing::debug!(
                            total = scored.len(),
                            with_embeddings = candidates.len(),
                            "RL re-rank skipped: skill embeddings unavailable (Qdrant backend does not expose in-process vectors)"
                        );
                    }
                }
            }

            let indices: Vec<usize> = if scored.is_empty() {
                // Embed or Qdrant failure: fall back to all skills so the agent
                // remains functional rather than running with an empty skill set.
                tracing::warn!("skill matcher returned no results, falling back to all skills");
                (0..all_meta.len()).collect()
            } else {
                // Drop skills whose score falls below the minimum injection floor.
                let min_score = self.skill_state.min_injection_score;
                let pre_retain_count = scored.len();
                let max_score_before_retain = scored
                    .iter()
                    .map(|s| s.score)
                    .fold(f32::NEG_INFINITY, f32::max);
                scored.retain(|s| s.score >= min_score);
                if scored.is_empty() {
                    tracing::warn!(
                        candidate_count = pre_retain_count,
                        threshold = min_score,
                        max_score = max_score_before_retain,
                        "all skill candidates dropped below min_injection_score threshold; running without skills this turn"
                    );
                }

                // Capture the names of skills that had real embedding scores for
                // usage stats — before disambiguation may reorder indices.
                skills_to_record = scored
                    .iter()
                    .filter_map(|s| all_meta.get(s.index).map(|m| m.name.clone()))
                    .collect();

                if scored.len() >= 2
                    && (scored[0].score - scored[1].score)
                        < self.skill_state.disambiguation_threshold
                {
                    match self.disambiguate_skills(query, &all_meta, &scored).await {
                        Some(reordered) => reordered,
                        None => scored.iter().map(|s| s.index).collect(),
                    }
                } else {
                    scored.iter().map(|s| s.index).collect()
                }
            };
            let _ = self.channel.send_status("").await;
            indices
        } else {
            (0..all_meta.len()).collect()
        };

        let matched_indices: Vec<usize> = matched_indices
            .into_iter()
            .filter(|&i| {
                let Some(meta) = all_meta.get(i) else {
                    return false;
                };
                let missing: Vec<&str> = meta
                    .requires_secrets
                    .iter()
                    .filter(|s| {
                        !self
                            .skill_state
                            .available_custom_secrets
                            .contains_key(s.as_str())
                    })
                    .map(String::as_str)
                    .collect();
                if !missing.is_empty() {
                    tracing::info!(
                        skill = %meta.name,
                        missing = ?missing,
                        "skill deactivated: missing required secrets"
                    );
                    return false;
                }
                true
            })
            .collect();

        self.skill_state.active_skill_names = matched_indices
            .iter()
            .filter_map(|&i| all_meta.get(i).map(|m| m.name.clone()))
            .collect();

        let skill_names = self.skill_state.active_skill_names.clone();
        let total = all_meta.len();
        self.update_metrics(|m| {
            m.active_skills = skill_names;
            m.total_skills = total;
        });

        if !skills_to_record.is_empty()
            && let Some(memory) = &self.memory_state.persistence.memory
        {
            let names: Vec<&str> = skills_to_record.iter().map(String::as_str).collect();
            if let Err(e) = memory.sqlite().record_skill_usage(&names).await {
                tracing::warn!("failed to record skill usage: {e:#}");
            }
        }
        self.update_skill_confidence_metrics().await;

        let (all_skills, active_skills): (Vec<Skill>, Vec<Skill>) = {
            let reg = self.skill_state.registry.read();
            let all: Vec<Skill> = reg
                .all_meta()
                .iter()
                .filter_map(|m| reg.get_skill(&m.name).ok())
                .filter(|s| {
                    let allowed =
                        zeph_config::is_skill_allowed(s.name(), &self.runtime.channel_skills);
                    if !allowed {
                        tracing::debug!(skill = s.name(), "skill excluded by channel allowlist");
                    }
                    allowed
                })
                .collect();
            let active: Vec<Skill> = self
                .skill_state
                .active_skill_names
                .iter()
                .filter_map(|name| reg.get_skill(name).ok())
                .filter(|s| {
                    let allowed =
                        zeph_config::is_skill_allowed(s.name(), &self.runtime.channel_skills);
                    if !allowed {
                        tracing::debug!(
                            skill = s.name(),
                            "active skill excluded by channel allowlist"
                        );
                    }
                    allowed
                })
                .collect();
            (all, active)
        };
        let remaining_skills: Vec<Skill> = all_skills
            .iter()
            .filter(|s| {
                !self
                    .skill_state
                    .active_skill_names
                    .contains(&s.name().to_string())
            })
            .cloned()
            .collect();

        let trust_map = self.build_skill_trust_map().await;

        // Apply the most restrictive trust level among active skills to the executor gate.
        let effective_trust = if self.skill_state.active_skill_names.is_empty() {
            zeph_tools::SkillTrustLevel::Trusted
        } else {
            self.skill_state
                .active_skill_names
                .iter()
                .filter_map(|name| trust_map.get(name).copied())
                .fold(zeph_tools::SkillTrustLevel::Trusted, |acc, lvl| {
                    acc.min_trust(lvl)
                })
        };
        self.tool_executor.set_effective_trust(effective_trust);

        // Build health_map: skill_name -> (posterior_mean, total_uses) for XML attributes.
        let health_map: std::collections::HashMap<String, (f64, u32)> = if let Some(memory) =
            &self.memory_state.persistence.memory
        {
            memory
                .sqlite()
                .load_skill_outcome_stats()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|m| {
                    let successes = u32::try_from(m.successes).unwrap_or(0);
                    let failures = u32::try_from(m.failures).unwrap_or(0);
                    let total = successes + failures;
                    let posterior = zeph_skills::trust_score::posterior_mean(successes, failures);
                    (m.skill_name, (posterior, total))
                })
                .collect()
        } else {
            std::collections::HashMap::new()
        };

        let effective_mode = match self.skill_state.prompt_mode {
            crate::config::SkillPromptMode::Auto => {
                if let Some(ref budget) = self.context_manager.budget
                    && budget.max_tokens() < 8192
                {
                    crate::config::SkillPromptMode::Compact
                } else {
                    crate::config::SkillPromptMode::Full
                }
            }
            other => other,
        };

        let mut skills_prompt = if effective_mode == crate::config::SkillPromptMode::Compact {
            format_skills_prompt_compact(&active_skills)
        } else {
            format_skills_prompt(&active_skills, &trust_map, &health_map)
        };
        // ERL: append learned heuristics for active skills (no-op when erl_enabled = false).
        let erl_suffix = self.build_erl_heuristics_prompt().await;
        if !erl_suffix.is_empty() {
            skills_prompt.push_str(&erl_suffix);
        }
        let catalog_prompt = format_skills_catalog(&remaining_skills);
        self.skill_state
            .last_skills_prompt
            .clone_from(&skills_prompt);
        self.session.env_context.refresh_git_branch();
        self.session
            .env_context
            .model_name
            .clone_from(&self.runtime.model_name);

        // MCP tool discovery (#2321 / #2298): select tools relevant to this turn's query.
        // Strategy dispatch: Embedding (new), Llm (existing prune_tools_cached), None (all).
        // Runs before the schema filter so the selected subset feeds into the combined
        // (native + MCP) tool set that the schema filter operates on.
        if !self.mcp.tools.is_empty() {
            match self.mcp.discovery_strategy {
                zeph_mcp::ToolDiscoveryStrategy::Embedding => {
                    let params = self.mcp.discovery_params.clone();
                    if self.mcp.tools.len() < params.min_tools_to_filter {
                        // Below threshold — skip filtering.
                        self.mcp.sync_executor_tools();
                    } else if let Some(ref index) = self.mcp.semantic_index {
                        // Resolve embedding provider for query.
                        let embed_provider = self
                            .mcp
                            .discovery_provider
                            .clone()
                            .unwrap_or_else(|| self.embedding_provider.clone());
                        let _ = self.channel.send_status("selecting tools...").await;
                        match embed_provider.embed(query).await {
                            Ok(query_emb) => {
                                let selected = index.select(
                                    &query_emb,
                                    params.top_k,
                                    params.min_similarity,
                                    &params.always_include,
                                );
                                tracing::info!(
                                    total = self.mcp.tools.len(),
                                    selected = selected.len(),
                                    "semantic tool discovery applied"
                                );
                                self.mcp.apply_pruned_tools(selected);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    strict = params.strict,
                                    "semantic tool discovery: query embed failed, falling back to all tools: {e:#}"
                                );
                                if !params.strict {
                                    self.mcp.sync_executor_tools();
                                }
                                // strict=true: do not sync — executor retains whatever tools it had
                                // (either previously synced or empty). The turn will proceed without
                                // MCP tools rather than silently degrading to the full unfiltered set.
                            }
                        }
                        let _ = self.channel.send_status("").await;
                    } else {
                        // Index not built (build failed at connect time).
                        tracing::warn!(
                            strict = params.strict,
                            "semantic tool discovery: index not available, falling back to all tools"
                        );
                        if !params.strict {
                            self.mcp.sync_executor_tools();
                        }
                    }
                }
                zeph_mcp::ToolDiscoveryStrategy::Llm => {
                    if self.mcp.pruning_enabled {
                        let pruning_provider = self
                            .mcp
                            .pruning_provider
                            .clone()
                            .unwrap_or_else(|| self.provider.clone());
                        let tools_snapshot = self.mcp.tools.clone();
                        let params_snapshot = self.mcp.pruning_params.clone();
                        match zeph_mcp::prune_tools_cached(
                            &mut self.mcp.pruning_cache,
                            &tools_snapshot,
                            query,
                            &params_snapshot,
                            &pruning_provider,
                        )
                        .await
                        {
                            Ok(pruned) => {
                                self.mcp.apply_pruned_tools(pruned);
                            }
                            Err(e) => {
                                tracing::warn!("MCP pruning failed, using all tools: {e:#}");
                                self.mcp.sync_executor_tools();
                            }
                        }
                    } else {
                        // pruning_enabled=false: pass all tools through.
                        self.mcp.sync_executor_tools();
                    }
                }
                zeph_mcp::ToolDiscoveryStrategy::None => {
                    // Pass all tools through without filtering.
                    self.mcp.sync_executor_tools();
                }
            }
        }

        // Dynamic tool schema filtering (#2020): compute once per turn, cache for native path.
        // Query embedding is computed here; when strategy=Embedding already computed it above,
        // but providers are stateless so a second embed() call is acceptable for MVP.
        self.tool_state.cached_filtered_tool_ids = None;
        if let Some(ref filter) = self.tool_state.tool_schema_filter {
            let defs = self.tool_executor.tool_definitions_erased();
            let all_ids: Vec<&str> = defs.iter().map(|d| d.id.as_ref()).collect();
            let descriptions: Vec<(&str, &str)> = defs
                .iter()
                .map(|d| (d.id.as_ref(), d.description.as_ref()))
                .collect();

            let _ = self.channel.send_status("filtering tools...").await;
            match self.embedding_provider.embed(query).await {
                Ok(query_emb) => {
                    let mut result = filter.filter(&all_ids, &descriptions, query, &query_emb);

                    // Apply dependency graph AFTER schema filter (and after any TAFC
                    // augmentation that may have added tools). This ensures hard gates
                    // are the final word on tool availability (MED-04 fix).
                    if let Some(ref dep_graph) = self.tool_state.dependency_graph {
                        let dep_config = &self.runtime.dependency_config;
                        dep_graph.apply(
                            &mut result,
                            &self.tool_state.completed_tool_ids,
                            dep_config.boost_per_dep,
                            dep_config.max_total_boost,
                            &self.tool_state.dependency_always_on,
                        );
                        if !result.dependency_exclusions.is_empty() {
                            tracing::info!(
                                excluded = result.dependency_exclusions.len(),
                                "tool dependency gate: excluded tools with unmet requires"
                            );
                            for excl in &result.dependency_exclusions {
                                tracing::debug!(
                                    tool_id = %excl.tool_id,
                                    unmet = ?excl.unmet_requires,
                                    "tool dependency gate exclusion"
                                );
                            }
                        }
                    }

                    tracing::info!(
                        total = all_ids.len(),
                        included = result.included.len(),
                        excluded = result.excluded.len(),
                        dep_excluded = result.dependency_exclusions.len(),
                        "tool schema filter applied"
                    );
                    for (tool_id, score) in &result.scores {
                        tracing::debug!(tool_id, score, "tool similarity score");
                    }
                    for (tool_id, reason) in &result.inclusion_reasons {
                        tracing::debug!(tool_id, ?reason, "tool inclusion reason");
                    }
                    self.tool_state.cached_filtered_tool_ids = Some(result.included);
                }
                Err(e) => {
                    tracing::warn!("tool filter: query embed failed, using all tools: {e:#}");
                }
            }
            let _ = self.channel.send_status("").await;
        }

        // BLOCK 1: stable within a session — base prompt + skills + tool catalog
        // Instruction blocks are passed separately and injected in the volatile section.
        #[allow(unused_mut)]
        let mut system_prompt = build_system_prompt_with_instructions(
            &skills_prompt,
            Some(&self.session.env_context),
            &self.instructions.blocks,
        );

        // BLOCK 2: semi-stable within a session — skills catalog, MCP, project context, repo map
        if !catalog_prompt.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&catalog_prompt);
        }

        system_prompt.push_str("\n<!-- cache:stable -->");

        self.append_mcp_prompt(query, &mut system_prompt).await;

        let cwd = match self.session.env_context.working_dir.as_str() {
            "" | "unknown" => std::env::current_dir().unwrap_or_default(),
            dir => PathBuf::from(dir),
        };
        let project_configs = crate::project::discover_project_configs(&cwd);
        let project_context = crate::project::load_project_context(&project_configs);
        if !project_context.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&project_context);
        }

        if self.index.repo_map_tokens > 0 {
            let now = std::time::Instant::now();
            let map = if let Some((ref cached, generated_at)) = self.index.cached_repo_map
                && now.duration_since(generated_at) < self.index.repo_map_ttl
            {
                cached.clone()
            } else {
                let cwd2 = cwd.clone();
                let token_budget = self.index.repo_map_tokens;
                let tc = Arc::clone(&self.metrics.token_counter);
                let fresh = tokio::task::spawn_blocking(move || {
                    zeph_index::repo_map::generate_repo_map(&cwd2, token_budget, &tc)
                })
                .await
                .unwrap_or_else(|_| Ok(String::new()))
                .unwrap_or_default();
                self.index.cached_repo_map = Some((fresh.clone(), now));
                fresh
            };
            if !map.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&map);
            }
        }

        // BLOCK 3: volatile — dynamic per-turn content, never cached
        system_prompt.push_str("\n<!-- cache:volatile -->");

        // Inject learned user preferences after the volatile marker so they
        // do not invalidate the stable/semi-stable cache blocks (S2 fix).
        self.inject_learned_preferences(&mut system_prompt).await;

        // If memory_save was used this session, remind the model to use memory_search
        // (not search_code) to recall user-provided facts (#2475).
        if self.tool_state.completed_tool_ids.contains("memory_save") {
            system_prompt.push_str(
                "\n\nFacts provided by the user in this session have been saved with memory_save — use memory_search to recall them, not search_code.",
            );
        }

        // Budget hint injection (#2267): inject remaining cost and tool call budget so the
        // LLM can self-regulate. Only injected when budget_hint_enabled = true (default).
        // Self-suppresses when no budget data sources are available.
        if self.runtime.budget_hint_enabled {
            let remaining_cost_cents = self.metrics.cost_tracker.as_ref().and_then(|ct| {
                let max = ct.max_daily_cents();
                if max > 0.0 {
                    Some((max - ct.current_spend()).max(0.0))
                } else {
                    None
                }
            });
            let total_budget_cents = self.metrics.cost_tracker.as_ref().and_then(|ct| {
                let max = ct.max_daily_cents();
                if max > 0.0 { Some(max) } else { None }
            });
            let max_tool_calls = self.tool_orchestrator.max_iterations;
            let remaining_tool_calls =
                max_tool_calls.saturating_sub(self.tool_state.current_tool_iteration);
            let hint = BudgetHint {
                remaining_cost_cents,
                total_budget_cents,
                remaining_tool_calls,
                max_tool_calls,
            };
            if let Some(xml) = hint.format_xml() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&xml);
            }
        }

        tracing::debug!(
            len = system_prompt.len(),
            skills = ?self.skill_state.active_skill_names,
            "system prompt rebuilt"
        );
        tracing::trace!(prompt = %system_prompt, "full system prompt");

        if let Some(msg) = self.msg.messages.first_mut() {
            msg.content = system_prompt;
        }
    }
}

/// Budget state injected into the volatile system prompt section (#2267).
///
/// All fields are optional — omitted when the corresponding data source is unavailable.
/// `format_xml()` returns `None` when all fields would be absent (nothing to inject).
struct BudgetHint {
    remaining_cost_cents: Option<f64>,
    total_budget_cents: Option<f64>,
    remaining_tool_calls: usize,
    max_tool_calls: usize,
}

impl BudgetHint {
    fn format_xml(&self) -> Option<String> {
        let has_cost = self.remaining_cost_cents.is_some();
        // Always include tool call budget — max_tool_calls > 0 in any real config.
        if !has_cost && self.max_tool_calls == 0 {
            return None;
        }
        let mut s = String::from("<budget>");
        if let Some(remaining) = self.remaining_cost_cents {
            let _ = write!(
                s,
                "\n<remaining_cost_cents>{remaining:.2}</remaining_cost_cents>"
            );
        }
        if let Some(total) = self.total_budget_cents {
            let _ = write!(s, "\n<total_budget_cents>{total:.2}</total_budget_cents>");
        }
        if self.max_tool_calls > 0 {
            let _ = write!(
                s,
                "\n<remaining_tool_calls>{}</remaining_tool_calls>",
                self.remaining_tool_calls
            );
            let _ = write!(
                s,
                "\n<max_tool_calls>{}</max_tool_calls>",
                self.max_tool_calls
            );
        }
        s.push_str("\n</budget>");
        Some(s)
    }
}

/// Compute the number of tail messages to retain during a `MemoryFirst` drain.
///
/// Starts at 2 (coherence anchor) and extends backward past any leading `Role::User` messages
/// that carry `MessagePart::ToolResult` parts. Such messages must always be immediately preceded
use zeph_context::assembler::memory_first_keep_tail;

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_context::assembler::MAX_KEEP_TAIL_SCAN;
    use zeph_llm::provider::{Message, MessagePart, Role};

    // ── effective_recall_timeout_ms tests (#2514) ────────────────────────────

    #[test]
    fn effective_recall_timeout_ms_nonzero_returns_unchanged() {
        let result = crate::agent::context::assembler::effective_recall_timeout_ms(500);
        assert_eq!(result, 500, "non-zero value must pass through unchanged");
    }

    #[test]
    fn effective_recall_timeout_ms_nonzero_large_returns_unchanged() {
        let result = crate::agent::context::assembler::effective_recall_timeout_ms(5000);
        assert_eq!(result, 5000);
    }

    #[test]
    fn effective_recall_timeout_ms_zero_clamps_to_100() {
        let result = crate::agent::context::assembler::effective_recall_timeout_ms(0);
        assert_eq!(
            result, 100,
            "zero recall_timeout_ms must be clamped to 100ms"
        );
    }

    #[test]
    fn spreading_activation_default_timeout_is_nonzero() {
        // Ensures the default used in production is not accidentally set to zero —
        // which would always trigger the zero-clamp warn path in effective_recall_timeout_ms.
        let result = crate::agent::context::assembler::effective_recall_timeout_ms(
            zeph_config::memory::SpreadingActivationConfig::default().recall_timeout_ms,
        );
        assert!(
            result > 0,
            "default recall_timeout_ms must produce a non-zero effective value"
        );
    }

    fn sys() -> Message {
        Message::from_legacy(Role::System, "system prompt")
    }

    fn user(text: &str) -> Message {
        Message::from_legacy(Role::User, text)
    }

    fn assistant(text: &str) -> Message {
        Message::from_legacy(Role::Assistant, text)
    }

    fn tool_use_msg() -> Message {
        Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: "tu1".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
        )
    }

    fn tool_result_msg() -> Message {
        Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "tu1".into(),
                content: "output".into(),
                is_error: false,
            }],
        )
    }

    #[test]
    fn keep_tail_no_tool_calls_returns_two() {
        // Normal conversation: no tool calls at boundary — keep_tail stays 2.
        let msgs = vec![
            sys(),
            user("hello"),
            assistant("hi"),
            user("how are you"),
            assistant("fine"),
        ];
        assert_eq!(memory_first_keep_tail(&msgs, 1), 2);
    }

    #[test]
    fn keep_tail_tool_result_at_boundary_extends_by_one() {
        // Last 2 messages: [tool_result, assistant_reply]
        // first_retained (index len-2) = tool_result  → must extend by 1 to include tool_use
        //   then first_retained becomes tool_use (Assistant) → stop
        let msgs = vec![
            sys(),
            user("q1"),
            assistant("a1"),
            tool_use_msg(),    // index 3: assistant issues tool call
            tool_result_msg(), // index 4: tool result
            assistant("done"), // index 5: assistant reply after tool
        ];
        // len=6, keep_tail starts at 2 → msgs[4]=tool_result → extend to 3 → msgs[3]=tool_use (Assistant) → stop
        assert_eq!(memory_first_keep_tail(&msgs, 1), 3);
    }

    #[test]
    fn keep_tail_multiple_tool_rounds_at_boundary() {
        // Two consecutive tool call/result pairs right before the final reply.
        let msgs = vec![
            sys(),
            user("q1"),
            assistant("a1"),
            tool_use_msg(),    // index 3
            tool_result_msg(), // index 4
            tool_use_msg(),    // index 5: second tool call
            tool_result_msg(), // index 6: second tool result
            assistant("done"), // index 7
        ];
        // len=8
        // keep_tail=2: msgs[6]=tool_result → extend
        // keep_tail=3: msgs[5]=tool_use (Assistant) → stop
        assert_eq!(memory_first_keep_tail(&msgs, 1), 3);
    }

    #[test]
    fn keep_tail_capped_at_available_history() {
        // Only system + one tool_result message (degenerate): keep_tail must not exceed len-history_start.
        let msgs = vec![sys(), tool_result_msg()];
        // len=2, len-history_start=1 → while condition `keep_tail < 1` is false from the start
        assert_eq!(memory_first_keep_tail(&msgs, 1), 2);
    }

    #[test]
    fn keep_tail_capped_at_max_scan_does_not_split_tool_pair() {
        // Build a history: system + (tool_use, tool_result) × 30 pairs + assistant reply.
        // Total: 1 + 60 + 1 = 62 messages. The cap fires at MAX_KEEP_TAIL_SCAN = 50.
        // At that point, keep_tail includes 49 ToolResult messages. The preceding message
        // (index len - 51) is a ToolUse — the fix must extend keep_tail to 51 so the pair
        // is not split.
        let mut msgs = vec![sys()];
        for _ in 0..30 {
            msgs.push(tool_use_msg());
            msgs.push(tool_result_msg());
        }
        msgs.push(assistant("done"));

        let tail = memory_first_keep_tail(&msgs, 1);

        // The result must not exceed MAX_KEEP_TAIL_SCAN + 1 (cap + one extra for ToolUse).
        assert!(
            tail <= MAX_KEEP_TAIL_SCAN + 1,
            "keep_tail {tail} exceeds cap + 1"
        );

        // Verify the first retained message is not a ToolResult without a preceding ToolUse.
        let len = msgs.len();
        let first_retained_idx = len - tail;
        // If the first retained message is a ToolResult, the message just before it must be
        // a ToolUse (or there is no message before it, which is impossible here).
        let first_retained = &msgs[first_retained_idx];
        let is_tool_result = first_retained.role == Role::User
            && first_retained
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }));
        if is_tool_result && first_retained_idx > 0 {
            let preceding = &msgs[first_retained_idx - 1];
            let has_tool_use = preceding.role == Role::Assistant
                && preceding
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }));
            assert!(
                has_tool_use,
                "ToolResult at index {first_retained_idx} has no preceding ToolUse — pair was split"
            );
        }
    }

    // ── BudgetHint tests (#2267) ─────────────────────────────────────────────

    #[test]
    fn budget_hint_all_none_no_xml_when_max_tools_zero() {
        let hint = BudgetHint {
            remaining_cost_cents: None,
            total_budget_cents: None,
            remaining_tool_calls: 0,
            max_tool_calls: 0,
        };
        assert!(hint.format_xml().is_none());
    }

    #[test]
    fn budget_hint_tool_only_produces_xml() {
        let hint = BudgetHint {
            remaining_cost_cents: None,
            total_budget_cents: None,
            remaining_tool_calls: 7,
            max_tool_calls: 10,
        };
        let xml = hint.format_xml().unwrap();
        assert!(xml.contains("<remaining_tool_calls>7</remaining_tool_calls>"));
        assert!(xml.contains("<max_tool_calls>10</max_tool_calls>"));
        assert!(!xml.contains("remaining_cost_cents"));
    }

    #[test]
    fn budget_hint_full_produces_all_fields() {
        let hint = BudgetHint {
            remaining_cost_cents: Some(42.5),
            total_budget_cents: Some(100.0),
            remaining_tool_calls: 5,
            max_tool_calls: 10,
        };
        let xml = hint.format_xml().unwrap();
        assert!(xml.contains("<remaining_cost_cents>42.50</remaining_cost_cents>"));
        assert!(xml.contains("<total_budget_cents>100.00</total_budget_cents>"));
        assert!(xml.contains("<remaining_tool_calls>5</remaining_tool_calls>"));
        assert!(xml.contains("<max_tool_calls>10</max_tool_calls>"));
    }

    #[test]
    fn budget_hint_zero_max_daily_cents_omits_cost_fields() {
        // max_daily_cents == 0.0 means unlimited — cost fields must be omitted.
        let hint = BudgetHint {
            remaining_cost_cents: None, // caller guards with > 0.0 check
            total_budget_cents: None,
            remaining_tool_calls: 3,
            max_tool_calls: 10,
        };
        let xml = hint.format_xml().unwrap();
        assert!(!xml.contains("remaining_cost_cents"));
        assert!(!xml.contains("total_budget_cents"));
    }

    // ── recall snippet filter (#2620) ────────────────────────────────────────

    /// Mirrors the filter condition in `fetch_semantic_recall` — used to verify
    /// that `[skipped]`/`[stopped]` markers are recognised and that normal
    /// snippets are not accidentally rejected.
    fn recall_is_policy_marker(content: &str) -> bool {
        content.starts_with("[skipped]") || content.starts_with("[stopped]")
    }

    /// Simulates the recall-text assembly loop from `fetch_semantic_recall`,
    /// returning only the snippets that pass the policy-marker filter.
    fn apply_recall_filter(snippets: &[&str]) -> Vec<String> {
        snippets
            .iter()
            .filter(|s| !recall_is_policy_marker(s))
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn recall_filter_skipped_marker_is_excluded() {
        let snippets = ["[skipped] bash was blocked by utility gate"];
        let result = apply_recall_filter(&snippets);
        assert!(
            result.is_empty(),
            "[skipped] snippet must be filtered from recall block"
        );
    }

    #[test]
    fn recall_filter_stopped_marker_is_excluded() {
        let snippets = ["[stopped] execution limit reached"];
        let result = apply_recall_filter(&snippets);
        assert!(
            result.is_empty(),
            "[stopped] snippet must be filtered from recall block"
        );
    }

    #[test]
    fn recall_filter_normal_snippet_passes_through() {
        let snippets = ["total 42\ndrwxr-xr-x  5 user group  160 Jan  1 00:00 src"];
        let result = apply_recall_filter(&snippets);
        assert_eq!(
            result.len(),
            1,
            "normal snippet must not be filtered from recall block"
        );
        assert_eq!(result[0], snippets[0]);
    }

    #[test]
    fn recall_filter_mixed_passes_only_normal_snippets() {
        let snippets = [
            "[skipped] bash blocked",
            "real output line",
            "[stopped] limit hit",
            "another real line",
        ];
        let result = apply_recall_filter(&snippets);
        assert_eq!(result, vec!["real output line", "another real line"]);
    }

    #[test]
    fn recall_filter_empty_content_is_not_a_marker() {
        // Empty string does not start with either marker → must pass through.
        let snippets = [""];
        let result = apply_recall_filter(&snippets);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn recall_filter_partial_prefix_is_not_a_marker() {
        // "[skip]" and "[stop]" are not the recognised markers.
        let snippets = ["[skip] not a real marker", "[stop] also not a marker"];
        let result = apply_recall_filter(&snippets);
        assert_eq!(
            result.len(),
            2,
            "partial prefixes must not be treated as policy markers"
        );
    }
}
