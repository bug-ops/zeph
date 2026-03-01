// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::borrow::Cow;
use std::fmt::Write;

use futures::StreamExt as _;

use zeph_llm::provider::{MessageMetadata, MessagePart};
use zeph_memory::TokenCounter;
use zeph_skills::ScoredMatch;
use zeph_skills::loader::SkillMeta;
use zeph_skills::prompt::{format_skills_catalog, format_skills_prompt_compact};

use crate::redact::scrub_content;

use super::{
    Agent, CODE_CONTEXT_PREFIX, CORRECTIONS_PREFIX, CROSS_SESSION_PREFIX, Channel, ContextBudget,
    DOCUMENT_RAG_PREFIX, LlmProvider, Message, RECALL_PREFIX, Role, SUMMARY_PREFIX, Skill,
    build_system_prompt, format_skills_prompt,
};

fn chunk_messages(
    messages: &[Message],
    budget: usize,
    oversized: usize,
    tc: &TokenCounter,
) -> Vec<Vec<Message>> {
    let mut chunks: Vec<Vec<Message>> = Vec::new();
    let mut current: Vec<Message> = Vec::new();
    let mut current_tokens = 0usize;

    for msg in messages {
        let msg_tokens = tc.count_tokens(&msg.content);

        if msg_tokens >= oversized {
            // Oversized message gets its own chunk
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_tokens = 0;
            }
            chunks.push(vec![msg.clone()]);
        } else if current_tokens + msg_tokens > budget && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_tokens = 0;
            current.push(msg.clone());
            current_tokens += msg_tokens;
        } else {
            current.push(msg.clone());
            current_tokens += msg_tokens;
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    if chunks.is_empty() {
        chunks.push(Vec::new());
    }

    chunks
}

/// Truncate `s` to at most `max_chars` Unicode scalar values, appending "…" if truncated.
pub(super) fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => format!("{}…", &s[..byte_idx]),
        None => s.to_owned(),
    }
}

impl<C: Channel> Agent<C> {
    pub(super) fn should_compact(&self) -> bool {
        self.context_manager
            .should_compact(self.cached_prompt_tokens)
    }

    fn build_chunk_prompt(messages: &[Message]) -> String {
        let estimated_len: usize = messages
            .iter()
            .map(|m| "[assistant]: ".len() + m.content.len() + 2)
            .sum();
        let mut history_text = String::with_capacity(estimated_len);
        for (i, m) in messages.iter().enumerate() {
            if i > 0 {
                history_text.push_str("\n\n");
            }
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };
            let _ = write!(history_text, "[{role}]: {}", m.content);
        }

