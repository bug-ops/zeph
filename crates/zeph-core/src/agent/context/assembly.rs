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

use crate::redact::scrub_content;
use zeph_sanitizer::{ContentSource, ContentSourceKind, MemorySourceHint};

#[cfg(feature = "lsp-context")]
use super::super::LSP_NOTE_PREFIX;
use super::super::{
    Agent, CODE_CONTEXT_PREFIX, CORRECTIONS_PREFIX, CROSS_SESSION_PREFIX, DOCUMENT_RAG_PREFIX,
    GRAPH_FACTS_PREFIX, MemoryState, RECALL_PREFIX, SUMMARY_PREFIX, Skill,
    build_system_prompt_with_instructions, format_skills_prompt,
};
use super::ContextSlot;
use crate::channel::Channel;

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
    #[cfg(feature = "lsp-context")]
    pub(in crate::agent) fn remove_lsp_messages(&mut self) {
        self.msg
            .messages
            .retain(|m| m.role != Role::System || !m.content.starts_with(LSP_NOTE_PREFIX));
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
        let max_hops = memory_state.graph_config.max_hops;
        let temporal_decay_rate = memory_state.graph_config.temporal_decay_rate;
        let edge_types = zeph_memory::classify_graph_subgraph(query);
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

        let mut body = String::from(GRAPH_FACTS_PREFIX);
        let mut tokens_so_far = tc.count_tokens(&body);
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

        if let Some(msg) = Self::fetch_semantic_recall(
            &self.memory_state,
            query,
            token_budget,
            &self.metrics.token_counter,
            None,
        )
        .await?
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
        router: Option<&dyn zeph_memory::MemoryRouter>,
    ) -> Result<Option<Message>, super::super::error::AgentError> {
        let Some(memory) = &memory_state.memory else {
            return Ok(None);
        };
        if memory_state.recall_limit == 0 || token_budget == 0 {
            return Ok(None);
        }

        let recalled = if let Some(r) = router {
            memory
                .recall_routed(query, memory_state.recall_limit, None, r)
                .await?
        } else {
            memory
                .recall(query, memory_state.recall_limit, None)
                .await?
        };
        if recalled.is_empty() {
            return Ok(None);
        }

        let mut recall_text = String::with_capacity(token_budget * 3);
        recall_text.push_str(RECALL_PREFIX);
        let mut tokens_used = tc.count_tokens(&recall_text);

        for item in &recalled {
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
            Ok(Some(Message::from_parts(
                Role::System,
                vec![MessagePart::Recall { text: recall_text }],
            )))
        } else {
            Ok(None)
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
        let alloc = budget.allocate(
            system_prompt,
            &self.skill_state.last_skills_prompt,
            &self.metrics.token_counter,
            graph_enabled,
        );

        // Remove stale injected messages before concurrent fetch
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
                    Some(&router),
                )
                .await
                .map(ContextSlot::SemanticRecall)
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
                        ContextSlot::SemanticRecall(msg) => recall_msg = msg,
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

        self.trim_messages_to_budget(alloc.recent_history);

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
            let provider = self.provider.clone();
            let _ = self.channel.send_status("matching skills...").await;
            let mut scored = matcher
                .match_skills(
                    &all_meta,
                    query,
                    self.skill_state.max_active_skills,
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
            }

            let indices: Vec<usize> = if scored.is_empty() {
                // Embed or Qdrant failure: fall back to all skills so the agent
                // remains functional rather than running with an empty skill set.
                tracing::warn!("skill matcher returned no results, falling back to all skills");
                (0..all_meta.len()).collect()
            } else {
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
                .collect();
            let active: Vec<Skill> = self
                .skill_state
                .active_skill_names
                .iter()
                .filter_map(|name| reg.get_skill(name).ok())
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
            zeph_tools::TrustLevel::Trusted
        } else {
            self.skill_state
                .active_skill_names
                .iter()
                .filter_map(|name| trust_map.get(name).copied())
                .fold(zeph_tools::TrustLevel::Trusted, |acc, lvl| {
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

        let skills_prompt = if effective_mode == crate::config::SkillPromptMode::Compact {
            format_skills_prompt_compact(&active_skills)
        } else {
            format_skills_prompt(&active_skills, &trust_map, &health_map)
        };
        let catalog_prompt = format_skills_catalog(&remaining_skills);
        self.skill_state
            .last_skills_prompt
            .clone_from(&skills_prompt);
        self.session.env_context.refresh_git_branch();
        self.session
            .env_context
            .model_name
            .clone_from(&self.runtime.model_name);

        // Dynamic tool schema filtering (#2020): compute once per turn, cache for native path.
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
            match self.provider.embed(query).await {
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
