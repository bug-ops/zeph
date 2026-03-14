// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write;

use futures::StreamExt as _;
use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, MessagePart, Role};

use super::super::Agent;
use super::super::context_manager::CompactionTier;
use crate::channel::Channel;
use crate::context::ContextBudget;

impl<C: Channel> Agent<C> {
    pub(super) fn build_chunk_prompt(messages: &[Message]) -> String {
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
    pub(super) fn build_metadata_summary(messages: &[Message]) -> String {
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

        let last_user_preview = super::truncate_chars(&last_user, 200);
        let last_assistant_preview = super::truncate_chars(&last_assistant, 200);

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

        let chunks = super::chunk_messages(
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
    pub(super) fn remove_tool_responses_middle_out(
        mut messages: Vec<Message>,
        fraction: f32,
    ) -> Vec<Message> {
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
    ) -> Result<String, super::super::error::AgentError> {
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

    pub(in crate::agent) async fn compact_context(
        &mut self,
    ) -> Result<(), super::super::error::AgentError> {
        // Force-apply any pending deferred summaries before draining to avoid losing them (CRIT-01).
        let _ = self.apply_deferred_summaries();

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
    pub(in crate::agent) fn prune_tool_outputs(&mut self, min_to_free: usize) -> usize {
        let protect = self.context_manager.prune_protect_tokens;
        let mut tail_tokens = 0usize;
        let mut protection_boundary = self.messages.len();
        if protect > 0 {
            for (i, msg) in self.messages.iter().enumerate().rev() {
                tail_tokens += self.token_counter.count_message_tokens(msg);
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
    ///
    /// # Invariant
    ///
    /// This method MUST be called AFTER `maybe_summarize_tool_pair()`. The summarizer
    /// reads `msg.content` to build the LLM prompt; pruning replaces that content with
    /// `"[pruned]"`. Calling prune first would cause the summarizer to produce useless
    /// summaries. After summarization, the processed pair has `deferred_summary` set and
    /// is skipped by `count_unsummarized_pairs`. The pruning loop may still clear their
    /// bodies for token savings, but the content has already been captured in the summary.
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

    pub(super) fn count_unsummarized_pairs(&self) -> usize {
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
                    && next.metadata.deferred_summary.is_none()
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

    /// Find the oldest tool request/response pair that has not yet been summarized.
    ///
    /// Skips pairs where:
    /// - `deferred_summary` is already set (already queued for application), or
    /// - the response content was pruned (all ToolResult/ToolOutput bodies are empty or
    ///   contain only `"[pruned]"`), which would produce a useless summary (IMP-03 fix).
    pub(super) fn find_oldest_unsummarized_pair(&self) -> Option<(usize, usize)> {
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
                    && next.metadata.deferred_summary.is_none()
                {
                    // Skip pairs whose response content has been fully pruned — summarizing
                    // "[pruned]" produces a useless result (IMP-03).
                    let all_pruned = next.parts.iter().all(|p| match p {
                        MessagePart::ToolOutput { body, .. } => body.is_empty(),
                        MessagePart::ToolResult { content, .. } => {
                            content.trim() == "[pruned]" || content.is_empty()
                        }
                        _ => true,
                    });
                    if !all_pruned {
                        return Some((i, i + 1));
                    }
                }
            }
            i += 1;
        }
        None
    }

    pub(super) fn count_deferred_summaries(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.metadata.deferred_summary.is_some())
            .count()
    }

    pub(super) fn build_tool_pair_summary_prompt(req: &Message, res: &Message) -> String {
        format!(
            "Produce a concise but technically precise summary of this tool invocation.\n\
             Preserve all facts that would be needed to continue work without re-running the tool:\n\
             - Tool name and key input parameters (file paths, function names, patterns, line ranges)\n\
             - Exact findings: line numbers, struct/enum/function names, error messages, numeric values\n\
             - Outcome: what was found, changed, created, or confirmed\n\
             Do NOT omit specific identifiers, paths, or numbers — they cannot be recovered later.\n\
             Use 2-4 sentences maximum.\n\n\
             <tool_request>\n{}\n</tool_request>\n\n<tool_response>\n{}\n</tool_response>",
            req.content, res.content
        )
    }

    pub(in crate::agent) async fn maybe_summarize_tool_pair(&mut self) {
        // Drain the entire backlog above cutoff in one turn so that a resumed session
        // with many accumulated pairs catches up before Tier 1 pruning fires.
        let cutoff = self.memory_state.tool_call_cutoff;
        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);
        let mut summarized = 0usize;
        loop {
            let pair_count = self.count_unsummarized_pairs();
            if pair_count <= cutoff {
                break;
            }
            let Some((req_idx, resp_idx)) = self.find_oldest_unsummarized_pair() else {
                break;
            };
            let prompt = Self::build_tool_pair_summary_prompt(
                &self.messages[req_idx],
                &self.messages[resp_idx],
            );
            let msgs = [Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];
            let _ = self.channel.send_status("summarizing output...").await;
            let chat_fut = self.summary_or_primary_provider().chat(&msgs);
            let summary = match tokio::time::timeout(llm_timeout, chat_fut).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(%e, "tool pair summarization failed, stopping batch");
                    let _ = self.channel.send_status("").await;
                    break;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = self.runtime.timeouts.llm_seconds,
                        "tool pair summarization timed out, stopping batch"
                    );
                    let _ = self.channel.send_status("").await;
                    break;
                }
            };
            // DEFERRED: store summary on response metadata instead of immediately mutating the
            // array. Applied lazily by apply_deferred_summaries() when context pressure rises,
            // preserving the message prefix for Claude API cache hits.
            let summary = super::cap_summary(self.maybe_redact(&summary).into_owned(), 8_000);
            self.messages[resp_idx].metadata.deferred_summary = Some(summary.clone());
            summarized += 1;
            tracing::debug!(
                pair_count,
                cutoff,
                req_idx,
                resp_idx,
                summary_len = summary.len(),
                "deferred tool pair summary stored"
            );
        }
        let _ = self.channel.send_status("").await;
        if summarized > 0 {
            tracing::info!(summarized, "batch-summarized tool pairs above cutoff");
        }
    }

    /// Batch-apply all pending deferred tool pair summaries.
    ///
    /// Processes in reverse index order (highest first) so that inserting a summary message
    /// at `resp_idx + 1` does not shift the indices of not-yet-processed pairs.
    ///
    /// Returns the number of summaries applied.
    pub(in crate::agent) fn apply_deferred_summaries(&mut self) -> usize {
        // Phase 1: collect (resp_idx, req_idx, summary) for all messages with deferred_summary.
        let mut targets: Vec<(usize, usize, String)> = Vec::new();
        for i in 1..self.messages.len() {
            if self.messages[i].metadata.deferred_summary.is_none() {
                continue;
            }
            // Verify the structural invariant: tool response preceded by matching tool request.
            if self.messages[i].role == Role::User
                && self.messages[i].metadata.agent_visible
                && i > 0
                && self.messages[i - 1].role == Role::Assistant
                && self.messages[i - 1].metadata.agent_visible
                && self.messages[i - 1]
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }))
            {
                let summary = self.messages[i]
                    .metadata
                    .deferred_summary
                    .clone()
                    .expect("checked above");
                targets.push((i, i - 1, summary));
            } else {
                tracing::warn!(
                    resp_idx = i,
                    "deferred summary orphaned: req message not found at resp_idx={i}"
                );
            }
        }