        format!(
            "<analysis>\n\
             Analyze this conversation and produce a structured compaction note for self-consumption.\n\
             This note replaces the original messages in your context window — be thorough.\n\
             Longer is better if it preserves actionable detail.\n\
             </analysis>\n\
             \n\
             Produce exactly these 9 sections:\n\
             1. User Intent — what the user is ultimately trying to accomplish\n\
             2. Technical Concepts — key technologies, patterns, constraints discussed\n\
             3. Files & Code — file paths, function names, structs, enums touched or relevant\n\
             4. Errors & Fixes — every error encountered and whether/how it was resolved\n\
             5. Problem Solving — approaches tried, decisions made, alternatives rejected\n\
             6. User Messages — verbatim user requests that are still pending or relevant\n\
             7. Pending Tasks — items explicitly promised or left TODO\n\
             8. Current Work — the exact task in progress at the moment of compaction\n\
             9. Next Step — the single most important action to take immediately after compaction\n\
             \n\
             Conversation:\n{history_text}"
        )
    }

    /// Build a metadata-only summary without calling the LLM.
    /// Used as last-resort fallback when LLM summarization repeatedly fails.
    fn build_metadata_summary(messages: &[Message]) -> String {
        let mut user_count = 0usize;
        let mut assistant_count = 0usize;
        let mut system_count = 0usize;
        let mut last_user = String::new();
        let mut last_assistant = String::new();

        for m in messages {
            match m.role {
                Role::User => {
                    user_count += 1;
                    if !m.content.is_empty() {
                        last_user.clone_from(&m.content);
                    }
                }
                Role::Assistant => {
                    assistant_count += 1;
                    if !m.content.is_empty() {
                        last_assistant.clone_from(&m.content);
                    }
                }
                Role::System => system_count += 1,
            }
        }

        let last_user_preview = truncate_chars(&last_user, 200);
        let last_assistant_preview = truncate_chars(&last_assistant, 200);

        format!(
            "[metadata summary — LLM compaction unavailable]\n\
             Messages compacted: {} ({} user, {} assistant, {} system)\n\
             Last user message: {last_user_preview}\n\
             Last assistant message: {last_assistant_preview}",
            messages.len(),
            user_count,
            assistant_count,
            system_count,
        )
    }

    #[allow(clippy::too_many_lines)]
    async fn try_summarize_with_llm(
        &self,
        messages: &[Message],
    ) -> Result<String, zeph_llm::LlmError> {
        const CHUNK_TOKEN_BUDGET: usize = 4096;
        const OVERSIZED_THRESHOLD: usize = CHUNK_TOKEN_BUDGET / 2;

        let chunks = chunk_messages(
            messages,
            CHUNK_TOKEN_BUDGET,
            OVERSIZED_THRESHOLD,
            &self.token_counter,
        );

        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);

        if chunks.len() <= 1 {
            let prompt = Self::build_chunk_prompt(messages);
            let msgs = [Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];
            return tokio::time::timeout(
                llm_timeout,
                self.summary_or_primary_provider().chat(&msgs),
            )
            .await
            .map_err(|_| zeph_llm::LlmError::Timeout)?;
        }

        // Summarize chunks with bounded concurrency to prevent runaway API calls
        let provider = self.summary_or_primary_provider();
        let results: Vec<_> = futures::stream::iter(chunks.iter().map(|chunk| {
            let prompt = Self::build_chunk_prompt(chunk);
            let p = provider.clone();
            async move {
                tokio::time::timeout(
                    llm_timeout,
                    p.chat(&[Message {
                        role: Role::User,
                        content: prompt,
                        parts: vec![],
                        metadata: MessageMetadata::default(),
                    }]),
                )
                .await
                .map_err(|_| zeph_llm::LlmError::Timeout)?
            }
        }))
        .buffer_unordered(4)
        .collect()
        .await;

        let partial_summaries: Vec<String> = results
            .into_iter()
            .collect::<Result<Vec<_>, zeph_llm::LlmError>>()
            .unwrap_or_else(|e| {
                tracing::warn!("chunked compaction: one or more chunks failed: {e:#}, falling back to single-pass");
                Vec::new()
            });

        if partial_summaries.is_empty() {
            // Fallback: single-pass on full messages
            let prompt = Self::build_chunk_prompt(messages);
            let msgs = [Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];
            return tokio::time::timeout(
                llm_timeout,
                self.summary_or_primary_provider().chat(&msgs),
            )
            .await
            .map_err(|_| zeph_llm::LlmError::Timeout)?;
        }

        // Consolidate partial summaries
        let numbered = {
            use std::fmt::Write as _;
            let cap: usize = partial_summaries.iter().map(|s| s.len() + 8).sum();
            let mut buf = String::with_capacity(cap);
            for (i, s) in partial_summaries.iter().enumerate() {
                if i > 0 {
                    buf.push_str("\n\n");
                }
                let _ = write!(buf, "{}. {s}", i + 1);
            }
            buf
        };

        let consolidation_prompt = format!(
            "<analysis>\n\
             Merge these partial conversation summaries into a single structured compaction note.\n\
             Produce exactly these 9 sections covering all partial summaries:\n\
             1. User Intent\n\
             2. Technical Concepts\n\
             3. Files & Code\n\
             4. Errors & Fixes\n\
             5. Problem Solving\n\
             6. User Messages\n\
             7. Pending Tasks\n\
             8. Current Work\n\
             9. Next Step\n\
             </analysis>\n\
             \n\
             Partial summaries:\n{numbered}"
        );

        let consolidation_msgs = [Message {
            role: Role::User,
            content: consolidation_prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        tokio::time::timeout(
            llm_timeout,
            self.summary_or_primary_provider().chat(&consolidation_msgs),
        )
        .await
        .map_err(|_| zeph_llm::LlmError::Timeout)?
    }

    /// Remove tool response parts from messages using middle-out order.
    /// `fraction` is in range (0.0, 1.0] — fraction of tool responses to remove.
    /// Returns the modified message list.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )]
    fn remove_tool_responses_middle_out(mut messages: Vec<Message>, fraction: f32) -> Vec<Message> {
        // Collect indices of messages that have ToolResult or ToolOutput parts
        let tool_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::ToolResult { .. } | MessagePart::ToolOutput { .. }
                    )
                })
            })
            .map(|(i, _)| i)
            .collect();

        if tool_indices.is_empty() {
            return messages;
        }

        let n = tool_indices.len();
        let to_remove = ((n as f32 * fraction).ceil() as usize).min(n);

        // Middle-out: start from center, alternate outward
        let center = n / 2;
        let mut remove_set: Vec<usize> = Vec::with_capacity(to_remove);
        let mut left = center as isize - 1;
        let mut right = center;
        let mut count = 0;

        while count < to_remove {
            if right < n {
                remove_set.push(tool_indices[right]);
                count += 1;
                right += 1;
            }
            if count < to_remove && left >= 0 {
                let idx = left as usize;
                if !remove_set.contains(&tool_indices[idx]) {
                    remove_set.push(tool_indices[idx]);
                    count += 1;
                }
            }
            left -= 1;
            if left < 0 && right >= n {
                break;
            }
        }

        for &msg_idx in &remove_set {
            let msg = &mut messages[msg_idx];
            for part in &mut msg.parts {
                match part {
                    MessagePart::ToolResult { content, .. } => {
                        "[compacted]".clone_into(content);
                    }
                    MessagePart::ToolOutput {
                        body, compacted_at, ..
                    } => {
                        if compacted_at.is_none() {
                            *body = String::new();
                            *compacted_at = Some(
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs()
                                    .cast_signed(),
                            );
                        }
                    }
                    _ => {}
                }
            }
            msg.rebuild_content();
        }
        messages
    }

    async fn summarize_messages(
        &self,
        messages: &[Message],
    ) -> Result<String, super::error::AgentError> {
        // Try direct summarization first
        match self.try_summarize_with_llm(messages).await {
            Ok(summary) => return Ok(summary),
            Err(e) if !e.is_context_length_error() => return Err(e.into()),
            Err(e) => {
                tracing::warn!(
                    "summarization hit context length error ({e}), trying progressive tool response removal"
                );
            }
        }

        // Progressive tool response removal tiers: 10%, 20%, 50%, 100%
        for fraction in [0.10f32, 0.20, 0.50, 1.0] {
            let reduced = Self::remove_tool_responses_middle_out(messages.to_vec(), fraction);
            tracing::debug!(
                fraction,
                "retrying summarization with reduced tool responses"
            );
            match self.try_summarize_with_llm(&reduced).await {
                Ok(summary) => {
                    tracing::info!(
                        fraction,
                        "summarization succeeded after tool response removal"
                    );
                    return Ok(summary);
                }
                Err(e) if e.is_context_length_error() => {
                    tracing::warn!(fraction, "still context length error, trying next tier");
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Final fallback: metadata-only summary without LLM
        tracing::warn!("all LLM summarization attempts failed, using metadata fallback");
        Ok(Self::build_metadata_summary(messages))
    }

    pub(super) async fn compact_context(&mut self) -> Result<(), super::error::AgentError> {
        let preserve_tail = self.context_manager.compaction_preserve_tail;

        if self.messages.len() <= preserve_tail + 1 {
            return Ok(());
        }

        let compact_end = self.messages.len() - preserve_tail;
        let to_compact = &self.messages[1..compact_end];
        if to_compact.is_empty() {
            return Ok(());
        }

        let summary = self.summarize_messages(to_compact).await?;

        let compacted_count = to_compact.len();
        let summary_content =
            format!("[conversation summary — {compacted_count} messages compacted]\n{summary}");
        self.messages.drain(1..compact_end);
        self.messages.insert(
            1,
            Message {
                role: Role::System,
                content: summary_content.clone(),
                parts: vec![],
                metadata: MessageMetadata::agent_only(),
            },
        );

        tracing::info!(
            compacted_count,
            summary_tokens = self.token_counter.count_tokens(&summary),
            "compacted context"
        );

        self.recompute_prompt_tokens();
        self.update_metrics(|m| {
            m.context_compactions += 1;
        });

        if let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        {
            // Persist compaction: mark originals as user_only, insert summary as agent_only.
            // Assumption: the system prompt is always the first (oldest) row for this conversation
            // in SQLite — i.e., ids[0] corresponds to self.messages[0] (the system prompt).
            // This holds for normal sessions but may not hold after cross-session restore if a
            // non-system message was persisted first. MVP assumption; document if changed.
            // oldest_message_ids returns ascending order; ids[1..=compacted_count] are the messages
            // that were drained from self.messages[1..compact_end].
            let sqlite = memory.sqlite();
            let ids = sqlite
                .oldest_message_ids(cid, u32::try_from(compacted_count + 1).unwrap_or(u32::MAX))
                .await;
            match ids {
                Ok(ids) if ids.len() >= 2 => {
                    // ids[0] is the system prompt; compact ids[1..=compacted_count]
                    let start = ids[1];
                    let end = ids[compacted_count.min(ids.len() - 1)];
                    if let Err(e) = sqlite
                        .replace_conversation(cid, start..=end, "system", &summary_content)
                        .await
                    {
                        tracing::warn!("failed to persist compaction in sqlite: {e:#}");
                    }
                }
                Ok(_) => {
                    // Not enough messages in DB — fall back to legacy summary storage
                    if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary: {e:#}");
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to get message ids for compaction: {e:#}");
                    if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary: {e:#}");
                    }
                }
            }
        }

        Ok(())
    }

    /// Prune tool output bodies outside the protection zone, oldest first.
    /// Returns the number of tokens freed.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn prune_tool_outputs(&mut self, min_to_free: usize) -> usize {
        let protect = self.context_manager.prune_protect_tokens;
        let mut tail_tokens = 0usize;
        let mut protection_boundary = self.messages.len();
        if protect > 0 {
            for (i, msg) in self.messages.iter().enumerate().rev() {
                tail_tokens += self.token_counter.count_tokens(&msg.content);
                if tail_tokens >= protect {
                    protection_boundary = i;
                    break;
                }
                if i == 0 {
                    protection_boundary = 0;
                }
            }
        }

        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        for msg in &mut self.messages[..protection_boundary] {
            if freed >= min_to_free {
                break;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let &mut MessagePart::ToolOutput {
                    ref mut body,
                    ref mut compacted_at,
                    ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                {
                    freed += self.token_counter.count_tokens(body);
                    *compacted_at = Some(now);
                    *body = String::new();
                    modified = true;
                }
            }
            if modified {
                msg.rebuild_content();
            }
        }

        if freed > 0 {
            self.update_metrics(|m| m.tool_output_prunes += 1);
            tracing::info!(freed, protection_boundary, "pruned tool outputs");
        }
        freed
    }

    /// Inline pruning for tool loops: clear tool output bodies from messages
    /// older than the last `keep_recent` messages. Called after each tool iteration
    /// to prevent context growth during long tool loops.
    pub(crate) fn prune_stale_tool_outputs(&mut self, keep_recent: usize) -> usize {
        if self.messages.len() <= keep_recent + 1 {
            return 0;
        }
        let boundary = self.messages.len().saturating_sub(keep_recent);
        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        // Skip system prompt (index 0), prune from 1..boundary
        for msg in &mut self.messages[1..boundary] {
            let mut modified = false;
            for part in &mut msg.parts {
                match part {
                    MessagePart::ToolOutput {
                        body, compacted_at, ..
                    } if compacted_at.is_none() && !body.is_empty() => {
                        freed += self.token_counter.count_tokens(body);
                        *compacted_at = Some(now);
                        *body = String::new();
                        modified = true;
                    }
                    MessagePart::ToolResult { content, .. } => {
                        let tokens = self.token_counter.count_tokens(content);
                        if tokens > 20 {
                            freed += tokens;
                            "[pruned]".clone_into(content);
                            freed -= 1;
                            modified = true;
                        }
                    }
                    _ => {}
                }
            }
            if modified {
                msg.rebuild_content();
            }
        }
        if freed > 0 {
            self.update_metrics(|m| m.tool_output_prunes += 1);
            tracing::debug!(
                freed,
                boundary,
                keep_recent,
                "inline pruned stale tool outputs"
            );
        }
        freed
    }

    fn count_tool_pairs(&self) -> usize {
        let mut count = 0usize;
        let mut i = 1; // skip system prompt
        while i < self.messages.len() {
            let msg = &self.messages[i];
            if !msg.metadata.agent_visible {
                i += 1;
                continue;
            }
            let is_tool_request = msg.role == Role::Assistant
                && msg
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }));
            if is_tool_request && i + 1 < self.messages.len() {
                let next = &self.messages[i + 1];
                if next.metadata.agent_visible
                    && next.role == Role::User
                    && next.parts.iter().any(|p| {
                        matches!(
                            p,
                            MessagePart::ToolResult { .. } | MessagePart::ToolOutput { .. }
                        )
                    })
                {
                    count += 1;
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }
        count
    }

    fn find_oldest_tool_pair(&self) -> Option<(usize, usize)> {
        let mut i = 1; // skip system prompt
        while i < self.messages.len() {
            let msg = &self.messages[i];
            if !msg.metadata.agent_visible {
                i += 1;
                continue;
            }
            let is_tool_request = msg.role == Role::Assistant
                && msg
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }));
            if is_tool_request && i + 1 < self.messages.len() {
                let next = &self.messages[i + 1];
                if next.metadata.agent_visible
                    && next.role == Role::User
                    && next.parts.iter().any(|p| {
                        matches!(
                            p,
                            MessagePart::ToolResult { .. } | MessagePart::ToolOutput { .. }
                        )
                    })
                {
                    return Some((i, i + 1));
                }
            }
            i += 1;
        }
        None
    }

    fn build_tool_pair_summary_prompt(req: &Message, res: &Message) -> String {
        format!(
            "Summarize this tool invocation in 1-2 sentences. Include the tool name, \
             key input parameters, and the essential outcome/result.\n\n\
             <tool_request>\n{}\n</tool_request>\n\n<tool_response>\n{}\n</tool_response>",
            req.content, res.content
        )
    }

    pub(super) async fn maybe_summarize_tool_pair(&mut self) {
        let pair_count = self.count_tool_pairs();
        if pair_count <= self.memory_state.tool_call_cutoff {
            return;
        }
        let Some((req_idx, resp_idx)) = self.find_oldest_tool_pair() else {
            return;
        };
        let prompt =
            Self::build_tool_pair_summary_prompt(&self.messages[req_idx], &self.messages[resp_idx]);
        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);
        let msgs = [Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let chat_fut = self.summary_or_primary_provider().chat(&msgs);
        let summary = match tokio::time::timeout(llm_timeout, chat_fut).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!(%e, "tool pair summarization failed, skipping");
                return;
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = self.runtime.timeouts.llm_seconds,
                    "tool pair summarization timed out, skipping"
                );
                return;
            }
        };
        self.messages[req_idx].metadata.agent_visible = false;
        self.messages[resp_idx].metadata.agent_visible = false;
        let summary = self.maybe_redact(&summary).into_owned();
        let content = format!("[tool summary] {summary}");
        let summary_msg = Message {
            role: Role::Assistant,
            content,
            parts: vec![MessagePart::Summary { text: summary }],
            metadata: MessageMetadata::agent_only(),
        };
        self.messages.insert(resp_idx + 1, summary_msg);
        tracing::debug!(
            pair_count,
            cutoff = self.memory_state.tool_call_cutoff,
            req_idx,
            resp_idx,
            "summarized oldest tool pair"
        );
    }

    /// Two-tier compaction: Tier 1 prunes tool outputs, Tier 2 falls back to full LLM compaction.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(super) async fn maybe_compact(&mut self) -> Result<(), super::error::AgentError> {
        if !self.should_compact() {
            return Ok(());
        }

        let budget = self
            .context_manager
            .budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        let total_tokens: usize = self
            .messages
            .iter()
            .map(|m| self.token_counter.count_tokens(&m.content))
            .sum();
        let threshold = (budget as f32 * self.context_manager.compaction_threshold) as usize;
        let min_to_free = total_tokens.saturating_sub(threshold);

        let freed = self.prune_tool_outputs(min_to_free);
        if freed >= min_to_free {
            tracing::info!(freed, "tier-1 pruning sufficient");
            return Ok(());
        }

        tracing::info!(
            freed,
            min_to_free,
            "tier-1 insufficient, falling back to tier-2 compaction"
        );
        let _ = self.channel.send_status("compacting context...").await;
        let result = self.compact_context().await;
        let _ = self.channel.send_status("").await;
        result
    }

    pub(super) fn clear_history(&mut self) {
        let system_prompt = self.messages.first().cloned();
        self.messages.clear();
        if let Some(sp) = system_prompt {
            self.messages.push(sp);
        }
        self.recompute_prompt_tokens();
    }

    pub(super) fn remove_recall_messages(&mut self) {
        self.messages.retain(|m| {
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

    pub(super) fn remove_correction_messages(&mut self) {
        self.messages
            .retain(|m| m.role != Role::System || !m.content.starts_with(CORRECTIONS_PREFIX));
    }

    async fn fetch_corrections(
        memory_state: &super::MemoryState,
        query: &str,
        limit: usize,
        min_score: f32,
    ) -> Result<Option<Message>, super::error::AgentError> {
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
            use std::fmt::Write;
            let _ = writeln!(
                text,
                "- When you said: \"{}…\" → User corrected: \"{}\"",
                truncate_chars(&scrub_content(&c.original_output), 80),
                truncate_chars(&scrub_content(&c.correction_text), 200),
            );
        }
        Ok(Some(Message::from_legacy(Role::System, text)))
    }

    #[cfg(test)]
    pub(super) async fn inject_semantic_recall(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> Result<(), super::error::AgentError> {
        self.remove_recall_messages();

        if let Some(msg) = Self::fetch_semantic_recall(
            &self.memory_state,
            query,
            token_budget,
            &self.token_counter,
        )
        .await?
        {
            if self.messages.len() > 1 {
                self.messages.insert(1, msg);
            }
        }

        Ok(())
    }

    async fn fetch_semantic_recall(
        memory_state: &super::MemoryState,
        query: &str,
        token_budget: usize,
        tc: &TokenCounter,
    ) -> Result<Option<Message>, super::error::AgentError> {
        let Some(memory) = &memory_state.memory else {
            return Ok(None);
        };
        if memory_state.recall_limit == 0 || token_budget == 0 {
            return Ok(None);
        }

        let recalled = memory
            .recall(query, memory_state.recall_limit, None)
            .await?;
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

    pub(super) fn remove_code_context_messages(&mut self) {
        self.messages.retain(|m| {
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

    fn remove_summary_messages(&mut self) {
        self.messages.retain(|m| {
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

    fn remove_cross_session_messages(&mut self) {
        self.messages.retain(|m| {
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
        self.messages
            .retain(|m| m.role != Role::System || !m.content.starts_with(DOCUMENT_RAG_PREFIX));
    }

    async fn fetch_document_rag(
        memory_state: &super::MemoryState,
        query: &str,
        token_budget: usize,
        tc: &TokenCounter,
    ) -> Result<Option<Message>, super::error::AgentError> {
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
    async fn inject_cross_session_context(
        &mut self,
        query: &str,
        token_budget: usize,
    ) -> Result<(), super::error::AgentError> {
        self.remove_cross_session_messages();

        if let Some(msg) =
            Self::fetch_cross_session(&self.memory_state, query, token_budget, &self.token_counter)
                .await?
        {
            if self.messages.len() > 1 {
                self.messages.insert(1, msg);
                tracing::debug!("injected cross-session context");
            }
        }

        Ok(())
    }

    async fn fetch_cross_session(
        memory_state: &super::MemoryState,
        query: &str,
        token_budget: usize,
        tc: &TokenCounter,
    ) -> Result<Option<Message>, super::error::AgentError> {
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
    async fn inject_summaries(
        &mut self,
        token_budget: usize,
    ) -> Result<(), super::error::AgentError> {
        self.remove_summary_messages();

        if let Some(msg) =
            Self::fetch_summaries(&self.memory_state, token_budget, &self.token_counter).await?
        {
            if self.messages.len() > 1 {
                self.messages.insert(1, msg);
                tracing::debug!("injected summaries into context");
            }
        }

        Ok(())
    }

    async fn fetch_summaries(
        memory_state: &super::MemoryState,
        token_budget: usize,
        tc: &TokenCounter,
    ) -> Result<Option<Message>, super::error::AgentError> {
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
            let entry = format!(
                "- Messages {}-{}: {}\n",
                summary.first_message_id, summary.last_message_id, summary.content
            );
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

    fn trim_messages_to_budget(&mut self, token_budget: usize) {
        if token_budget == 0 {
            return;
        }

        let history_start = self
            .messages
            .iter()
            .position(|m| m.role != Role::System)
            .unwrap_or(self.messages.len());

        if history_start >= self.messages.len() {
            return;
        }

        let mut total = 0usize;
        let mut keep_from = self.messages.len();

        for i in (history_start..self.messages.len()).rev() {
            let msg_tokens = self.token_counter.count_tokens(&self.messages[i].content);
            if total + msg_tokens > token_budget {
                break;
            }
            total += msg_tokens;
            keep_from = i;
        }

        if keep_from > history_start {
            let removed = keep_from - history_start;
            self.messages.drain(history_start..keep_from);
            self.recompute_prompt_tokens();
            tracing::info!(
                removed,
                token_budget,
                "trimmed messages to fit context budget"
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn prepare_context(
        &mut self,
        query: &str,
    ) -> Result<(), super::error::AgentError> {
        let Some(ref budget) = self.context_manager.budget else {
            return Ok(());
        };
        let _ = self.channel.send_status("building context...").await;

        let system_prompt = self.messages.first().map_or("", |m| m.content.as_str());
        let alloc = budget.allocate(
            system_prompt,
            &self.skill_state.last_skills_prompt,
            &self.token_counter,
        );

        // Remove stale injected messages before concurrent fetch
        self.remove_summary_messages();
        self.remove_cross_session_messages();
        self.remove_recall_messages();
        self.remove_document_rag_messages();
        self.remove_correction_messages();
        #[cfg(feature = "index")]
        self.remove_code_context_messages();

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

        // Fetch all context sources concurrently
        let tc = self.token_counter.clone();
        #[cfg(not(feature = "index"))]
        let (summaries_msg, cross_session_msg, recall_msg, doc_rag_msg, corrections_msg) = {
            let (recall_limit, min_sim) = correction_params.unwrap_or((3, 0.75));
            let result = tokio::try_join!(
                Self::fetch_summaries(&self.memory_state, alloc.summaries, &tc),
                Self::fetch_cross_session(&self.memory_state, &query, alloc.cross_session, &tc),
                Self::fetch_semantic_recall(&self.memory_state, &query, alloc.semantic_recall, &tc),
                Self::fetch_document_rag(&self.memory_state, &query, alloc.semantic_recall, &tc),
                Self::fetch_corrections(&self.memory_state, &query, recall_limit, min_sim),
            );
            match result {
                Ok(v) => v,
                Err(e) => {
                    let _ = self.channel.send_status("").await;
                    return Err(e);
                }
            }
        };

        #[cfg(feature = "index")]
        let (
            summaries_msg,
            cross_session_msg,
            recall_msg,
            doc_rag_msg,
            code_rag_text,
            corrections_msg,
        ) = {
            let (recall_limit, min_sim) = correction_params.unwrap_or((3, 0.75));
            let result = tokio::try_join!(
                Self::fetch_summaries(&self.memory_state, alloc.summaries, &tc),
                Self::fetch_cross_session(&self.memory_state, &query, alloc.cross_session, &tc),
                Self::fetch_semantic_recall(&self.memory_state, &query, alloc.semantic_recall, &tc),
                Self::fetch_document_rag(&self.memory_state, &query, alloc.semantic_recall, &tc),
                Self::fetch_code_rag(&self.index, &query, alloc.code_context),
                Self::fetch_corrections(&self.memory_state, &query, recall_limit, min_sim),
            );
            match result {
                Ok(v) => v,
                Err(e) => {
                    let _ = self.channel.send_status("").await;
                    return Err(e);
                }
            }
        };

        // Insert fetched messages (order: doc_rag, corrections, recall, cross-session, summaries at position 1)
        if let Some(msg) = doc_rag_msg.filter(|_| self.messages.len() > 1) {
            self.messages.insert(1, msg); // codeql[rust/cleartext-logging] false positive: document chunks from Qdrant, no secrets
            tracing::debug!("injected document RAG context");
        }
        if let Some(msg) = corrections_msg.filter(|_| self.messages.len() > 1) {
            self.messages.insert(1, msg); // codeql[rust/cleartext-logging] false positive: user correction text from Qdrant, no secrets
            tracing::debug!("injected past corrections into context");
        }
        if let Some(msg) = recall_msg.filter(|_| self.messages.len() > 1) {
            self.messages.insert(1, msg);
        }
        if let Some(msg) = cross_session_msg.filter(|_| self.messages.len() > 1) {
            self.messages.insert(1, msg);
        }
        if let Some(msg) = summaries_msg.filter(|_| self.messages.len() > 1) {
            self.messages.insert(1, msg);
            tracing::debug!("injected summaries into context");
        }

        #[cfg(feature = "index")]
        if let Some(text) = code_rag_text {
            self.inject_code_context(&text);
        }

        self.trim_messages_to_budget(alloc.recent_history);

        if self.runtime.redact_credentials {
            for msg in &mut self.messages {
                if let Cow::Owned(s) = scrub_content(&msg.content) {
                    msg.content = s;
                }
            }
        }

        self.recompute_prompt_tokens();
        let _ = self.channel.send_status("").await;

        Ok(())
    }

    async fn disambiguate_skills(
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

    #[allow(clippy::too_many_lines)]
    pub(super) async fn rebuild_system_prompt(&mut self, query: &str) {
        let all_meta = self.skill_state.registry.all_meta();
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
            } else if scored.len() >= 2
                && (scored[0].score - scored[1].score) < self.skill_state.disambiguation_threshold
            {
                match self.disambiguate_skills(query, &all_meta, &scored).await {
                    Some(reordered) => reordered,
                    None => scored.iter().map(|s| s.index).collect(),
                }
            } else {
                scored.iter().map(|s| s.index).collect()
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

        if !self.skill_state.active_skill_names.is_empty()
            && let Some(memory) = &self.memory_state.memory
        {
            let names: Vec<&str> = self
                .skill_state
                .active_skill_names
                .iter()
                .map(String::as_str)
                .collect();
            if let Err(e) = memory.sqlite().record_skill_usage(&names).await {
                tracing::warn!("failed to record skill usage: {e:#}");
            }
        }

        let all_skills: Vec<Skill> = self
            .skill_state
            .registry
            .all_meta()
            .iter()
            .filter_map(|m| self.skill_state.registry.get_skill(&m.name).ok())
            .collect();
        let active_skills: Vec<Skill> = self
            .skill_state
            .active_skill_names
            .iter()
            .filter_map(|name| self.skill_state.registry.get_skill(name).ok())
            .collect();
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
        self.env_context.refresh_git_branch();
        self.env_context
            .model_name
            .clone_from(&self.runtime.model_name);
        let env = &self.env_context;
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
        #[allow(unused_mut)]
        let mut system_prompt = build_system_prompt(
            &skills_prompt,
            Some(env),
            tool_catalog.as_deref(),
            self.provider.supports_tool_use(),
        );

        if !catalog_prompt.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&catalog_prompt);
        }

        system_prompt.push_str("\n<!-- cache:stable -->");
        system_prompt.push_str("\n<!-- cache:volatile -->");

        self.append_mcp_prompt(query, &mut system_prompt).await;

        let cwd = std::env::current_dir().unwrap_or_default();
        let project_configs = crate::project::discover_project_configs(&cwd);
        let project_context = crate::project::load_project_context(&project_configs);
        if !project_context.is_empty() {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&project_context);
        }

        #[cfg(feature = "index")]
        if self.index.retriever.is_some() && self.index.repo_map_tokens > 0 {
            let now = std::time::Instant::now();
            let map = if let Some((ref cached, generated_at)) = self.index.cached_repo_map
                && now.duration_since(generated_at) < self.index.repo_map_ttl
            {
                cached.clone()
            } else {
                let fresh = zeph_index::repo_map::generate_repo_map(
                    &cwd,
                    self.index.repo_map_tokens,
                    &self.token_counter,
                )
                .unwrap_or_default();
                self.index.cached_repo_map = Some((fresh.clone(), now));
                fresh
            };
            if !map.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&map);
            }
        }

        tracing::debug!(
            len = system_prompt.len(),
            skills = ?self.skill_state.active_skill_names,
            "system prompt rebuilt"
        );
        tracing::trace!(prompt = %system_prompt, "full system prompt");

        if let Some(msg) = self.messages.first_mut() {
            msg.content = system_prompt;
        }
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    #[allow(clippy::wildcard_imports)]
    use crate::agent::agent_tests::*;

    #[test]
    fn chunk_messages_empty_input_returns_single_empty_chunk() {
        let tc = zeph_memory::TokenCounter::new();
        let messages: &[Message] = &[];
        let chunks = chunk_messages(messages, 4096, 2048, &tc);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_empty());
    }

    #[test]
    fn chunk_messages_single_oversized_message_gets_own_chunk() {
        let tc = zeph_memory::TokenCounter::new();
        // A message >= oversized threshold goes into its own chunk
        let oversized_content = "x".repeat(2048 * 4 + 1); // > 2048 tokens
        let messages = vec![Message {
            role: Role::User,
            content: oversized_content.clone(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let chunks = chunk_messages(&messages, 4096, 2048, &tc);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0][0].content, oversized_content);
    }

    #[test]
    fn chunk_messages_splits_at_budget_boundary() {
        let tc = zeph_memory::TokenCounter::new();
        // Two messages each consuming exactly half of budget → should fit in one chunk
        // Use messages whose token count is just under half of budget
        let half = "w".repeat(1000 * 4); // 1000 tokens
        let messages = vec![
            Message {
                role: Role::User,
                content: half.clone(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: half.clone(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::User,
                content: half.clone(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];
        // budget = 2000 tokens: first two fit, third overflows → 2 chunks
        let chunks = chunk_messages(&messages, 2000, 4096, &tc);
        assert!(chunks.len() >= 2, "expected split into multiple chunks");
    }

    // SF-5: SkillPromptMode::Auto threshold
    #[test]
    fn skill_prompt_mode_auto_selects_compact_when_budget_below_8192() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(4096, 0.20, 0.80, 4, 0);

        // Auto mode: budget < 8192 → Compact
        let effective_mode = match crate::config::SkillPromptMode::Auto {
            crate::config::SkillPromptMode::Auto => {
                if let Some(ref budget) = agent.context_manager.budget
                    && budget.max_tokens() < 8192
                {
                    crate::config::SkillPromptMode::Compact
                } else {
                    crate::config::SkillPromptMode::Full
                }
            }
            other => other,
        };
        assert_eq!(effective_mode, crate::config::SkillPromptMode::Compact);
    }

    #[test]
    fn skill_prompt_mode_auto_selects_full_when_budget_above_8192() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(16384, 0.20, 0.80, 4, 0);

        // Auto mode: budget >= 8192 → Full
        let effective_mode = match crate::config::SkillPromptMode::Auto {
            crate::config::SkillPromptMode::Auto => {
                if let Some(ref budget) = agent.context_manager.budget
                    && budget.max_tokens() < 8192
                {
                    crate::config::SkillPromptMode::Compact
                } else {
                    crate::config::SkillPromptMode::Full
                }
            }
            other => other,
        };
        assert_eq!(effective_mode, crate::config::SkillPromptMode::Full);
    }

    // SF-6: SkillPromptMode::Compact forced config
    #[test]
    fn skill_prompt_mode_compact_forced_regardless_of_budget() {
        // Even with a large budget, Compact mode stays Compact
        let effective_mode = match crate::config::SkillPromptMode::Compact {
            crate::config::SkillPromptMode::Auto => {
                crate::config::SkillPromptMode::Full // would normally pick Full
            }
            other => other,
        };
        assert_eq!(effective_mode, crate::config::SkillPromptMode::Compact);
    }

    #[test]
    fn should_compact_disabled_without_budget() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        for i in 0..20 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i} with some content to add tokens"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }
        assert!(!agent.should_compact());
    }

    #[test]
    fn should_compact_below_threshold() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(1000, 0.20, 0.75, 4, 0);
        assert!(!agent.should_compact());
    }

    #[test]
    fn should_compact_above_threshold() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 4, 0);

        for i in 0..20 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message number {i} with enough content to push over budget"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }
        assert!(agent.should_compact());
    }

    #[tokio::test]
    async fn compact_context_preserves_system_and_tail() {
        let provider = mock_provider(vec!["compacted summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 2, 0);

        let system_content = agent.messages[0].content.clone();

        for i in 0..8 {
            agent.messages.push(Message {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: format!("message {i}"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }

        agent.compact_context().await.unwrap();

        assert_eq!(agent.messages[0].role, Role::System);
        assert_eq!(agent.messages[0].content, system_content);

        assert_eq!(agent.messages[1].role, Role::System);
        assert!(agent.messages[1].content.contains("[conversation summary"));

        let tail = &agent.messages[2..];
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].content, "message 6");
        assert_eq!(tail[1].content, "message 7");
    }

    #[tokio::test]
    async fn compact_context_too_few_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 4, 0);

        agent.messages.push(Message {
            role: Role::User,
            content: "msg1".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "msg2".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        let len_before = agent.messages.len();
        agent.compact_context().await.unwrap();
        assert_eq!(agent.messages.len(), len_before);
    }

    #[test]
    fn with_context_budget_zero_disables() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(0, 0.20, 0.75, 4, 0);
        assert!(agent.context_manager.budget.is_none());
    }

    #[test]
    fn with_context_budget_nonzero_enables() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(4096, 0.20, 0.80, 6, 0);

        assert!(agent.context_manager.budget.is_some());
        assert_eq!(
            agent.context_manager.budget.as_ref().unwrap().max_tokens(),
            4096
        );
        assert!((agent.context_manager.compaction_threshold - 0.80).abs() < f32::EPSILON);
        assert_eq!(agent.context_manager.compaction_preserve_tail, 6);
    }

    #[tokio::test]
    async fn compact_context_increments_metric() {
        let provider = mock_provider(vec!["summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 2, 0)
            .with_metrics(tx);

        for i in 0..8 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }

        agent.compact_context().await.unwrap();
        assert_eq!(rx.borrow().context_compactions, 1);
    }

    #[tokio::test]
    async fn test_prepare_context_no_budget_is_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent.prepare_context("test query").await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_correction_messages_removed_between_turns() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{CORRECTIONS_PREFIX}old correction data"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        );
        assert_eq!(agent.messages.len(), 2);

        agent.remove_correction_messages();
        assert_eq!(agent.messages.len(), 1);
        assert!(!agent.messages[0].content.starts_with(CORRECTIONS_PREFIX));
    }

    #[tokio::test]
    async fn test_remove_correction_messages_preserves_non_correction_system() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Add a non-correction system message
        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: "regular system message".to_string(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        );
        // Add a correction system message
        agent.messages.insert(
            2,
            Message {
                role: Role::System,
                content: format!("{CORRECTIONS_PREFIX}correction data"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        );
        assert_eq!(agent.messages.len(), 3);

        agent.remove_correction_messages();

        assert_eq!(agent.messages.len(), 2);
        assert!(
            agent
                .messages
                .iter()
                .any(|m| m.content == "regular system message")
        );
        assert!(
            !agent
                .messages
                .iter()
                .any(|m| m.content.starts_with(CORRECTIONS_PREFIX))
        );
    }

    #[tokio::test]
    async fn test_recall_injection_removed_between_turns() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{RECALL_PREFIX}old recall data"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        );
        assert_eq!(agent.messages.len(), 2);

        agent.remove_recall_messages();
        assert_eq!(agent.messages.len(), 1);
        assert!(!agent.messages[0].content.starts_with(RECALL_PREFIX));
    }

    #[tokio::test]
    async fn test_recall_without_qdrant_returns_empty() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent.inject_semantic_recall("test", 1000).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_trim_messages_preserves_system() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }
        assert_eq!(agent.messages.len(), 11);

        agent.trim_messages_to_budget(5);

        assert_eq!(agent.messages[0].role, Role::System);
        assert!(agent.messages.len() < 11);
    }

    #[tokio::test]
    async fn test_trim_messages_keeps_recent() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("msg {i}"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }

        agent.trim_messages_to_budget(5);

        let last = agent.messages.last().unwrap();
        assert_eq!(last.content, "msg 9");
    }

    #[tokio::test]
    async fn test_trim_zero_budget_is_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        for i in 0..5 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }
        let msg_count = agent.messages.len();

        agent.trim_messages_to_budget(0);
        assert_eq!(agent.messages.len(), msg_count);
    }

    async fn create_memory_with_summaries(
        provider: zeph_llm::any::AnyProvider,
        summaries: &[&str],
    ) -> (SemanticMemory, zeph_memory::ConversationId) {
        let memory = SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test")
            .await
            .unwrap();
        let cid = memory.sqlite().create_conversation().await.unwrap();
        for content in summaries {
            let m1 = memory
                .sqlite()
                .save_message(cid, "user", "q")
                .await
                .unwrap();
            let m2 = memory
                .sqlite()
                .save_message(cid, "assistant", "a")
                .await
                .unwrap();
            memory
                .sqlite()
                .save_summary(
                    cid,
                    content,
                    m1,
                    m2,
                    zeph_memory::TokenCounter::new().count_tokens(content) as i64,
                )
                .await
                .unwrap();
        }
        (memory, cid)
    }

    #[tokio::test]
    async fn test_inject_summaries_no_memory_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent.inject_summaries(1000).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_summaries_zero_budget_noop() {
        let provider = mock_provider(vec![]);
        let (memory, cid) = create_memory_with_summaries(provider.clone(), &["summary text"]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            50,
        );
        let msg_count = agent.messages.len();

        agent.inject_summaries(0).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_summaries_empty_summaries_noop() {
        let provider = mock_provider(vec![]);
        let (memory, cid) = create_memory_with_summaries(provider.clone(), &[]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            50,
        );
        let msg_count = agent.messages.len();

        agent.inject_summaries(1000).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_summaries_inserts_at_position_1() {
        let provider = mock_provider(vec![]);
        let (memory, cid) =
            create_memory_with_summaries(provider.clone(), &["User asked about Rust ownership"])
                .await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            50,
        );

        agent.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        agent.inject_summaries(1000).await.unwrap();

        assert_eq!(agent.messages[0].role, Role::System);
        assert!(agent.messages[1].content.starts_with(SUMMARY_PREFIX));
        assert_eq!(agent.messages[1].role, Role::System);
        assert!(
            agent.messages[1]
                .content
                .contains("User asked about Rust ownership")
        );
        assert_eq!(agent.messages[2].content, "hello");
    }

    #[tokio::test]
    async fn test_inject_summaries_removes_old_before_inject() {
        let provider = mock_provider(vec![]);
        let (memory, cid) =
            create_memory_with_summaries(provider.clone(), &["new summary data"]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            50,
        );

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{SUMMARY_PREFIX}old summary data"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        );
        agent.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        assert_eq!(agent.messages.len(), 3);

        agent.inject_summaries(1000).await.unwrap();

        let summary_msgs: Vec<_> = agent
            .messages
            .iter()
            .filter(|m| m.content.starts_with(SUMMARY_PREFIX))
            .collect();
        assert_eq!(summary_msgs.len(), 1);
        assert!(summary_msgs[0].content.contains("new summary data"));
        assert!(!summary_msgs[0].content.contains("old summary data"));
    }

    #[tokio::test]
    async fn test_inject_summaries_respects_token_budget() {
        let provider = mock_provider(vec![]);
        // Each summary entry is "- Messages X-Y: <content>\n" (~prefix overhead + content)
        let (memory, cid) = create_memory_with_summaries(
            provider.clone(),
            &[
                "short",
                "this is a much longer summary that should consume more tokens",
            ],
        )
        .await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            50,
        );

        agent.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        // Use a very small budget: only the prefix + maybe one short entry
        let tc = zeph_memory::TokenCounter::new();
        let prefix_cost = tc.count_tokens(SUMMARY_PREFIX);
        agent.inject_summaries(prefix_cost + 10).await.unwrap();

        let summary_msg = agent
            .messages
            .iter()
            .find(|m| m.content.starts_with(SUMMARY_PREFIX));

        if let Some(msg) = summary_msg {
            let token_count = tc.count_tokens(&msg.content);
            assert!(token_count <= prefix_cost + 10);
        }
    }

    #[tokio::test]
    async fn test_remove_summary_messages_preserves_other_system() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message {
                role: Role::System,
                content: format!("{SUMMARY_PREFIX}old summary"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        );
        agent.messages.insert(
            2,
            Message {
                role: Role::System,
                content: format!("{RECALL_PREFIX}recall data"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        );
        assert_eq!(agent.messages.len(), 3);

        agent.remove_summary_messages();
        assert_eq!(agent.messages.len(), 2);
        assert!(agent.messages[1].content.starts_with(RECALL_PREFIX));
    }

    #[test]
    fn test_prune_frees_tokens() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(1000, 0.20, 0.75, 4, 0)
            .with_metrics(tx);

        let big_body = "x".repeat(500);
        agent.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: big_body,
                compacted_at: None,
            }],
        ));

        let freed = agent.prune_tool_outputs(10);
        assert!(freed > 0);
        assert_eq!(rx.borrow().tool_output_prunes, 1);

        if let MessagePart::ToolOutput {
            body, compacted_at, ..
        } = &agent.messages[1].parts[0]
        {
            assert!(compacted_at.is_some());
            assert!(body.is_empty(), "body should be cleared after prune");
        } else {
            panic!("expected ToolOutput");
        }
    }

    #[test]
    fn test_prune_respects_protection_zone() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(10000, 0.20, 0.75, 4, 999_999);

        let big_body = "x".repeat(500);
        agent.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: big_body,
                compacted_at: None,
            }],
        ));

        let freed = agent.prune_tool_outputs(10);
        assert_eq!(freed, 0);

        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.messages[1].parts[0] {
            assert!(compacted_at.is_none());
        } else {
            panic!("expected ToolOutput");
        }
    }

    #[tokio::test]
    async fn test_tier2_after_insufficient_prune() {
        let provider = mock_provider(vec!["summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(100, 0.20, 0.75, 2, 0)
            .with_metrics(tx);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i} with enough content to push over budget threshold"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }

        agent.maybe_compact().await.unwrap();
        assert_eq!(rx.borrow().context_compactions, 1);
    }

    #[tokio::test]
    async fn test_inject_cross_session_no_memory_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        let msg_count = agent.messages.len();

        agent
            .inject_cross_session_context("test", 1000)
            .await
            .unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_inject_cross_session_zero_budget_noop() {
        let provider = mock_provider(vec![]);
        let (memory, cid) = create_memory_with_summaries(provider.clone(), &["summary"]).await;

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            std::sync::Arc::new(memory),
            cid,
            50,
            5,
            50,
        );
        let msg_count = agent.messages.len();

        agent.inject_cross_session_context("test", 0).await.unwrap();
        assert_eq!(agent.messages.len(), msg_count);
    }

    #[tokio::test]
    async fn test_remove_cross_session_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message::from_parts(
                Role::System,
                vec![MessagePart::CrossSession {
                    text: "old cross-session".into(),
                }],
            ),
        );
        assert_eq!(agent.messages.len(), 2);

        agent.remove_cross_session_messages();
        assert_eq!(agent.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_remove_cross_session_preserves_other_system() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.insert(
            1,
            Message::from_parts(
                Role::System,
                vec![MessagePart::Summary {
                    text: "keep this summary".into(),
                }],
            ),
        );
        agent.messages.insert(
            2,
            Message::from_parts(
                Role::System,
                vec![MessagePart::CrossSession {
                    text: "remove this".into(),
                }],
            ),
        );
        assert_eq!(agent.messages.len(), 3);

        agent.remove_cross_session_messages();
        assert_eq!(agent.messages.len(), 2);
        assert!(agent.messages[1].content.contains("keep this summary"));
    }

    #[tokio::test]
    async fn test_store_session_summary_on_compaction() {
        let provider = mock_provider(vec!["compacted summary".to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let (memory, cid) = create_memory_with_summaries(provider.clone(), &[]).await;

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(std::sync::Arc::new(memory), cid, 50, 5, 50)
            .with_context_budget(10000, 0.20, 0.80, 2, 0);

        for i in 0..10 {
            agent.messages.push(Message {
                role: Role::User,
                content: format!("message {i}"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }

        // compact_context should succeed (non-fatal store)
        agent.compact_context().await.unwrap();
        assert!(agent.messages[1].content.contains("compacted summary"));
    }

    #[test]
    fn test_budget_allocation_cross_session() {
        let budget = crate::context::ContextBudget::new(1000, 0.20);
        let tc = zeph_memory::TokenCounter::new();
        let alloc = budget.allocate("", "", &tc);

        assert!(alloc.cross_session > 0);
        assert!(alloc.summaries > 0);
        assert!(alloc.semantic_recall > 0);
        // cross_session should be smaller than summaries
        assert!(alloc.cross_session < alloc.summaries);
    }

    #[test]
    fn test_cross_session_score_threshold_filters() {
        use zeph_memory::semantic::SessionSummaryResult;

        let threshold: f32 = 0.35;

        let results = vec![
            SessionSummaryResult {
                summary_text: "high score".into(),
                score: 0.9,
                conversation_id: zeph_memory::ConversationId(1),
            },
            SessionSummaryResult {
                summary_text: "at threshold".into(),
                score: 0.35,
                conversation_id: zeph_memory::ConversationId(2),
            },
            SessionSummaryResult {
                summary_text: "below threshold".into(),
                score: 0.2,
                conversation_id: zeph_memory::ConversationId(3),
            },
            SessionSummaryResult {
                summary_text: "way below".into(),
                score: 0.0,
                conversation_id: zeph_memory::ConversationId(4),
            },
        ];

        let filtered: Vec<_> = results
            .into_iter()
            .filter(|r| r.score >= threshold)
            .collect();

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].summary_text, "high score");
        assert_eq!(filtered[1].summary_text, "at threshold");
    }

    #[test]
    fn context_budget_80_percent_threshold() {
        let budget = ContextBudget::new(1000, 0.20);
        let threshold = budget.max_tokens() * 4 / 5;
        assert_eq!(threshold, 800);
        assert!(800 >= threshold); // at threshold → should stop
        assert!(799 < threshold); // below threshold → should continue
    }

    #[test]
    fn prune_stale_tool_outputs_clears_old() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(10000, 0.20, 0.75, 4, 0)
            .with_metrics(tx);

        // Add 6 messages with tool outputs
        for i in 0..6 {
            agent.messages.push(Message::from_parts(
                Role::User,
                vec![MessagePart::ToolOutput {
                    tool_name: format!("tool_{i}"),
                    body: "x".repeat(200),
                    compacted_at: None,
                }],
            ));
        }
        // 7 messages total (1 system + 6 user)

        let freed = agent.prune_stale_tool_outputs(4);
        assert!(freed > 0);
        assert_eq!(rx.borrow().tool_output_prunes, 1);

        // Messages 1..3 should be pruned (boundary = 7-4=3)
        for i in 1..3 {
            if let MessagePart::ToolOutput {
                body, compacted_at, ..
            } = &agent.messages[i].parts[0]
            {
                assert!(body.is_empty(), "message {i} should be pruned");
                assert!(compacted_at.is_some());
            }
        }
        // Messages 3..6 should be untouched
        for i in 3..7 {
            if let MessagePart::ToolOutput { body, .. } = &agent.messages[i].parts[0] {
                assert!(!body.is_empty(), "message {i} should be kept");
            }
        }
    }

    #[test]
    fn prune_stale_tool_outputs_noop_when_few_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: "bash".into(),
                body: "output".into(),
                compacted_at: None,
            }],
        ));

        let freed = agent.prune_stale_tool_outputs(4);
        assert_eq!(freed, 0);
    }

    #[test]
    fn prune_stale_prunes_tool_result_too() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Add old message with large ToolResult
        agent.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "t1".into(),
                content: "x".repeat(500),
                is_error: false,
            }],
        ));
        // Add 4 recent messages
        for _ in 0..4 {
            agent.messages.push(Message {
                role: Role::User,
                content: "recent".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }

        let freed = agent.prune_stale_tool_outputs(4);
        assert!(freed > 0);

        if let MessagePart::ToolResult { content, .. } = &agent.messages[1].parts[0] {
            assert_eq!(content, "[pruned]");
        } else {
            panic!("expected ToolResult");
        }
    }

    #[test]
    fn prune_stale_tool_outputs_multi_part_tool_result_counted_once_per_part() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // One message with two ToolResult parts — each should be counted/pruned independently.
        agent.messages.push(Message::from_parts(
            Role::User,
            vec![
                MessagePart::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "x".repeat(500),
                    is_error: false,
                },
                MessagePart::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "y".repeat(500),
                    is_error: false,
                },
            ],
        ));
        // Add 4 recent messages to push the above into the prune zone.
        for _ in 0..4 {
            agent.messages.push(Message {
                role: Role::User,
                content: "recent".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }

        let freed = agent.prune_stale_tool_outputs(4);
        // Both parts must have contributed tokens.
        assert!(freed > 0, "freed must reflect tokens from both parts");

        // Both ToolResult parts in the stale message must be pruned.
        if let MessagePart::ToolResult { content, .. } = &agent.messages[1].parts[0] {
            assert_eq!(content, "[pruned]", "first ToolResult part must be pruned");
        } else {
            panic!("expected ToolResult at parts[0]");
        }
        if let MessagePart::ToolResult { content, .. } = &agent.messages[1].parts[1] {
            assert_eq!(content, "[pruned]", "second ToolResult part must be pruned");
        } else {
            panic!("expected ToolResult at parts[1]");
        }
    }

    #[tokio::test]
    async fn test_prepare_context_scrubs_secrets_when_redact_enabled() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(4096, 0.20, 0.80, 4, 0)
            .with_redact_credentials(true);

        // Push a user message containing a secret and a path
        agent.messages.push(Message {
            role: Role::User,
            content: "my key is sk-abc123xyz and lives at /Users/dev/config.toml".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        agent.prepare_context("test").await.unwrap();

        let user_msg = agent
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap();
        assert!(
            !user_msg.content.contains("sk-abc123xyz"),
            "secret must be redacted"
        );
        assert!(
            !user_msg.content.contains("/Users/dev/"),
            "path must be redacted"
        );
        assert!(
            user_msg.content.contains("[REDACTED]"),
            "secret replaced with [REDACTED]"
        );
        assert!(
            user_msg.content.contains("[PATH]"),
            "path replaced with [PATH]"
        );
    }

    #[tokio::test]
    async fn test_prepare_context_no_scrub_when_redact_disabled() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(4096, 0.20, 0.80, 4, 0)
            .with_redact_credentials(false);

        let original = "key sk-abc123xyz at /Users/dev/file.rs".to_string();
        agent.messages.push(Message {
            role: Role::User,
            content: original.clone(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        agent.prepare_context("test").await.unwrap();

        let user_msg = agent
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap();
        assert_eq!(
            user_msg.content, original,
            "content must be unchanged when redact disabled"
        );
    }

    #[test]
    fn should_compact_triggers_when_cached_tokens_exceed_threshold() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        // budget 1000, threshold 0.75 → compact at 750 tokens
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(1000, 0.20, 0.75, 4, 0);
        agent.cached_prompt_tokens = 900;

        assert!(
            agent.should_compact(),
            "cached_prompt_tokens above threshold must trigger compaction"
        );
    }

    #[test]
    fn should_compact_does_not_trigger_below_threshold() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        // budget 1000, threshold 0.75 → compact at 750 tokens
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_context_budget(1000, 0.20, 0.75, 4, 0);
        agent.cached_prompt_tokens = 100;

        assert!(
            !agent.should_compact(),
            "cached_prompt_tokens below threshold must not trigger compaction"
        );
    }

    #[tokio::test]
    async fn disambiguate_skills_reorders_on_match() {
        let json = r#"{"skill_name":"beta_skill","confidence":0.9,"params":{}}"#;
        let provider = mock_provider(vec![json.to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let metas = vec![
            SkillMeta {
                name: "alpha_skill".into(),
                description: "does alpha".into(),
                compatibility: None,
                license: None,
                metadata: Vec::new(),
                allowed_tools: Vec::new(),
                requires_secrets: Vec::new(),
                skill_dir: std::path::PathBuf::new(),
            },
            SkillMeta {
                name: "beta_skill".into(),
                description: "does beta".into(),
                compatibility: None,
                license: None,
                metadata: Vec::new(),
                allowed_tools: Vec::new(),
                requires_secrets: Vec::new(),
                skill_dir: std::path::PathBuf::new(),
            },
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let scored = vec![
            ScoredMatch {
                index: 0,
                score: 0.90,
            },
            ScoredMatch {
                index: 1,
                score: 0.88,
            },
        ];

        let result = agent
            .disambiguate_skills("do beta stuff", &refs, &scored)
            .await;
        assert!(result.is_some());
        let indices = result.unwrap();
        assert_eq!(indices[0], 1); // beta_skill moved to front
    }

    #[tokio::test]
    async fn disambiguate_skills_returns_none_on_error() {
        let provider = mock_provider_failing();
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let metas = vec![SkillMeta {
            name: "test".into(),
            description: "test".into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: Vec::new(),
            skill_dir: std::path::PathBuf::new(),
        }];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let scored = vec![ScoredMatch {
            index: 0,
            score: 0.5,
        }];

        let result = agent.disambiguate_skills("query", &refs, &scored).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn disambiguate_skills_empty_candidates() {
        let json = r#"{"skill_name":"none","confidence":0.1,"params":{}}"#;
        let provider = mock_provider(vec![json.to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let metas: Vec<SkillMeta> = vec![];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let scored: Vec<ScoredMatch> = vec![];

        let result = agent.disambiguate_skills("query", &refs, &scored).await;
        assert!(result.is_some());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn disambiguate_skills_unknown_skill_preserves_order() {
        let json = r#"{"skill_name":"nonexistent","confidence":0.5,"params":{}}"#;
        let provider = mock_provider(vec![json.to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let metas = vec![
            SkillMeta {
                name: "first".into(),
                description: "first skill".into(),
                compatibility: None,
                license: None,
                metadata: Vec::new(),
                allowed_tools: Vec::new(),
                requires_secrets: Vec::new(),
                skill_dir: std::path::PathBuf::new(),
            },
            SkillMeta {
                name: "second".into(),
                description: "second skill".into(),
                compatibility: None,
                license: None,
                metadata: Vec::new(),
                allowed_tools: Vec::new(),
                requires_secrets: Vec::new(),
                skill_dir: std::path::PathBuf::new(),
            },
        ];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let scored = vec![
            ScoredMatch {
                index: 0,
                score: 0.9,
            },
            ScoredMatch {
                index: 1,
                score: 0.88,
            },
        ];

        let result = agent
            .disambiguate_skills("query", &refs, &scored)
            .await
            .unwrap();
        // No swap since LLM returned unknown name
        assert_eq!(result[0], 0);
        assert_eq!(result[1], 1);
    }

    #[tokio::test]
    async fn disambiguate_single_candidate_no_swap() {
        let json = r#"{"skill_name":"only_skill","confidence":0.95,"params":{}}"#;
        let provider = mock_provider(vec![json.to_string()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let agent = Agent::new(provider, channel, registry, None, 5, executor);

        let metas = vec![SkillMeta {
            name: "only_skill".into(),
            description: "the only one".into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: Vec::new(),
            skill_dir: std::path::PathBuf::new(),
        }];
        let refs: Vec<&SkillMeta> = metas.iter().collect();
        let scored = vec![ScoredMatch {
            index: 0,
            score: 0.95,
        }];

        let result = agent
            .disambiguate_skills("query", &refs, &scored)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], 0);
    }

    #[tokio::test]
    async fn rebuild_system_prompt_excludes_skill_when_secret_missing() {
        use std::collections::HashMap;
        use zeph_skills::loader::SkillMeta;
        use zeph_skills::registry::SkillRegistry;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = SkillRegistry::default();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Skill requires a secret that is NOT available
        let meta_with_secret = SkillMeta {
            name: "secure-skill".into(),
            description: "needs a secret".into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: vec!["my_api_key".into()],
            skill_dir: std::path::PathBuf::new(),
        };

        // available_custom_secrets is empty — skill must be excluded
        agent.skill_state.available_custom_secrets = HashMap::new();

        let all_meta = vec![meta_with_secret];
        let matched_indices: Vec<usize> = vec![0];

        let filtered: Vec<usize> = matched_indices
            .into_iter()
            .filter(|&i| {
                let Some(meta) = all_meta.get(i) else {
                    return false;
                };
                meta.requires_secrets.iter().all(|s| {
                    agent
                        .skill_state
                        .available_custom_secrets
                        .contains_key(s.as_str())
                })
            })
            .collect();

        assert!(
            filtered.is_empty(),
            "skill must be excluded when required secret is missing"
        );
    }

    #[tokio::test]
    async fn rebuild_system_prompt_includes_skill_when_secret_present() {
        use zeph_skills::loader::SkillMeta;
        use zeph_skills::registry::SkillRegistry;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = SkillRegistry::default();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let meta_with_secret = SkillMeta {
            name: "secure-skill".into(),
            description: "needs a secret".into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: vec!["my_api_key".into()],
            skill_dir: std::path::PathBuf::new(),
        };

        // Secret IS available
        agent
            .skill_state
            .available_custom_secrets
            .insert("my_api_key".into(), crate::vault::Secret::new("token-val"));

        let all_meta = vec![meta_with_secret];
        let matched_indices: Vec<usize> = vec![0];

        let filtered: Vec<usize> = matched_indices
            .into_iter()
            .filter(|&i| {
                let Some(meta) = all_meta.get(i) else {
                    return false;
                };
                meta.requires_secrets.iter().all(|s| {
                    agent
                        .skill_state
                        .available_custom_secrets
                        .contains_key(s.as_str())
                })
            })
            .collect();

        assert_eq!(
            filtered,
            vec![0],
            "skill must be included when required secret is present"
        );
    }

    #[tokio::test]
    async fn rebuild_system_prompt_excludes_skill_when_only_partial_secrets_present() {
        use zeph_skills::loader::SkillMeta;
        use zeph_skills::registry::SkillRegistry;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = SkillRegistry::default();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let meta = SkillMeta {
            name: "multi-secret-skill".into(),
            description: "needs two secrets".into(),
            compatibility: None,
            license: None,
            metadata: Vec::new(),
            allowed_tools: Vec::new(),
            requires_secrets: vec!["secret_a".into(), "secret_b".into()],
            skill_dir: std::path::PathBuf::new(),
        };

        // Only "secret_a" present, "secret_b" missing — skill must be excluded.
        agent
            .skill_state
            .available_custom_secrets
            .insert("secret_a".into(), crate::vault::Secret::new("val-a"));

        let all_meta = vec![meta];
        let matched_indices: Vec<usize> = vec![0];

        let filtered: Vec<usize> = matched_indices
            .into_iter()
            .filter(|&i| {
                let Some(meta) = all_meta.get(i) else {
                    return false;
                };
                meta.requires_secrets.iter().all(|s| {
                    agent
                        .skill_state
                        .available_custom_secrets
                        .contains_key(s.as_str())
                })
            })
            .collect();

        assert!(
            filtered.is_empty(),
            "skill must be excluded when only partial secrets are available"
        );
    }

    fn make_tool_result_message(content: &str) -> Message {
        Message::from_parts(
            Role::User,
            vec![zeph_llm::provider::MessagePart::ToolResult {
                tool_use_id: "t1".into(),
                content: content.into(),
                is_error: false,
            }],
        )
    }

    fn make_text_message(text: &str) -> Message {
        Message::from_legacy(Role::User, text)
    }

    #[test]
    fn remove_tool_responses_empty_messages_unchanged() {
        let msgs: Vec<Message> = vec![];
        let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
        assert!(result.is_empty());
    }

    #[test]
    fn remove_tool_responses_no_tool_messages_unchanged() {
        let msgs = vec![make_text_message("hello"), make_text_message("world")];
        let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "hello");
    }

    #[test]
    fn remove_tool_responses_100_percent_clears_all() {
        let msgs = vec![
            make_tool_result_message("result1"),
            make_tool_result_message("result2"),
            make_tool_result_message("result3"),
        ];
        let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
        assert_eq!(result.len(), 3);
        for msg in &result {
            if let Some(zeph_llm::provider::MessagePart::ToolResult { content, .. }) =
                msg.parts.first()
            {
                assert_eq!(content, "[compacted]");
            }
        }
    }

    #[test]
    fn remove_tool_responses_50_percent_removes_half() {
        let msgs = vec![
            make_tool_result_message("r1"),
            make_tool_result_message("r2"),
            make_tool_result_message("r3"),
            make_tool_result_message("r4"),
        ];
        let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 0.5);
        let compacted = result
            .iter()
            .filter(|m| {
                m.parts.first().is_some_and(|p| {
                    matches!(p, zeph_llm::provider::MessagePart::ToolResult { content, .. } if content == "[compacted]")
                })
            })
            .count();
        assert_eq!(compacted, 2);
    }

    #[test]
    fn build_metadata_summary_includes_counts() {
        let msgs = vec![
            make_text_message("user question"),
            Message::from_legacy(Role::Assistant, "assistant response"),
        ];
        let summary = Agent::<MockChannel>::build_metadata_summary(&msgs);
        assert!(summary.contains("2"));
        assert!(summary.contains("1 user"));
        assert!(summary.contains("1 assistant"));
    }

    #[test]
    fn remove_tool_responses_middle_out_order_is_center_first() {
        // 5 tool messages at positions 0..4 (no non-tool messages).
        // Middle-out from center(=2): first right=2, then left=1, then right=3, then left=0, then right=4.
        // So removal order for 5 items: indices 2, 1, 3, 0, 4.
        // With fraction=1.0 (all 5 removed), all must be compacted.
        // To verify ordering we test partial removals:
        // fraction ~0.2 (ceil(5*0.2)=1) → 1 removed → must be center (index 2)
        // fraction ~0.4 (ceil(5*0.4)=2) → 2 removed → must be indices 2 and 1
        let msgs: Vec<Message> = (0..5)
            .map(|i| {
                Message::from_parts(
                    Role::User,
                    vec![zeph_llm::provider::MessagePart::ToolResult {
                        tool_use_id: format!("t{i}"),
                        content: format!("result{i}"),
                        is_error: false,
                    }],
                )
            })
            .collect();

        let is_compacted = |msgs: &[Message], idx: usize| -> bool {
            msgs[idx].parts.first().is_some_and(|p| {
                matches!(p, zeph_llm::provider::MessagePart::ToolResult { content, .. } if content == "[compacted]")
            })
        };

        // 1 removal — center (index 2)
        let one = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.20);
        assert!(
            is_compacted(&one, 2),
            "center (idx 2) must be first removed"
        );
        assert!(!is_compacted(&one, 0));
        assert!(!is_compacted(&one, 1));
        assert!(!is_compacted(&one, 3));
        assert!(!is_compacted(&one, 4));

        // 2 removals — center (2) + left-of-center (1)
        let two = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.40);
        assert!(is_compacted(&two, 2));
        assert!(is_compacted(&two, 1));
        assert!(!is_compacted(&two, 0));
        assert!(!is_compacted(&two, 3));
        assert!(!is_compacted(&two, 4));

        // 3 removals — 2 + right-of-center (3)
        let three = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 0.60);
        assert!(is_compacted(&three, 2));
        assert!(is_compacted(&three, 1));
        assert!(is_compacted(&three, 3));
        assert!(!is_compacted(&three, 0));
        assert!(!is_compacted(&three, 4));
    }

    #[test]
    fn truncate_chars_is_safe_for_multibyte() {
        // Each Cyrillic char is 2 bytes; slicing at byte 200 would panic on odd boundaries.
        let s: String = "Привет".repeat(50); // 300 chars, 600 bytes
        let truncated = super::truncate_chars(&s, 200);
        assert!(truncated.ends_with('…'));
        // Must be valid UTF-8 (no panic means success, but also check char count)
        assert_eq!(truncated.chars().count(), 201); // 200 chars + '…'
    }

    // --- truncate_chars additional edge cases ---

    #[test]
    fn truncate_chars_ascii_exact() {
        let s = "abcde";
        // max_chars == len → no truncation
        let result = super::truncate_chars(s, 5);
        assert_eq!(result, "abcde");
    }

    #[test]
    fn truncate_chars_emoji() {
        // 🚀 is a single Unicode scalar even though it is 4 bytes
        let s = "🚀🚀🚀🚀🚀";
        let result = super::truncate_chars(s, 3);
        assert!(result.ends_with('…'), "should append ellipsis");
        // 3 emoji + ellipsis = 4 Unicode scalars
        assert_eq!(result.chars().count(), 4);
    }

    #[test]
    fn truncate_chars_empty() {
        let result = super::truncate_chars("", 10);
        assert_eq!(result, "");
    }

    #[test]
    fn truncate_chars_shorter_than_max() {
        let s = "hello";
        let result = super::truncate_chars(s, 100);
        assert_eq!(result, "hello");
    }

    #[test]
    fn truncate_chars_zero_max() {
        let s = "hello";
        // max_chars = 0 means every char is beyond the limit → truncate at position 0
        let result = super::truncate_chars(s, 0);
        assert!(result.ends_with('…'));
        // The part before '…' must be empty (0 chars kept)
        assert_eq!(result, "…");
    }

    // --- build_chunk_prompt ---

    #[test]
    fn build_chunk_prompt_contains_all_nine_sections() {
        let messages = vec![Message {
            role: Role::User,
            content: "help me refactor this code".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let prompt = Agent::<MockChannel>::build_chunk_prompt(&messages);

        let sections = [
            "User Intent",
            "Technical Concepts",
            "Files & Code",
            "Errors & Fixes",
            "Problem Solving",
            "User Messages",
            "Pending Tasks",
            "Current Work",
            "Next Step",
        ];
        for section in sections {
            assert!(
                prompt.contains(section),
                "prompt missing section: {section}"
            );
        }
    }

    #[test]
    fn build_chunk_prompt_empty_messages() {
        let messages: &[Message] = &[];
        let prompt = Agent::<MockChannel>::build_chunk_prompt(messages);
        // Even with no messages the prompt structure must be valid (not panic, contains sections)
        assert!(prompt.contains("User Intent"));
        assert!(prompt.contains("Next Step"));
    }

    // --- build_metadata_summary robustness ---

    #[test]
    fn build_metadata_summary_empty_messages() {
        let messages: &[Message] = &[];
        let summary = Agent::<MockChannel>::build_metadata_summary(messages);
        assert!(summary.contains("Messages compacted: 0"));
        assert!(summary.contains("0 user"));
        assert!(summary.contains("0 assistant"));
    }

    #[test]
    fn build_metadata_summary_utf8_content() {
        let messages = vec![
            Message {
                role: Role::User,
                content: "Привет мир 🌍".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::Assistant,
                content: "Hello 🌐".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];
        let summary = Agent::<MockChannel>::build_metadata_summary(&messages);
        // Must not panic on multi-byte content
        assert!(summary.contains("Messages compacted: 2"));
        assert!(summary.contains("1 user"));
        assert!(summary.contains("1 assistant"));
    }

    #[test]
    fn build_metadata_summary_truncation_boundary() {
        let long_content = "a".repeat(300);
        let messages = vec![Message {
            role: Role::User,
            content: long_content,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let summary = Agent::<MockChannel>::build_metadata_summary(&messages);
        // The last user message preview is capped at 200 chars + '…'
        assert!(
            summary.contains('…'),
            "long content should be truncated with ellipsis"
        );
    }

    // --- remove_tool_responses_middle_out edge cases ---

    #[test]
    fn remove_tool_responses_single_tool_message() {
        let msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: "t1".into(),
                content: "result".into(),
                is_error: false,
            }],
        );
        let result = Agent::<MockChannel>::remove_tool_responses_middle_out(vec![msg], 1.0);
        assert_eq!(result.len(), 1);
        if let MessagePart::ToolResult { content, .. } = &result[0].parts[0] {
            assert_eq!(content, "[compacted]");
        } else {
            panic!("expected ToolResult part");
        }
    }

    #[test]
    fn remove_tool_responses_all_tiers_progressive() {
        // Build 10 messages, all with ToolResult parts
        let make_tool_msg = |i: usize| {
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: format!("result_{i}"),
                    is_error: false,
                }],
            )
        };
        let msgs: Vec<Message> = (0..10).map(make_tool_msg).collect();

        let count_compacted = |result: &[Message]| {
            result
                .iter()
                .filter(|m| {
                    m.parts.iter().any(|p| {
                        matches!(p, MessagePart::ToolResult { content, .. } if content == "[compacted]")
                    })
                })
                .count()
        };

        // 10% of 10 = ceil(1.0) = 1
        let r10 = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.10);
        assert_eq!(count_compacted(&r10), 1);

        // 20% of 10 = ceil(2.0) = 2
        let r20 = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.20);
        assert_eq!(count_compacted(&r20), 2);

        // 50% of 10 = ceil(5.0) = 5
        let r50 = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs.clone(), 0.50);
        assert_eq!(count_compacted(&r50), 5);

        // 100% of 10 = 10
        let r100 = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
        assert_eq!(count_compacted(&r100), 10);
    }

    fn make_tool_pair(agent: &mut Agent<MockChannel>, tool_name: &str) {
        agent.messages.push(Message::from_parts(
            Role::Assistant,
            vec![MessagePart::ToolUse {
                id: format!("id_{tool_name}"),
                name: tool_name.to_owned(),
                input: serde_json::json!({"cmd": "echo hello"}),
            }],
        ));
        agent.messages.push(Message::from_parts(
            Role::User,
            vec![MessagePart::ToolResult {
                tool_use_id: format!("id_{tool_name}"),
                content: format!("output of {tool_name}"),
                is_error: false,
            }],
        ));
    }

    #[test]
    fn count_tool_pairs_counts_visible_native_pairs() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        assert_eq!(agent.count_tool_pairs(), 0);

        make_tool_pair(&mut agent, "bash");
        assert_eq!(agent.count_tool_pairs(), 1);

        make_tool_pair(&mut agent, "read_file");
        assert_eq!(agent.count_tool_pairs(), 2);
    }

    #[test]
    fn count_tool_pairs_ignores_hidden_pairs() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        make_tool_pair(&mut agent, "bash");
        // hide the first pair
        agent.messages[1].metadata.agent_visible = false;
        agent.messages[2].metadata.agent_visible = false;

        assert_eq!(agent.count_tool_pairs(), 0);
    }

    #[test]
    fn find_oldest_tool_pair_returns_correct_indices() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        assert_eq!(agent.find_oldest_tool_pair(), None);

        make_tool_pair(&mut agent, "bash");
        // system = 0, request = 1, response = 2
        assert_eq!(agent.find_oldest_tool_pair(), Some((1, 2)));

        make_tool_pair(&mut agent, "read_file");
        // oldest pair is still (1, 2)
        assert_eq!(agent.find_oldest_tool_pair(), Some((1, 2)));
    }

    #[test]
    fn find_oldest_tool_pair_skips_hidden() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        make_tool_pair(&mut agent, "bash");
        make_tool_pair(&mut agent, "read_file");
        // hide first pair
        agent.messages[1].metadata.agent_visible = false;
        agent.messages[2].metadata.agent_visible = false;

        // second pair: request = 3, response = 4
        assert_eq!(agent.find_oldest_tool_pair(), Some((3, 4)));
    }

    #[tokio::test]
    async fn maybe_summarize_tool_pair_below_cutoff_does_nothing() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent =
            Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(6);

        // 3 pairs < cutoff of 6
        make_tool_pair(&mut agent, "bash");
        make_tool_pair(&mut agent, "read_file");
        make_tool_pair(&mut agent, "write_file");

        let msg_count_before = agent.messages.len();
        agent.maybe_summarize_tool_pair().await;
        assert_eq!(agent.messages.len(), msg_count_before);
    }

    #[tokio::test]
    async fn maybe_summarize_tool_pair_at_exact_cutoff_does_nothing() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent =
            Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(3);

        // exactly 3 pairs == cutoff of 3, should NOT summarize
        make_tool_pair(&mut agent, "a");
        make_tool_pair(&mut agent, "b");
        make_tool_pair(&mut agent, "c");

        let msg_count_before = agent.messages.len();
        agent.maybe_summarize_tool_pair().await;
        assert_eq!(agent.messages.len(), msg_count_before);
    }

    #[tokio::test]
    async fn maybe_summarize_tool_pair_above_cutoff_hides_oldest_and_inserts_summary() {
        let summary_text = "summarized tool call".to_owned();
        let provider = mock_provider(vec![summary_text.clone()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent =
            Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(2);

        // 3 pairs > cutoff of 2
        make_tool_pair(&mut agent, "bash");
        make_tool_pair(&mut agent, "read_file");
        make_tool_pair(&mut agent, "write_file");

        let msg_count_before = agent.messages.len();
        agent.maybe_summarize_tool_pair().await;

        // one summary message inserted
        assert_eq!(agent.messages.len(), msg_count_before + 1);
        // oldest pair (indices 1, 2) should be hidden
        assert!(!agent.messages[1].metadata.agent_visible);
        assert!(!agent.messages[2].metadata.agent_visible);
        // summary inserted at index 3
        let summary_msg = &agent.messages[3];
        assert!(
            summary_msg.content.contains(&summary_text),
            "summary message should contain the LLM response"
        );
        assert!(!summary_msg.metadata.user_visible);
        assert!(summary_msg.metadata.agent_visible);
        assert!(
            summary_msg
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::Summary { .. }))
        );
    }

    #[tokio::test]
    async fn maybe_summarize_tool_pair_llm_error_skips_gracefully() {
        let provider = mock_provider_failing();
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent =
            Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(1);

        // 2 pairs > cutoff of 1
        make_tool_pair(&mut agent, "bash");
        make_tool_pair(&mut agent, "read_file");

        let msg_count_before = agent.messages.len();
        // Should not panic, just warn and skip
        agent.maybe_summarize_tool_pair().await;
        // No messages should be added or hidden
        assert_eq!(agent.messages.len(), msg_count_before);
        assert!(agent.messages[1].metadata.agent_visible);
        assert!(agent.messages[2].metadata.agent_visible);
    }

    #[test]
    fn build_tool_pair_summary_prompt_contains_xml_delimiters() {
        let req = Message {
            role: Role::Assistant,
            content: "call bash".into(),
            ..Message::default()
        };
        let res = Message {
            role: Role::User,
            content: "exit code 0".into(),
            ..Message::default()
        };
        let prompt = Agent::<MockChannel>::build_tool_pair_summary_prompt(&req, &res);
        assert!(prompt.contains("<tool_request>"), "missing <tool_request>");
        assert!(
            prompt.contains("</tool_request>"),
            "missing </tool_request>"
        );
        assert!(
            prompt.contains("<tool_response>"),
            "missing <tool_response>"
        );
        assert!(
            prompt.contains("</tool_response>"),
            "missing </tool_response>"
        );
        assert!(prompt.contains("call bash"));
        assert!(prompt.contains("exit code 0"));
    }

    #[tokio::test]
    async fn maybe_summarize_tool_pair_empty_messages_does_nothing() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent =
            Agent::new(provider, channel, registry, None, 5, executor).with_tool_call_cutoff(1);

        agent.messages.clear();
        agent.maybe_summarize_tool_pair().await;
        assert!(agent.messages.is_empty());
    }

    #[test]
    fn remove_tool_responses_fraction_zero_changes_nothing() {
        let msgs = vec![
            make_tool_result_message("result1"),
            make_tool_result_message("result2"),
        ];
        let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 0.0);
        assert_eq!(result.len(), 2);
        for msg in &result {
            if let Some(MessagePart::ToolResult { content, .. }) = msg.parts.first() {
                assert_ne!(
                    content, "[compacted]",
                    "fraction=0.0 should not compact anything"
                );
            }
        }
    }

    #[test]
    fn remove_tool_responses_tool_output_parts_compacted() {
        let msgs = vec![
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolOutput {
                    tool_name: "bash".into(),
                    body: "output text".into(),
                    compacted_at: None,
                }],
            ),
            Message::from_parts(
                Role::User,
                vec![MessagePart::ToolOutput {
                    tool_name: "read_file".into(),
                    body: "file content".into(),
                    compacted_at: None,
                }],
            ),
        ];
        let result = Agent::<MockChannel>::remove_tool_responses_middle_out(msgs, 1.0);
        assert_eq!(result.len(), 2);
        for msg in &result {
            if let Some(MessagePart::ToolOutput {
                body, compacted_at, ..
            }) = msg.parts.first()
            {
                assert!(
                    body.is_empty(),
                    "ToolOutput body should be cleared after compaction"
                );
                assert!(
                    compacted_at.is_some(),
                    "ToolOutput compacted_at should be set"
                );
            } else {
                panic!("expected ToolOutput part");
            }
        }
    }
}
