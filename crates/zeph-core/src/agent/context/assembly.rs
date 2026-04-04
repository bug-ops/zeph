// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::borrow::Cow;
use std::fmt::Write;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;

use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, MessagePart, Role};
use zeph_memory::TokenCounter;
use zeph_skills::ScoredMatch;
use zeph_skills::loader::SkillMeta;
use zeph_skills::prompt::{format_skills_catalog, format_skills_prompt_compact};

use super::super::LSP_NOTE_PREFIX;
use super::super::{
    Agent, CODE_CONTEXT_PREFIX, CORRECTIONS_PREFIX, CROSS_SESSION_PREFIX, DOCUMENT_RAG_PREFIX,
    GRAPH_FACTS_PREFIX, MemoryState, RECALL_PREFIX, SESSION_DIGEST_PREFIX, SUMMARY_PREFIX, Skill,
    build_system_prompt_with_instructions, format_skills_prompt,
};
use super::ContextSlot;
use crate::channel::Channel;
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
        self.completed_tool_ids.clear();
        self.recompute_prompt_tokens();
    }

    pub(in crate::agent) fn remove_recall_messages(&mut self) {
        self.msg.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::Recall { .. }))
            {
                return false;
            }
            !m.content.starts_with(RECALL_PREFIX)
        });
    }

    pub(in crate::agent) fn remove_correction_messages(&mut self) {
        self.msg
            .messages
            .retain(|m| m.role != Role::System || !m.content.starts_with(CORRECTIONS_PREFIX));
    }

    pub(in crate::agent) fn remove_graph_facts_messages(&mut self) {
        self.msg
            .messages
            .retain(|m| m.role != Role::System || !m.content.starts_with(GRAPH_FACTS_PREFIX));
    }

    /// Remove previously injected LSP context notes from the message history.
    ///
    /// Called before injecting fresh notes each turn so stale diagnostics/hover
    /// data from the previous tool call do not accumulate across iterations.
    /// LSP notes use `Role::System` (consistent with graph facts and recall),
    /// so they are skipped by tool-pair summarization automatically.
    pub(in crate::agent) fn remove_lsp_messages(&mut self) {
        self.msg
            .messages
            .retain(|m| m.role != Role::System || !m.content.starts_with(LSP_NOTE_PREFIX));
    }

    fn effective_recall_timeout_ms(configured: u64) -> u64 {
        if configured == 0 {
            tracing::warn!(
                "recall_timeout_ms is 0, which would disable spreading activation recall; \
                 clamping to 100ms"
            );
            100
        } else {
            configured
        }
    }

    pub(super) async fn fetch_graph_facts(
        memory_state: &MemoryState,
        query: &str,
        budget_tokens: usize,
        tc: &TokenCounter,
    ) -> Result<Option<Message>, super::super::error::AgentError> {
        if budget_tokens == 0 || !memory_state.graph_config.enabled {
            return Ok(None);
        }
        let Some(ref memory) = memory_state.memory else {
            return Ok(None);
        };
        let recall_limit = memory_state.graph_config.recall_limit;
        let temporal_decay_rate = memory_state.graph_config.temporal_decay_rate;
        let edge_types = zeph_memory::classify_graph_subgraph(query);
        let sa_config = &memory_state.graph_config.spreading_activation;

        let mut body = String::from(GRAPH_FACTS_PREFIX);
        let mut tokens_so_far = tc.count_tokens(&body);

        if sa_config.enabled {
            // Build SpreadingActivationParams from config (zeph-memory has no zeph-config dep).
            let sa_params = zeph_memory::graph::SpreadingActivationParams {
                decay_lambda: sa_config.decay_lambda,
                max_hops: sa_config.max_hops,
                activation_threshold: sa_config.activation_threshold,
                inhibition_threshold: sa_config.inhibition_threshold,
                max_activated_nodes: sa_config.max_activated_nodes,
                temporal_decay_rate,
                seed_structural_weight: sa_config.seed_structural_weight,
                seed_community_cap: sa_config.seed_community_cap,
            };
            // Spreading activation path: wrap in a configurable timeout to bound latency.
            let timeout_ms = Self::effective_recall_timeout_ms(sa_config.recall_timeout_ms);
            let recall_fut =
                memory.recall_graph_activated(query, recall_limit, sa_params, &edge_types);
            let activated_facts = match tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                recall_fut,
            )
            .await
            {
                Ok(Ok(facts)) => facts,
                Ok(Err(e)) => {
                    tracing::warn!("spreading activation recall failed: {e:#}");
                    Vec::new()
                }
                Err(_) => {
                    tracing::warn!("spreading activation recall timed out ({timeout_ms}ms)");
                    Vec::new()
                }
            };

            if activated_facts.is_empty() {
                return Ok(None);
            }

            for f in &activated_facts {
                let fact_text = f.edge.fact.replace(['\n', '\r', '<', '>'], " ");
                let line = format!(
                    "- {} (confidence: {:.2}, activation: {:.2})\n",
                    fact_text, f.edge.confidence, f.activation_score
                );
                let line_tokens = tc.count_tokens(&line);
                if tokens_so_far + line_tokens > budget_tokens {
                    break;
                }
                body.push_str(&line);
                tokens_so_far += line_tokens;
            }
        } else {
            // BFS path (default when spreading_activation.enabled = false).
            let max_hops = memory_state.graph_config.max_hops;
            let facts = memory
                .recall_graph(
                    query,
                    recall_limit,
                    max_hops,
                    None,
                    temporal_decay_rate,
                    &edge_types,
                )
                .await
                .map_err(|e| {
                    tracing::warn!("graph recall failed: {e:#}");
                    super::super::error::AgentError::Memory(e)
                })?;

            if facts.is_empty() {
                return Ok(None);
            }

            for f in &facts {
                // Strip newlines and angle-brackets from stored entity names/relations
                // to prevent graph-stored injection strings from escaping into the prompt.
                let fact_text = f.fact.replace(['\n', '\r', '<', '>'], " ");
                let line = format!("- {} (confidence: {:.2})\n", fact_text, f.confidence);
                let line_tokens = tc.count_tokens(&line);
                if tokens_so_far + line_tokens > budget_tokens {
                    break;
                }
                body.push_str(&line);
                tokens_so_far += line_tokens;
            }
        }

        if body == GRAPH_FACTS_PREFIX {
            return Ok(None);
        }

        Ok(Some(Message::from_legacy(Role::System, body)))
    }

    pub(super) fn format_correction_note(_original_output: &str, correction_text: &str) -> String {
        // Never replay the faulty assistant/tool output itself into future prompts.
        // If it contained a bad command with an absolute path, path redaction would turn it
        // into a literal placeholder like `[PATH]` that the model may copy verbatim.
        format!(
            "- Past user correction: \"{}\"",
            super::truncate_chars(&scrub_content(correction_text), 200)
        )
    }

    async fn fetch_corrections(
        memory_state: &MemoryState,
        query: &str,
        limit: usize,
        min_score: f32,
    ) -> Result<Option<Message>, super::super::error::AgentError> {
        let Some(ref memory) = memory_state.memory else {
            return Ok(None);
        };
        let corrections = memory
            .retrieve_similar_corrections(query, limit, min_score)
            .await
            .unwrap_or_default();
        if corrections.is_empty() {
            return Ok(None);
        }
        let mut text = String::from(CORRECTIONS_PREFIX);
        for c in &corrections {
            text.push_str(&Self::format_correction_note(
                &c.original_output,
                &c.correction_text,
            ));
            text.push('\n');
        }
        Ok(Some(Message::from_legacy(Role::System, text)))
    }

    #[cfg(test)]
    pub(in crate::agent) async fn inject_semantic_recall(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> Result<(), super::super::error::AgentError> {
        self.remove_recall_messages();

        let (msg, _score) = Self::fetch_semantic_recall(
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

    async fn fetch_semantic_recall(
        memory_state: &MemoryState,
        query: &str,
        token_budget: usize,
        tc: &TokenCounter,
        router: Option<&dyn zeph_memory::AsyncMemoryRouter>,
    ) -> Result<(Option<Message>, Option<f32>), super::super::error::AgentError> {
        let Some(memory) = &memory_state.memory else {
            return Ok((None, None));
        };
        if memory_state.recall_limit == 0 || token_budget == 0 {
            return Ok((None, None));
        }

        let recalled = if let Some(r) = router {
            memory
                .recall_routed_async(query, memory_state.recall_limit, None, r)
                .await?
        } else {
            memory
                .recall(query, memory_state.recall_limit, None)
                .await?
        };
        if recalled.is_empty() {
            return Ok((None, None));
        }

        let top_score = recalled.first().map(|r| r.score);

        let mut recall_text = String::with_capacity(token_budget * 3);
        recall_text.push_str(RECALL_PREFIX);
        let mut tokens_used = tc.count_tokens(&recall_text);

        for item in &recalled {
            // Filter out internal utility-policy markers so they never surface as recalled
            // context — a [skipped] bash ToolResult in Qdrant would make the LLM believe the
            // tool is blocked and prevent re-dispatch after a Retrieve gate (#2620).
            if item.message.content.starts_with("[skipped]")
                || item.message.content.starts_with("[stopped]")
            {
                continue;
            }
            let role_label = match item.message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };
            let entry = format!("- [{}] {}\n", role_label, item.message.content);
            let entry_tokens = tc.count_tokens(&entry);
            if tokens_used + entry_tokens > token_budget {
                break;
            }
            recall_text.push_str(&entry);
            tokens_used += entry_tokens;
        }

        if tokens_used > tc.count_tokens(RECALL_PREFIX) {
            Ok((
                Some(Message::from_parts(
                    Role::System,
                    vec![MessagePart::Recall { text: recall_text }],
                )),
                top_score,
            ))
        } else {
            Ok((None, None))
        }
    }

    pub(in crate::agent) fn remove_code_context_messages(&mut self) {
        self.msg.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::CodeContext { .. }))
            {
                return false;
            }
            !m.content.starts_with(CODE_CONTEXT_PREFIX)
        });
    }

    pub(super) fn remove_summary_messages(&mut self) {
        self.msg.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::Summary { .. }))
            {
                return false;
            }
            !m.content.starts_with(SUMMARY_PREFIX)
        });
    }

    pub(super) fn remove_cross_session_messages(&mut self) {
        self.msg.messages.retain(|m| {
            if m.role != Role::System {
                return true;
            }
            if m.parts
                .first()
                .is_some_and(|p| matches!(p, MessagePart::CrossSession { .. }))
            {
                return false;
            }
            !m.content.starts_with(CROSS_SESSION_PREFIX)
        });
    }

    fn remove_document_rag_messages(&mut self) {
        self.msg
            .messages
            .retain(|m| m.role != Role::System || !m.content.starts_with(DOCUMENT_RAG_PREFIX));
    }

    pub(in crate::agent) fn remove_session_digest_message(&mut self) {
        self.msg
            .messages
            .retain(|m| m.role != Role::User || !m.content.starts_with(SESSION_DIGEST_PREFIX));
    }

    /// Spawn a fire-and-forget background task to generate and persist a session digest for
    /// `conversation_id`. No-op when digest is disabled or the conversation has no messages.
    fn spawn_outgoing_digest(&self, conversation_id: Option<zeph_memory::ConversationId>) {
        if !self.memory_state.digest_config.enabled {
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
        let digest_config = self.memory_state.digest_config.clone();
        let memory = self.memory_state.memory.clone();
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
        let new_conversation_id = if let Some(ref memory) = self.memory_state.memory {
            match memory.sqlite().create_conversation().await {
                Ok(id) => Some(id),
                Err(e) => return Err(super::super::error::AgentError::Memory(e)),
            }
        } else {
            None
        };

        let old_conversation_id = self.memory_state.conversation_id;

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
        if let Ok(mut urls) = self.security.user_provided_urls.write() {
            urls.clear();
        }
        self.security.flagged_urls.clear();

        // --- Step 8: reset compaction and compression states ---
        self.context_manager.reset_compaction();
        self.focus.reset();
        self.sidequest.reset();

        // --- Step 9: reset misc session-scoped fields ---
        self.debug_state.iteration_counter = 0;
        self.last_persisted_message_id = None;
        self.deferred_db_hide_ids.clear();
        self.deferred_db_summaries.clear();
        self.cached_filtered_tool_ids = None;
        self.providers.cached_prompt_tokens = 0;

        // --- Step 10: update conversation ID and memory state ---
        self.memory_state.conversation_id = new_conversation_id;
        self.memory_state.unsummarized_count = 0;
        // Clear cached digest — the new conversation has no prior digest yet.
        self.memory_state.cached_session_digest = None;

        // --- Step 11: clear TUI status ---
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send(String::new());
        }

        Ok((old_conversation_id, new_conversation_id))
    }

    async fn fetch_document_rag(
        memory_state: &MemoryState,
        query: &str,
        token_budget: usize,
        tc: &TokenCounter,
    ) -> Result<Option<Message>, super::super::error::AgentError> {
        if !memory_state.document_config.rag_enabled || token_budget == 0 {
            return Ok(None);
        }
        let Some(memory) = &memory_state.memory else {
            return Ok(None);
        };

        let collection = &memory_state.document_config.collection;
        let top_k = memory_state.document_config.top_k;
        let points = memory
            .search_document_collection(collection, query, top_k)
            .await?;
        if points.is_empty() {
            return Ok(None);
        }

        let mut text = String::from(DOCUMENT_RAG_PREFIX);
        let mut tokens_used = tc.count_tokens(&text);

        for point in &points {
            let chunk = point
                .payload
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if chunk.is_empty() {
                continue;
            }
            let entry = format!("{chunk}\n");
            let cost = tc.count_tokens(&entry);
            if tokens_used + cost > token_budget {
                break;
            }
            text.push_str(&entry);
            tokens_used += cost;
        }

        if tokens_used > tc.count_tokens(DOCUMENT_RAG_PREFIX) {
            Ok(Some(Message {
                role: Role::System,
                content: text,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }))
        } else {
            Ok(None)
        }
    }

    #[cfg(test)]
    pub(super) async fn inject_cross_session_context(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> Result<(), super::super::error::AgentError> {
        self.remove_cross_session_messages();

        if let Some(msg) = Self::fetch_cross_session(
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

    async fn fetch_cross_session(
        memory_state: &MemoryState,
        query: &str,
        token_budget: usize,
        tc: &TokenCounter,
    ) -> Result<Option<Message>, super::super::error::AgentError> {
        let (Some(memory), Some(cid)) = (&memory_state.memory, memory_state.conversation_id) else {
            return Ok(None);
        };
        if token_budget == 0 {
            return Ok(None);
        }

        let threshold = memory_state.cross_session_score_threshold;
        let results: Vec<_> = memory
            .search_session_summaries(query, 5, Some(cid))
            .await?
            .into_iter()
            .filter(|r| r.score >= threshold)
            .collect();
        if results.is_empty() {
            return Ok(None);
        }

        let mut text = String::from(CROSS_SESSION_PREFIX);
        let mut tokens_used = tc.count_tokens(&text);

        for item in &results {
            let entry = format!("- {}\n", item.summary_text);
            let cost = tc.count_tokens(&entry);
            if tokens_used + cost > token_budget {
                break;
            }
            text.push_str(&entry);
            tokens_used += cost;
        }

        if tokens_used > tc.count_tokens(CROSS_SESSION_PREFIX) {
            Ok(Some(Message::from_parts(
                Role::System,
                vec![MessagePart::CrossSession { text }],
            )))
        } else {
            Ok(None)
        }
    }

    #[cfg(test)]
    pub(super) async fn inject_summaries(
        &mut self,
        token_budget: usize,
    ) -> Result<(), super::super::error::AgentError> {
        self.remove_summary_messages();

        if let Some(msg) = Self::fetch_summaries(
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

    async fn fetch_summaries(
        memory_state: &MemoryState,
        token_budget: usize,
        tc: &TokenCounter,
    ) -> Result<Option<Message>, super::super::error::AgentError> {
        let (Some(memory), Some(cid)) = (&memory_state.memory, memory_state.conversation_id) else {
            return Ok(None);
        };
        if token_budget == 0 {
            return Ok(None);
        }

        let summaries = memory.load_summaries(cid).await?;
        if summaries.is_empty() {
            return Ok(None);
        }

        let mut summary_text = String::from(SUMMARY_PREFIX);
        let mut tokens_used = tc.count_tokens(&summary_text);

        for summary in summaries.iter().rev() {
            let first = summary.first_message_id.map_or(0, |m| m.0);
            let last = summary.last_message_id.map_or(0, |m| m.0);
            let entry = format!("- Messages {first}-{last}: {}\n", summary.content);
            let cost = tc.count_tokens(&entry);
            if tokens_used + cost > token_budget {
                break;
            }
            summary_text.push_str(&entry);
            tokens_used += cost;
        }

        if tokens_used > tc.count_tokens(SUMMARY_PREFIX) {
            Ok(Some(Message::from_parts(
                Role::System,
                vec![MessagePart::Summary { text: summary_text }],
            )))
        } else {
            Ok(None)
        }
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

    // FuturesUnordered is chosen for extensibility (graph-memory, future sources) rather
    // than performance. The overhead of ~7 heap allocations is negligible vs. network I/O.
    #[allow(clippy::too_many_lines)] // parallel context gathering: memory, graph, skill, knowledge — coupled async fanout
    pub(in crate::agent) async fn prepare_context(
        &mut self,
        query: &str,
    ) -> Result<(), super::super::error::AgentError> {
        let Some(ref budget) = self.context_manager.budget else {
            return Ok(());
        };
        let _ = self.channel.send_status("recalling context...").await;

        let system_prompt = self.msg.messages.first().map_or("", |m| m.content.as_str());
        let graph_enabled = self.memory_state.graph_config.enabled;

        // Resolve effective context strategy (#2288).
        let effective_strategy = match self.memory_state.context_strategy {
            crate::config::ContextStrategy::FullHistory => {
                crate::config::ContextStrategy::FullHistory
            }
            crate::config::ContextStrategy::MemoryFirst => {
                crate::config::ContextStrategy::MemoryFirst
            }
            crate::config::ContextStrategy::Adaptive => {
                if self.sidequest.turn_counter
                    >= u64::from(self.memory_state.crossover_turn_threshold)
                {
                    crate::config::ContextStrategy::MemoryFirst
                } else {
                    crate::config::ContextStrategy::FullHistory
                }
            }
        };
        let memory_first = effective_strategy == crate::config::ContextStrategy::MemoryFirst;

        // Pre-count digest tokens so the budget allocator can deduct them before splits.
        let digest_tokens = self
            .memory_state
            .cached_session_digest
            .as_ref()
            .map_or(0, |(_, tokens)| *tokens);

        let alloc = budget.allocate_with_opts(
            system_prompt,
            &self.skill_state.last_skills_prompt,
            &self.metrics.token_counter,
            graph_enabled,
            digest_tokens,
            memory_first,
        );

        // Remove stale injected messages before concurrent fetch
        self.remove_session_digest_message();
        self.remove_summary_messages();
        self.remove_cross_session_messages();
        self.remove_recall_messages();
        self.remove_document_rag_messages();
        self.remove_correction_messages();
        self.remove_code_context_messages();
        self.remove_graph_facts_messages();

        // Own the query to satisfy Send bounds when agent.run() is spawned
        let query = query.to_owned();

        let correction_params = self
            .learning_engine
            .config
            .as_ref()
            .filter(|c| c.correction_detection)
            .map(|c| {
                (
                    c.correction_recall_limit as usize,
                    c.correction_min_similarity,
                )
            });

        // Fetch all context sources concurrently via FuturesUnordered.
        // All immutable field borrows are scoped to the block below, so they are released
        // before mutable self access (insert, trim, recompute) below.
        let mut summaries_msg: Option<Message> = None;
        let mut cross_session_msg: Option<Message> = None;
        let mut recall_msg: Option<Message> = None;
        let mut recall_confidence: Option<f32> = None;
        let mut doc_rag_msg: Option<Message> = None;
        let mut corrections_msg: Option<Message> = None;
        let mut code_rag_text: Option<String> = None;
        let mut graph_facts_msg: Option<Message> = None;

        {
            type CtxFuture<'a> = Pin<
                Box<
                    dyn Future<Output = Result<ContextSlot, super::super::error::AgentError>>
                        + Send
                        + 'a,
                >,
            >;

            let tc = self.metrics.token_counter.clone();
            let router = self.context_manager.build_router();
            let router_ref: &dyn zeph_memory::AsyncMemoryRouter = router.as_ref();
            let memory_state = &self.memory_state;
            let index = &self.index;

            let (recall_limit, min_sim) = correction_params.unwrap_or((3, 0.75));

            let mut fetchers: FuturesUnordered<CtxFuture<'_>> = FuturesUnordered::new();

            fetchers.push(Box::pin(async {
                Self::fetch_summaries(memory_state, alloc.summaries, &tc)
                    .await
                    .map(ContextSlot::Summaries)
            }));
            fetchers.push(Box::pin(async {
                Self::fetch_cross_session(memory_state, &query, alloc.cross_session, &tc)
                    .await
                    .map(ContextSlot::CrossSession)
            }));
            fetchers.push(Box::pin(async {
                Self::fetch_semantic_recall(
                    memory_state,
                    &query,
                    alloc.semantic_recall,
                    &tc,
                    Some(router_ref),
                )
                .await
                .map(|(msg, score)| ContextSlot::SemanticRecall(msg, score))
            }));
            fetchers.push(Box::pin(async {
                Self::fetch_document_rag(memory_state, &query, alloc.semantic_recall, &tc)
                    .await
                    .map(ContextSlot::DocumentRag)
            }));
            fetchers.push(Box::pin(async {
                Self::fetch_corrections(memory_state, &query, recall_limit, min_sim)
                    .await
                    .map(ContextSlot::Corrections)
            }));
            fetchers.push(Box::pin(async {
                Self::fetch_code_rag(index, &query, alloc.code_context)
                    .await
                    .map(ContextSlot::CodeContext)
            }));
            fetchers.push(Box::pin(async {
                Self::fetch_graph_facts(memory_state, &query, alloc.graph_facts, &tc)
                    .await
                    .map(ContextSlot::GraphFacts)
            }));

            while let Some(result) = fetchers.next().await {
                match result {
                    Ok(slot) => match slot {
                        ContextSlot::Summaries(msg) => summaries_msg = msg,
                        ContextSlot::CrossSession(msg) => cross_session_msg = msg,
                        ContextSlot::SemanticRecall(msg, score) => {
                            recall_msg = msg;
                            recall_confidence = score;
                        }
                        ContextSlot::DocumentRag(msg) => doc_rag_msg = msg,
                        ContextSlot::Corrections(msg) => corrections_msg = msg,
                        ContextSlot::CodeContext(text) => code_rag_text = text,
                        ContextSlot::GraphFacts(msg) => graph_facts_msg = msg,
                    },
                    Err(e) => {
                        // Drop fetchers (releases immutable borrows) before &mut self below
                        drop(fetchers);
                        let _ = self.channel.send_status("").await;
                        return Err(e);
                    }
                }
            }
        }

        // Store top-1 recall score on agent state for MAR routing signal.
        self.memory_state.last_recall_confidence = recall_confidence;

        // MemoryFirst: drain conversation history BEFORE inserting memory messages so that the
        // memory inserts land into the shorter array and are not accidentally removed.
        if memory_first {
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
        if let Some(msg) = graph_facts_msg.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ExternalContent)
                    .await,
            ); // lgtm[rust/cleartext-logging]
            tracing::debug!("injected knowledge graph facts into context");
        }
        if let Some(msg) = doc_rag_msg.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ExternalContent)
                    .await,
            ); // lgtm[rust/cleartext-logging]
            tracing::debug!("injected document RAG context");
        }
        if let Some(msg) = corrections_msg.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ConversationHistory)
                    .await,
            ); // lgtm[rust/cleartext-logging]
            tracing::debug!("injected past corrections into context");
        }
        if let Some(msg) = recall_msg.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::ConversationHistory)
                    .await,
            ); // lgtm[rust/cleartext-logging]
        }
        if let Some(msg) = cross_session_msg.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::LlmSummary)
                    .await,
            ); // lgtm[rust/cleartext-logging]
        }
        if let Some(msg) = summaries_msg.filter(|_| self.msg.messages.len() > 1) {
            self.msg.messages.insert(
                1,
                self.sanitize_memory_message(msg, MemorySourceHint::LlmSummary)
                    .await,
            ); // lgtm[rust/cleartext-logging]
            tracing::debug!("injected summaries into context");
        }

        if let Some(text) = code_rag_text {
            // Sanitize before injection: indexed repo files can contain injection patterns
            // embedded in comments, docstrings, or string literals (ContentSourceKind::ToolResult
            // / LocalUntrusted — local repo, not external).
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

        if !memory_first {
            self.trim_messages_to_budget(alloc.recent_history);
        }

        // Inject session digest AFTER all other memory inserts so it lands at position 1
        // (closest to the system prompt). #2289
        if let Some((digest_text, _)) = self
            .memory_state
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
        let _ = self.channel.send_status("").await;

        Ok(())
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
            .expect("registry read lock")
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
                    if let Some(memory) = &self.memory_state.memory {
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
                scored.retain(|s| s.score >= min_score);

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
            && let Some(memory) = &self.memory_state.memory
        {
            let names: Vec<&str> = skills_to_record.iter().map(String::as_str).collect();
            if let Err(e) = memory.sqlite().record_skill_usage(&names).await {
                tracing::warn!("failed to record skill usage: {e:#}");
            }
        }
        self.update_skill_confidence_metrics().await;

        let (all_skills, active_skills): (Vec<Skill>, Vec<Skill>) = {
            let reg = self
                .skill_state
                .registry
                .read()
                .expect("registry read lock");
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
            &self.memory_state.memory
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
                        self.sync_mcp_executor_tools();
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
                                self.apply_pruned_mcp_tools(selected);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    strict = params.strict,
                                    "semantic tool discovery: query embed failed, falling back to all tools: {e:#}"
                                );
                                if !params.strict {
                                    self.sync_mcp_executor_tools();
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
                            self.sync_mcp_executor_tools();
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
                                self.apply_pruned_mcp_tools(pruned);
                            }
                            Err(e) => {
                                tracing::warn!("MCP pruning failed, using all tools: {e:#}");
                                self.sync_mcp_executor_tools();
                            }
                        }
                    } else {
                        // pruning_enabled=false: pass all tools through.
                        self.sync_mcp_executor_tools();
                    }
                }
                zeph_mcp::ToolDiscoveryStrategy::None => {
                    // Pass all tools through without filtering.
                    self.sync_mcp_executor_tools();
                }
            }
        }

        // Dynamic tool schema filtering (#2020): compute once per turn, cache for native path.
        // Query embedding is computed here; when strategy=Embedding already computed it above,
        // but providers are stateless so a second embed() call is acceptable for MVP.
        self.cached_filtered_tool_ids = None;
        if let Some(ref filter) = self.tool_schema_filter
            && self.provider.supports_tool_use()
        {
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
                    if let Some(ref dep_graph) = self.dependency_graph {
                        let dep_config = &self.runtime.dependency_config;
                        dep_graph.apply(
                            &mut result,
                            &self.completed_tool_ids,
                            dep_config.boost_per_dep,
                            dep_config.max_total_boost,
                            &self.dependency_always_on,
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
                    self.cached_filtered_tool_ids = Some(result.included);
                }
                Err(e) => {
                    tracing::warn!("tool filter: query embed failed, using all tools: {e:#}");
                }
            }
            let _ = self.channel.send_status("").await;
        }

        let tool_catalog = if self.provider.supports_tool_use() {
            // Native tool_use: tools are passed via API, skip prompt-based instructions
            None
        } else {
            let defs = self.tool_executor.tool_definitions_erased();
            if defs.is_empty() {
                None
            } else {
                let reg = zeph_tools::ToolRegistry::from_definitions(defs);
                Some(reg.format_for_prompt_filtered(&self.runtime.permission_policy))
            }
        };
        // BLOCK 1: stable within a session — base prompt + skills + tool catalog
        // Instruction blocks are passed separately and injected in the volatile section.
        #[allow(unused_mut)]
        let mut system_prompt = build_system_prompt_with_instructions(
            &skills_prompt,
            Some(&self.session.env_context),
            tool_catalog.as_deref(),
            self.provider.supports_tool_use(),
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
        if self.completed_tool_ids.contains("memory_save") {
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
            let remaining_tool_calls = max_tool_calls.saturating_sub(self.current_tool_iteration);
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
/// by the `Role::Assistant` message that issued the corresponding `ToolUse`, otherwise the
/// provider returns HTTP 400.
///
/// `history_start` is the index of the first non-system message (typically 1).
fn memory_first_keep_tail(messages: &[Message], history_start: usize) -> usize {
    let mut keep_tail = 2usize;
    let len = messages.len();

    while keep_tail < len.saturating_sub(history_start) {
        let first_retained = &messages[len - keep_tail];
        if first_retained.role == Role::User
            && first_retained
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        {
            keep_tail += 1;
        } else {
            break;
        }
    }

    keep_tail
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::{Message, MessagePart, Role};

    use crate::agent::agent_tests::MockChannel;

    // ── effective_recall_timeout_ms tests (#2514) ────────────────────────────

    #[test]
    fn effective_recall_timeout_ms_nonzero_returns_unchanged() {
        let result = Agent::<MockChannel>::effective_recall_timeout_ms(500);
        assert_eq!(result, 500, "non-zero value must pass through unchanged");
    }

    #[test]
    fn effective_recall_timeout_ms_nonzero_large_returns_unchanged() {
        let result = Agent::<MockChannel>::effective_recall_timeout_ms(5000);
        assert_eq!(result, 5000);
    }

    #[test]
    fn effective_recall_timeout_ms_zero_clamps_to_100() {
        let result = Agent::<MockChannel>::effective_recall_timeout_ms(0);
        assert_eq!(
            result, 100,
            "zero recall_timeout_ms must be clamped to 100ms"
        );
    }

    #[test]
    fn spreading_activation_default_timeout_is_nonzero() {
        // Ensures the default used in production is not accidentally set to zero —
        // which would always trigger the zero-clamp warn path in effective_recall_timeout_ms.
        let result = Agent::<MockChannel>::effective_recall_timeout_ms(
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
            .map(|s| s.to_string())
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