        if targets.is_empty() {
            return 0;
        }

        // Phase 2: sort descending by resp_idx so insertions do not invalidate lower indices.
        targets.sort_by(|a, b| b.0.cmp(&a.0));

        let count = targets.len();
        for (resp_idx, req_idx, summary) in targets {
            self.messages[req_idx].metadata.agent_visible = false;
            self.messages[resp_idx].metadata.agent_visible = false;
            self.messages[resp_idx].metadata.deferred_summary = None;

            let content = format!("[tool summary] {summary}");
            let summary_msg = Message {
                role: Role::Assistant,
                content,
                parts: vec![MessagePart::Summary { text: summary }],
                metadata: MessageMetadata::agent_only(),
            };
            self.messages.insert(resp_idx + 1, summary_msg);
        }

        self.recompute_prompt_tokens();
        tracing::info!(count, "applied deferred tool pair summaries");
        count
    }

    /// Apply deferred summaries if context usage exceeds the soft compaction threshold,
    /// or when enough summaries have accumulated to prevent content loss from pruning.
    ///
    /// Two triggers:
    /// - Token pressure: `cached_prompt_tokens > budget * soft_compaction_threshold`
    /// - Count pressure: `pending >= tool_call_cutoff` (guards against pruning replacing
    ///   summaries with `[pruned]` when `prepare_context` recomputes tokens to a low value)
    ///
    /// This is Tier 0 — a pure in-memory operation with no LLM call. Intentionally
    /// does NOT set `compacted_this_turn` so that proactive/reactive compaction may
    /// also fire in the same turn if tokens remain above their respective thresholds.
    /// Called from tool execution loops on every iteration to apply summaries eagerly.
    pub(in crate::agent) fn maybe_apply_deferred_summaries(&mut self) {
        let pending = self.count_deferred_summaries();
        if pending == 0 {
            return;
        }
        let token_pressure = matches!(
            self.compaction_tier(),
            CompactionTier::Soft | CompactionTier::Hard
        );
        let count_pressure = pending >= self.memory_state.tool_call_cutoff;
        if !token_pressure && !count_pressure {
            return;
        }
        let applied = self.apply_deferred_summaries();
        if applied > 0 {
            tracing::info!(
                applied,
                token_pressure,
                count_pressure,
                "tier-0: batch-applied deferred tool summaries"
            );
        }
    }

    /// Tiered compaction: Soft tier prunes tool outputs + applies deferred summaries (no LLM),
    /// Hard tier falls back to full LLM summarization.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    pub(in crate::agent) async fn maybe_compact(
        &mut self,
    ) -> Result<(), super::super::error::AgentError> {
        // Guard 3 — Exhaustion: stop compaction permanently when it cannot reduce context.
        if self.context_manager.compaction_exhausted {
            if !self.context_manager.exhaustion_warned {
                self.context_manager.exhaustion_warned = true;
                tracing::warn!("compaction exhausted: context budget too tight for this session");
                let _ = self
                    .channel
                    .send(
                        "Warning: context budget is too tight — compaction cannot free enough \
                         space. Consider increasing [memory] context_budget_tokens or starting \
                         a new session.",
                    )
                    .await;
            }
            return Ok(());
        }

        // S1: skip client-side compaction when server compaction is active — unless context
        // has grown past 95% of the budget without a server compaction event (safety fallback).
        if self.server_compaction_active {
            let budget = self
                .context_manager
                .budget
                .as_ref()
                .map_or(0, ContextBudget::max_tokens);
            if budget > 0 {
                let total_tokens: usize = self
                    .messages
                    .iter()
                    .map(|m| self.token_counter.count_message_tokens(m))
                    .sum();
                let fallback_threshold = budget * 95 / 100;
                if total_tokens < fallback_threshold {
                    return Ok(());
                }
                tracing::warn!(
                    total_tokens,
                    fallback_threshold,
                    "server compaction active but context at 95%+ — falling back to client-side"
                );
            } else {
                return Ok(());
            }
        }
        // Skip if hard compaction already ran this turn (CRIT-03).
        if self.context_manager.compacted_this_turn {
            return Ok(());
        }
        // Guard 1 — Cooldown: skip Hard-tier LLM compaction for N turns after the last successful
        // compaction. Soft compaction (pruning only) is still allowed during cooldown.
        let in_cooldown = self.context_manager.compaction_turns_since > 0;
        if in_cooldown {
            self.context_manager.compaction_turns_since -= 1;
        }

        match self.compaction_tier() {
            CompactionTier::None => Ok(()),
            CompactionTier::Soft => {
                let _ = self.channel.send_status("soft compacting context...").await;

                // Step 1: apply deferred tool summaries (free tokens without LLM).
                self.apply_deferred_summaries();

                // Step 2: prune tool outputs down to soft threshold.
                let budget = self
                    .context_manager
                    .budget
                    .as_ref()
                    .map_or(0, ContextBudget::max_tokens);
                let soft_threshold =
                    (budget as f32 * self.context_manager.soft_compaction_threshold) as usize;
                let cached = usize::try_from(self.cached_prompt_tokens).unwrap_or(usize::MAX);
                let min_to_free = cached.saturating_sub(soft_threshold);
                if min_to_free > 0 {
                    self.prune_tool_outputs(min_to_free);
                }

                let _ = self.channel.send_status("").await;
                tracing::info!(
                    cached_tokens = self.cached_prompt_tokens,
                    soft_threshold,
                    "soft compaction complete"
                );
                // Soft compaction does NOT set compacted_this_turn, allowing Hard to fire
                // in the same turn if context is still above the hard threshold.
                Ok(())
            }
            CompactionTier::Hard => {
                // Cooldown guard: skip LLM summarization while cooling down.
                if in_cooldown {
                    tracing::debug!(
                        turns_remaining = self.context_manager.compaction_turns_since,
                        "hard compaction skipped: cooldown active"
                    );
                    return Ok(());
                }

                let budget = self
                    .context_manager
                    .budget
                    .as_ref()
                    .map_or(0, ContextBudget::max_tokens);
                let hard_threshold =
                    (budget as f32 * self.context_manager.hard_compaction_threshold) as usize;
                let cached = usize::try_from(self.cached_prompt_tokens).unwrap_or(usize::MAX);
                let min_to_free = cached.saturating_sub(hard_threshold);

                let _ = self.channel.send_status("compacting context...").await;

                // Step 1: apply deferred summaries first (free tokens without LLM).
                self.apply_deferred_summaries();

                // Step 2: prune tool outputs.
                let freed = self.prune_tool_outputs(min_to_free);
                if freed >= min_to_free {
                    tracing::info!(freed, "hard compaction: pruning sufficient");
                    self.context_manager.compacted_this_turn = true;
                    self.context_manager.compaction_turns_since =
                        self.context_manager.compaction_cooldown_turns;
                    let _ = self.channel.send_status("").await;
                    return Ok(());
                }

                // Step 3: Guard 2 — Counterproductive: check if there are enough messages
                // to make LLM summarization worthwhile.
                let preserve_tail = self.context_manager.compaction_preserve_tail;
                let compactable = self.messages.len().saturating_sub(preserve_tail + 1);
                if compactable <= 1 {
                    tracing::warn!(
                        compactable,
                        "hard compaction: too few messages to compact, marking exhausted"
                    );
                    self.context_manager.compaction_exhausted = true;
                    let _ = self.channel.send_status("").await;
                    return Ok(());
                }

                // Step 4: fall back to full LLM summarization.
                tracing::info!(
                    freed,
                    min_to_free,
                    "hard compaction: pruning insufficient, falling back to LLM summarization"
                );
                let tokens_before = self.cached_prompt_tokens;
                let result = self.compact_context().await;
                if result.is_ok() {
                    // Guard 2 — Counterproductive: net freed tokens is zero (summary ate all
                    // freed space — no net reduction).
                    let freed_tokens = tokens_before.saturating_sub(self.cached_prompt_tokens);
                    if freed_tokens == 0 {
                        tracing::warn!(
                            "hard compaction: summary consumed all freed tokens — no net \
                             reduction, marking exhausted"
                        );
                        self.context_manager.compaction_exhausted = true;
                        let _ = self.channel.send_status("").await;
                        return result;
                    }
                    // Guard 3 — Still above threshold: compaction freed some tokens but context
                    // remains above the hard threshold; further LLM attempts are unlikely to help.
                    if matches!(self.compaction_tier(), CompactionTier::Hard) {
                        tracing::warn!(
                            freed_tokens,
                            "hard compaction: context still above hard threshold after \
                             compaction, marking exhausted"
                        );
                        self.context_manager.compaction_exhausted = true;
                        let _ = self.channel.send_status("").await;
                        return result;
                    }
                    self.context_manager.compacted_this_turn = true;
                    self.context_manager.compaction_turns_since =
                        self.context_manager.compaction_cooldown_turns;
                }
                let _ = self.channel.send_status("").await;
                result
            }
        }
    }

    /// Proactive context compression: fires before reactive compaction when context exceeds
    /// the configured `threshold_tokens`. Mutually exclusive with reactive compaction per turn
    /// (guarded by `compacted_this_turn`).
    pub(in crate::agent) async fn maybe_proactive_compress(
        &mut self,
    ) -> Result<(), super::super::error::AgentError> {
        // S1: skip proactive compression when server compaction is active — unless context
        // has grown past 95% of the budget without a server compaction event (safety fallback).
        if self.server_compaction_active {
            let budget = self
                .context_manager
                .budget
                .as_ref()
                .map_or(0, ContextBudget::max_tokens);
            if budget > 0 {
                let fallback_threshold = (budget * 95 / 100) as u64;
                if self.cached_prompt_tokens <= fallback_threshold {
                    return Ok(());
                }
                tracing::warn!(
                    cached_prompt_tokens = self.cached_prompt_tokens,
                    fallback_threshold,
                    "server compaction active but context at 95%+ — falling back to client-side proactive"
                );
            } else {
                return Ok(());
            }
        }
        let Some((_threshold, max_summary_tokens)) = self
            .context_manager
            .should_proactively_compress(self.cached_prompt_tokens)
        else {
            return Ok(());
        };

        let tokens_before = self.cached_prompt_tokens;
        let _ = self.channel.send_status("compressing context...").await;
        tracing::info!(
            max_summary_tokens,
            cached_tokens = tokens_before,
            "proactive compression triggered"
        );

        let result = self
            .compact_context_with_budget(Some(max_summary_tokens))
            .await;

        if result.is_ok() {
            self.context_manager.compacted_this_turn = true;
            let tokens_saved = tokens_before.saturating_sub(self.cached_prompt_tokens);
            self.update_metrics(|m| {
                m.compression_events += 1;
                m.compression_tokens_saved += tokens_saved;
            });
        }

        let _ = self.channel.send_status("").await;
        result
    }

    /// Run LLM compaction with an optional chunk budget hint for the summary.
    ///
    /// When `max_summary_tokens` is `Some(n)`, the chunk budget used by `chunk_messages`
    /// is capped at `n`, limiting how much context is summarized per LLM call.
    async fn compact_context_with_budget(
        &mut self,
        max_summary_tokens: Option<usize>,
    ) -> Result<(), super::super::error::AgentError> {
        // Force-apply any pending deferred summaries before draining to avoid losing them (CRIT-01).
        let _ = self.apply_deferred_summaries();

        let preserve_tail = self.context_manager.compaction_preserve_tail;

        if self.messages.len() <= preserve_tail + 1 {
            return Ok(());
        }

        let compact_end = self.messages.len() - preserve_tail;
        let to_compact = &self.messages[1..compact_end];
        if to_compact.is_empty() {
            return Ok(());
        }

        let summary = self
            .summarize_messages_with_budget(to_compact, max_summary_tokens)
            .await?;

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
                metadata: zeph_llm::provider::MessageMetadata::agent_only(),
            },
        );

        tracing::info!(
            compacted_count,
            summary_tokens = self.token_counter.count_tokens(&summary),
            "compacted context (with budget)"
        );

        self.recompute_prompt_tokens();
        self.update_metrics(|m| {
            m.context_compactions += 1;
        });

        if let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        {
            let sqlite = memory.sqlite();
            let ids = sqlite
                .oldest_message_ids(cid, u32::try_from(compacted_count + 1).unwrap_or(u32::MAX))
                .await;
            match ids {
                Ok(ids) if ids.len() >= 2 => {
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

    /// Summarize messages with an optional chunk-size budget.
    ///
    /// When `chunk_budget` is `Some(n)`, the token budget per chunk is `n` instead of
    /// the default 4096. This indirectly limits how long summaries are by reducing
    /// how much context is fed to each LLM call.
    async fn summarize_messages_with_budget(
        &self,
        messages: &[Message],
        chunk_budget: Option<usize>,
    ) -> Result<String, super::super::error::AgentError> {
        // Try direct summarization first
        let chunk_token_budget = chunk_budget.unwrap_or(4096);
        let oversized_threshold = chunk_token_budget / 2;

        let chunks = super::chunk_messages(
            messages,
            chunk_token_budget,
            oversized_threshold,
            &self.token_counter,
        );

        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);

        let try_llm = |msgs: &[Message]| {
            let prompt = Self::build_chunk_prompt(msgs);
            let provider = self.summary_or_primary_provider().clone();
            async move {
                tokio::time::timeout(
                    llm_timeout,
                    provider.chat(&[Message {
                        role: Role::User,
                        content: prompt,
                        parts: vec![],
                        metadata: zeph_llm::provider::MessageMetadata::default(),
                    }]),
                )
                .await
                .map_err(|_| zeph_llm::LlmError::Timeout)?
            }
        };

        // For single chunk, summarize directly
        if chunks.len() <= 1 {
            match try_llm(messages).await {
                Ok(s) => {
                    // SEC-02: cap summary length to avoid LLM output expanding context.
                    // Estimate 4 chars per token; cap at 2× the requested budget or 8000 tokens.
                    let cap_chars = chunk_budget.unwrap_or(8_000).saturating_mul(8);
                    return Ok(super::cap_summary(s, cap_chars));
                }
                Err(e) if !e.is_context_length_error() => return Err(e.into()),
                Err(_) => {
                    tracing::warn!(
                        "summarization hit context length error, using metadata fallback"
                    );
                }
            }
            return Ok(Self::build_metadata_summary(messages));
        }

        // Multi-chunk: use the existing summarize_messages logic (chunk_budget only applied to
        // chunk splitting above; consolidated summary uses the default path)
        self.summarize_messages(messages).await
    }
}
