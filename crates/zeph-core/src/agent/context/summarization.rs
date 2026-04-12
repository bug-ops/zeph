// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::fmt::Write;

use futures::StreamExt as _;
use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, MessagePart, Role};
use zeph_memory::AnchoredSummary;

use super::super::Agent;
use super::super::context_manager::CompactionTier;
use super::super::tool_execution::OVERFLOW_NOTICE_PREFIX;
use super::CompactionOutcome;
use crate::channel::Channel;
use crate::context::ContextBudget;

/// Extract the overflow UUID from a tool output body, if present.
///
/// The overflow notice has the format:
/// `\n[full output stored — ID: {uuid} — {bytes} bytes, use read_overflow tool to retrieve]`
///
/// Returns the UUID substring on success, or `None` if the notice is absent.
fn extract_overflow_ref(body: &str) -> Option<&str> {
    let start = body.find(OVERFLOW_NOTICE_PREFIX)?;
    let rest = &body[start + OVERFLOW_NOTICE_PREFIX.len()..];
    let end = rest.find(" \u{2014} ")?;
    Some(&rest[..end])
}

impl<C: Channel> Agent<C> {
    pub(super) fn build_chunk_prompt(messages: &[Message], guidelines: &str) -> String {
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

        let guidelines_section = if guidelines.is_empty() {
            String::new()
        } else {
            format!("\n<compression-guidelines>\n{guidelines}\n</compression-guidelines>\n")
        };

        format!(
            "<analysis>\n\
             Analyze this conversation and produce a structured compaction note for self-consumption.\n\
             This note replaces the original messages in your context window — be thorough.\n\
             Longer is better if it preserves actionable detail.\n\
             </analysis>\n\
             {guidelines_section}\n\
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

    /// Build a prompt for structured `AnchoredSummary` output.
    pub(super) fn build_anchored_summary_prompt(messages: &[Message], guidelines: &str) -> String {
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

        let guidelines_section = if guidelines.is_empty() {
            String::new()
        } else {
            format!("\n<compression-guidelines>\n{guidelines}\n</compression-guidelines>\n")
        };

        format!(
            "<analysis>\n\
             You are compacting a conversation into a structured summary for self-consumption.\n\
             This summary replaces the original messages in your context window.\n\
             Every field MUST be populated — empty fields mean lost information.\n\
             </analysis>\n\
             {guidelines_section}\n\
             Produce a JSON object with exactly these 5 fields:\n\
             - session_intent: string — what the user is trying to accomplish\n\
             - files_modified: string[] — file paths, function names, structs touched\n\
             - decisions_made: string[] — each entry: \"Decision: X — Reason: Y\"\n\
             - open_questions: string[] — unresolved questions or blockers\n\
             - next_steps: string[] — concrete next actions\n\
             \n\
             Be thorough. Preserve all file paths, line numbers, error messages, \
             and specific identifiers — they cannot be recovered.\n\
             \n\
             Conversation:\n{history_text}"
        )
    }

    /// Attempt structured summarization via `chat_typed_erased::<AnchoredSummary>()`.
    ///
    /// Returns `Ok(AnchoredSummary)` on success, `Err` when mandatory fields are missing
    /// or the LLM fails. The caller is responsible for falling back to prose on `Err`.
    async fn try_summarize_structured(
        &self,
        messages: &[Message],
        guidelines: &str,
    ) -> Result<AnchoredSummary, zeph_llm::LlmError> {
        let prompt = Self::build_anchored_summary_prompt(messages, guidelines);
        let msgs = [Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);
        let summary: AnchoredSummary = tokio::time::timeout(
            llm_timeout,
            self.summary_or_primary_provider()
                .chat_typed_erased::<AnchoredSummary>(&msgs),
        )
        .await
        .map_err(|_| zeph_llm::LlmError::Timeout)??;

        if !summary.files_modified.is_empty() && summary.decisions_made.is_empty() {
            tracing::warn!("structured summary: decisions_made is empty");
        } else if summary.files_modified.is_empty() {
            tracing::warn!(
                "structured summary: files_modified is empty (may be a pure discussion session)"
            );
        }

        if !summary.is_complete() {
            tracing::warn!(
                session_intent_empty = summary.session_intent.trim().is_empty(),
                next_steps_empty = summary.next_steps.is_empty(),
                "structured summary incomplete: mandatory fields missing, falling back to prose"
            );
            return Err(zeph_llm::LlmError::Other(
                "structured summary missing mandatory fields".into(),
            ));
        }

        if let Err(msg) = summary.validate() {
            tracing::warn!(
                error = %msg,
                "structured summary failed field validation, falling back to prose"
            );
            return Err(zeph_llm::LlmError::Other(msg));
        }

        Ok(summary)
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

    async fn single_pass_summary(
        &self,
        messages: &[Message],
        guidelines: &str,
        timeout: std::time::Duration,
    ) -> Result<String, zeph_llm::LlmError> {
        let prompt = Self::build_chunk_prompt(messages, guidelines);
        let msgs = [Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        tokio::time::timeout(timeout, self.summary_or_primary_provider().chat(&msgs))
            .await
            .map_err(|_| zeph_llm::LlmError::Timeout)?
    }

    #[allow(clippy::too_many_lines)]
    async fn try_summarize_with_llm(
        &self,
        messages: &[Message],
        guidelines: &str,
    ) -> Result<String, zeph_llm::LlmError> {
        const CHUNK_TOKEN_BUDGET: usize = 4096;
        const OVERSIZED_THRESHOLD: usize = CHUNK_TOKEN_BUDGET / 2;

        let chunks = super::chunk_messages(
            messages,
            CHUNK_TOKEN_BUDGET,
            OVERSIZED_THRESHOLD,
            &self.metrics.token_counter,
        );

        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);

        if chunks.len() <= 1 {
            return self
                .single_pass_summary(messages, guidelines, llm_timeout)
                .await;
        }

        // Summarize chunks with bounded concurrency to prevent runaway API calls
        let provider = self.summary_or_primary_provider();
        let guidelines_owned = guidelines.to_string();
        let results: Vec<_> = futures::stream::iter(chunks.iter().map(|chunk| {
            let prompt = Self::build_chunk_prompt(chunk, &guidelines_owned);
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
            return self
                .single_pass_summary(messages, guidelines, llm_timeout)
                .await;
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

        // IMP-01: for the final consolidation, apply structured output when enabled.
        // Per-chunk summaries remain prose; only the consolidation becomes AnchoredSummary.
        if self.memory_state.compaction.structured_summaries {
            let anchored_prompt = format!(
                "<analysis>\n\
                 Merge these partial conversation summaries into a single structured summary.\n\
                 </analysis>\n\
                 \n\
                 Produce a JSON object with exactly these 5 fields:\n\
                 - session_intent: string — what the user is trying to accomplish\n\
                 - files_modified: string[] — file paths, function names, structs touched\n\
                 - decisions_made: string[] — each entry: \"Decision: X — Reason: Y\"\n\
                 - open_questions: string[] — unresolved questions or blockers\n\
                 - next_steps: string[] — concrete next actions\n\
                 \n\
                 Partial summaries:\n{numbered}"
            );
            let anchored_msgs = [Message {
                role: Role::User,
                content: anchored_prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];
            match tokio::time::timeout(
                llm_timeout,
                self.summary_or_primary_provider()
                    .chat_typed_erased::<AnchoredSummary>(&anchored_msgs),
            )
            .await
            {
                Ok(Ok(anchored)) if anchored.is_complete() => {
                    if let Some(ref d) = self.debug_state.debug_dumper {
                        d.dump_anchored_summary(&anchored, false, &self.metrics.token_counter);
                    }
                    return Ok(super::cap_summary(anchored.to_markdown(), 16_000));
                }
                Ok(Ok(anchored)) => {
                    tracing::warn!(
                        "chunked consolidation: structured summary incomplete, falling back to prose"
                    );
                    if let Some(ref d) = self.debug_state.debug_dumper {
                        d.dump_anchored_summary(&anchored, true, &self.metrics.token_counter);
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "chunked consolidation: structured output failed, falling back to prose");
                }
                Err(_) => {
                    tracing::warn!(
                        "chunked consolidation: structured output timed out, falling back to prose"
                    );
                }
            }
        }

        let consolidation_prompt = format!(
            "<analysis>\n\
             Merge these partial conversation summaries into a single structured compaction note.\n\
             Produce exactly these 9 sections covering all partial summaries:\n\
             1. User Intent\n2. Technical Concepts\n3. Files & Code\n4. Errors & Fixes\n\
             5. Problem Solving\n6. User Messages\n7. Pending Tasks\n8. Current Work\n9. Next Step\n\
             </analysis>\n\n\
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
                        let ref_notice = extract_overflow_ref(content).map_or_else(
                            || String::from("[compacted]"),
                            |uuid| {
                                format!(
                                    "[tool output pruned; use read_overflow {uuid} to retrieve]"
                                )
                            },
                        );
                        *content = ref_notice;
                    }
                    MessagePart::ToolOutput {
                        body, compacted_at, ..
                    } => {
                        if compacted_at.is_none() {
                            let ref_notice = extract_overflow_ref(body)
                                .map(|uuid| {
                                    format!(
                                        "[tool output pruned; use read_overflow {uuid} to retrieve]"
                                    )
                                })
                                .unwrap_or_default();
                            *body = ref_notice;
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
        guidelines: &str,
    ) -> Result<String, super::super::error::AgentError> {
        // Density-aware budget partitioning (#2481).
        //
        // When density budgets are configured (non-default or explicitly set), log the split
        // so operators can observe which fraction of content is high vs. low density.
        // The budgets inform future per-density summarization passes (Phase 2).
        {
            use crate::agent::compaction_strategy::partition_by_density;
            let compression = &self.context_manager.compression;
            let high_budget = compression.high_density_budget;
            let low_budget = compression.low_density_budget;
            let (high, low) = partition_by_density(messages);
            tracing::debug!(
                high_density_count = high.len(),
                low_density_count = low.len(),
                high_budget,
                low_budget,
                "compaction: density-aware partition"
            );
        }

        // Structured path: attempt AnchoredSummary when enabled, fall back to prose on failure.
        if self.memory_state.compaction.structured_summaries {
            match self.try_summarize_structured(messages, guidelines).await {
                Ok(anchored) => {
                    if let Some(ref d) = self.debug_state.debug_dumper {
                        d.dump_anchored_summary(&anchored, false, &self.metrics.token_counter);
                    }
                    return Ok(super::cap_summary(anchored.to_markdown(), 16_000));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "structured summarization failed, falling back to prose");
                    if let Some(ref d) = self.debug_state.debug_dumper {
                        let empty = AnchoredSummary {
                            session_intent: String::new(),
                            files_modified: vec![],
                            decisions_made: vec![],
                            open_questions: vec![],
                            next_steps: vec![],
                        };
                        d.dump_anchored_summary(&empty, true, &self.metrics.token_counter);
                    }
                }
            }
        }

        // Try direct summarization first
        match self.try_summarize_with_llm(messages, guidelines).await {
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
            match self.try_summarize_with_llm(&reduced, guidelines).await {
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

    /// Load the current compression guidelines from `SQLite` if the feature is enabled.
    ///
    /// Returns an empty string when the feature is disabled, memory is not initialized,
    /// or the database query fails (non-fatal).
    async fn load_compression_guidelines_if_enabled(&self) -> String {
        let config = &self.memory_state.compaction.compression_guidelines_config;
        if !config.enabled {
            return String::new();
        }
        let Some(memory) = &self.memory_state.persistence.memory else {
            return String::new();
        };
        match memory
            .sqlite()
            .load_compression_guidelines(self.memory_state.persistence.conversation_id)
            .await
        {
            Ok((_, text)) => text,
            Err(e) => {
                tracing::warn!("failed to load compression guidelines: {e:#}");
                String::new()
            }
        }
    }

    /// Archive tool output bodies from `to_compact` messages before compaction (Memex #2432).
    ///
    /// Saves each non-empty, non-already-archived `ToolOutput` body to `tool_overflow`
    /// with `archive_type = 'archive'`. Returns a list of reference strings in the format
    /// `[archived:{uuid} — tool: {tool_name} — {bytes} bytes]` for use as a postfix.
    ///
    /// References are injected AFTER summarization (fix C1: LLM would destroy them).
    /// Returns an empty vec when `archive_tool_outputs` is disabled or memory is unavailable.
    async fn archive_tool_outputs_for_compaction(&self, to_compact: &[Message]) -> Vec<String> {
        if !self.context_manager.compression.archive_tool_outputs {
            return Vec::new();
        }
        let (Some(memory), Some(cid)) = (
            &self.memory_state.persistence.memory,
            self.memory_state.persistence.conversation_id,
        ) else {
            return Vec::new();
        };

        let mut refs = Vec::new();
        let sqlite = memory.sqlite();

        for msg in to_compact {
            for part in &msg.parts {
                if let MessagePart::ToolOutput {
                    body, tool_name, ..
                } = part
                {
                    // Skip empty, already-archived, or already-overflowed bodies.
                    if body.is_empty()
                        || body.starts_with("[archived:")
                        || body.starts_with("[full output stored")
                        || body.starts_with("[tool output pruned")
                    {
                        continue;
                    }
                    match sqlite.save_archive(cid.0, body.as_bytes()).await {
                        Ok(uuid) => {
                            let bytes = body.len();
                            refs.push(format!(
                                "[archived:{uuid} — tool: {tool_name} — {bytes} bytes]"
                            ));
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "Memex: failed to archive tool output (non-fatal)"
                            );
                        }
                    }
                }
            }
        }

        if !refs.is_empty() {
            tracing::debug!(
                archived = refs.len(),
                "Memex: archived tool outputs before compaction"
            );
        }
        refs
    }

    #[allow(clippy::too_many_lines)]
    pub(in crate::agent) async fn compact_context(
        &mut self,
    ) -> Result<CompactionOutcome, super::super::error::AgentError> {
        // Force-apply any pending deferred summaries before draining to avoid losing them (CRIT-01).
        let _ = self.apply_deferred_summaries();

        let preserve_tail = self.context_manager.compaction_preserve_tail;

        if self.msg.messages.len() <= preserve_tail + 1 {
            return Ok(CompactionOutcome::NoChange);
        }

        let compact_end = self.msg.messages.len() - preserve_tail;

        // S1 fix: extract focus-pinned messages before draining so they survive compaction.
        // These are Knowledge block messages created by the Focus Agent (#1850).
        let pinned_messages: Vec<Message> = self.msg.messages[1..compact_end]
            .iter()
            .filter(|m| m.metadata.focus_pinned)
            .cloned()
            .collect();

        // S2 fix (#2022): extract active-subgoal messages before draining so they survive
        // compaction. Mirrors the focus_pinned pattern exactly. Only applies when a subgoal
        // strategy is active and the registry has tagged active messages.
        let active_subgoal_messages: Vec<Message> = if self
            .context_manager
            .compression
            .pruning_strategy
            .is_subgoal()
        {
            use crate::agent::compaction_strategy::SubgoalState;
            self.msg.messages[1..compact_end]
                .iter()
                .enumerate()
                .filter(|(slice_i, m)| {
                    // slice_i is 0-based within [1..compact_end]; actual index = slice_i + 1.
                    let actual_i = slice_i + 1;
                    !m.metadata.focus_pinned
                        && matches!(
                            self.compression.subgoal_registry.subgoal_state(actual_i),
                            Some(SubgoalState::Active)
                        )
                })
                .map(|(_, m)| m.clone())
                .collect()
        } else {
            vec![]
        };

        // Summarize only the non-pinned, non-active-subgoal messages in the compaction range.
        let to_compact: Vec<Message> = {
            let is_subgoal = self
                .context_manager
                .compression
                .pruning_strategy
                .is_subgoal();

            if is_subgoal {
                use crate::agent::compaction_strategy::SubgoalState;
                self.msg.messages[1..compact_end]
                    .iter()
                    .enumerate()
                    .filter(|(slice_i, m)| {
                        let actual_i = slice_i + 1;
                        !m.metadata.focus_pinned
                            && !matches!(
                                self.compression.subgoal_registry.subgoal_state(actual_i),
                                Some(SubgoalState::Active)
                            )
                    })
                    .map(|(_, m)| m.clone())
                    .collect()
            } else {
                self.msg.messages[1..compact_end]
                    .iter()
                    .filter(|m| !m.metadata.focus_pinned)
                    .cloned()
                    .collect()
            }
        };
        if to_compact.is_empty() {
            return Ok(CompactionOutcome::NoChange);
        }

        // Load compression guidelines if configured.
        let guidelines = self.load_compression_guidelines_if_enabled().await;

        // Memex: archive tool output bodies before compaction (#2432).
        //
        // Archives are saved BEFORE summarization, but references are injected AFTER
        // summarization as a postfix (fix C1: LLM would destroy [archived:UUID] markers).
        // The LLM summarizes the original messages without placeholders.
        //
        // Invariant: save_archive() is called before the placeholder is created;
        // the placeholder is only inserted into the postfix, not into `to_compact`.
        let archived_refs: Vec<String> =
            self.archive_tool_outputs_for_compaction(&to_compact).await;

        let summary = self.summarize_messages(&to_compact, &guidelines).await?;

        // Compaction probe: validate summary quality before committing it.
        if self.context_manager.compression.probe.enabled {
            let _ = self
                .channel
                .send_status("Validating compaction quality...")
                .await;
            let probe_result = match zeph_memory::validate_compaction(
                self.probe_or_summary_provider(),
                &to_compact,
                &summary,
                &self.context_manager.compression.probe,
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!("compaction probe error (non-blocking): {e:#}");
                    self.update_metrics(|m| {
                        m.compaction_probe_errors += 1;
                        m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::Error);
                        m.last_probe_score = None;
                        m.last_probe_category_scores = None;
                    });
                    None
                }
            };

            if let Some(ref result) = probe_result {
                if let Some(ref d) = self.debug_state.debug_dumper {
                    d.dump_compaction_probe(result);
                }

                let cat_scores = result.category_scores.clone();
                let probe_threshold = result.threshold;
                let probe_hard_fail_threshold = result.hard_fail_threshold;
                match result.verdict {
                    zeph_memory::ProbeVerdict::HardFail => {
                        tracing::warn!(
                            score = result.score,
                            threshold = result.hard_fail_threshold,
                            "compaction probe HARD FAIL — keeping original messages"
                        );
                        self.update_metrics(|m| {
                            m.compaction_probe_failures += 1;
                            m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::HardFail);
                            m.last_probe_score = Some(result.score);
                            m.last_probe_category_scores = Some(cat_scores.clone());
                            m.compaction_probe_threshold = probe_threshold;
                            m.compaction_probe_hard_fail_threshold = probe_hard_fail_threshold;
                        });
                        return Ok(CompactionOutcome::ProbeRejected);
                    }
                    zeph_memory::ProbeVerdict::SoftFail => {
                        tracing::warn!(
                            score = result.score,
                            threshold = result.threshold,
                            "compaction probe SOFT FAIL — proceeding with warning"
                        );
                        self.update_metrics(|m| {
                            m.compaction_probe_soft_failures += 1;
                            m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::SoftFail);
                            m.last_probe_score = Some(result.score);
                            m.last_probe_category_scores = Some(cat_scores.clone());
                            m.compaction_probe_threshold = probe_threshold;
                            m.compaction_probe_hard_fail_threshold = probe_hard_fail_threshold;
                        });
                    }
                    zeph_memory::ProbeVerdict::Pass => {
                        tracing::info!(score = result.score, "compaction probe passed");
                        self.update_metrics(|m| {
                            m.compaction_probe_passes += 1;
                            m.last_probe_verdict = Some(zeph_memory::ProbeVerdict::Pass);
                            m.last_probe_score = Some(result.score);
                            m.last_probe_category_scores = Some(cat_scores.clone());
                            m.compaction_probe_threshold = probe_threshold;
                            m.compaction_probe_hard_fail_threshold = probe_hard_fail_threshold;
                        });
                    }
                    zeph_memory::ProbeVerdict::Error => {
                        // Unreachable: validate_compaction returns Err on errors, not Ok(Error).
                        // If this fires, the error-handling path in validate_compaction changed.
                        debug_assert!(false, "ProbeVerdict::Error reached inside Ok path");
                    }
                }
            }
        }

        let compacted_count = to_compact.len();
        // Inject archived references as a postfix AFTER the LLM summary (fix C1).
        // The LLM summary is unaware of archives; the postfix ensures references survive.
        let archive_postfix = if archived_refs.is_empty() {
            String::new()
        } else {
            let refs = archived_refs.join("\n");
            format!("\n\n[archived tool outputs — retrievable via read_overflow]\n{refs}")
        };
        let summary_content = format!(
            "[conversation summary — {compacted_count} messages compacted]\n{summary}{archive_postfix}"
        );
        // Drain the original range (includes pinned, active-subgoal, and non-pinned messages).
        self.msg.messages.drain(1..compact_end);
        // Insert the compaction summary at position 1.
        self.msg.messages.insert(
            1,
            Message {
                role: Role::System,
                content: summary_content.clone(),
                parts: vec![],
                metadata: MessageMetadata::agent_only(),
            },
        );
        // Re-insert pinned messages right after the summary (position 2+).
        // They are placed before the preserved tail so the LLM always sees them.
        let pinned_count = pinned_messages.len();
        for (i, pinned) in pinned_messages.into_iter().enumerate() {
            self.msg.messages.insert(2 + i, pinned);
        }
        // Re-insert active-subgoal messages after pinned messages (#2022 S2 fix).
        // Active-subgoal messages are protected from summarization — they carry the current
        // working context and must not be lost during compaction.
        for (i, active_msg) in active_subgoal_messages.into_iter().enumerate() {
            self.msg.messages.insert(2 + pinned_count + i, active_msg);
        }

        // S1 fix (#2022): rebuild subgoal index map from scratch after drain + reinsert.
        // Arithmetic offset is fragile because the final positions depend on pinned_count
        // and active_subgoal_count. Rebuild is O(subgoals * avg_span) — negligible.
        if self
            .context_manager
            .compression
            .pruning_strategy
            .is_subgoal()
        {
            self.compression
                .subgoal_registry
                .rebuild_after_compaction(&self.msg.messages, compact_end);
        }

        tracing::info!(
            compacted_count,
            summary_tokens = self.metrics.token_counter.count_tokens(&summary),
            "compacted context"
        );

        self.recompute_prompt_tokens();
        self.update_metrics(|m| {
            m.context_compactions += 1;
        });

        if let (Some(memory), Some(cid)) = (
            &self.memory_state.persistence.memory,
            self.memory_state.persistence.conversation_id,
        ) {
            // Persist compaction: mark originals as user_only, insert summary as agent_only.
            // Assumption: the system prompt is always the first (oldest) row for this conversation
            // in SQLite — i.e., ids[0] corresponds to self.msg.messages[0] (the system prompt).
            // This holds for normal sessions but may not hold after cross-session restore if a
            // non-system message was persisted first. MVP assumption; document if changed.
            // oldest_message_ids returns ascending order; ids[1..=compacted_count] are the messages
            // that were drained from self.msg.messages[1..compact_end].
            let sqlite = memory.sqlite();
            let ids = sqlite
                .oldest_message_ids(cid, u32::try_from(compacted_count + 1).unwrap_or(u32::MAX))
                .await;
            let mut persist_failed = false;
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
                        persist_failed = true;
                    } else if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary in Qdrant: {e:#}");
                        persist_failed = true;
                    }
                }
                Ok(_) => {
                    // Not enough messages in DB — fall back to legacy summary storage
                    if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary: {e:#}");
                        persist_failed = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to get message ids for compaction: {e:#}");
                    if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary: {e:#}");
                    }
                    persist_failed = true;
                }
            }
            if persist_failed {
                let _ = self
                    .channel
                    .send(
                        "Context compaction failed — response quality may be affected in long \
                         sessions.",
                    )
                    .await;
            }
        }

        Ok(CompactionOutcome::Compacted)
    }

    /// Prune tool output bodies.
    ///
    /// Dispatches to scored pruning when `context-compression` is enabled and the configured
    /// pruning strategy is not `Reactive`. Falls back to oldest-first when the feature is
    /// disabled or the strategy is `Reactive`.
    ///
    /// Returns the number of tokens freed.
    pub(in crate::agent) fn prune_tool_outputs(&mut self, min_to_free: usize) -> usize {
        {
            use crate::config::PruningStrategy;
            match &self.context_manager.compression.pruning_strategy {
                PruningStrategy::TaskAware => {
                    return self.prune_tool_outputs_scored(min_to_free);
                }
                PruningStrategy::Mig => {
                    return self.prune_tool_outputs_mig(min_to_free);
                }
                PruningStrategy::Subgoal => {
                    return self.prune_tool_outputs_subgoal(min_to_free);
                }
                PruningStrategy::SubgoalMig => {
                    return self.prune_tool_outputs_subgoal_mig(min_to_free);
                }
                PruningStrategy::Reactive => {} // fall through to oldest-first
            }
        }
        self.prune_tool_outputs_oldest_first(min_to_free)
    }

    /// Oldest-first (Reactive) tool output pruning.
    ///
    /// This is the non-dispatching inner implementation. Called directly by the dispatcher
    /// when strategy is `Reactive` and by scored strategies as their fallback — the latter
    /// avoids the infinite recursion that would occur if they called `prune_tool_outputs`.
    #[allow(clippy::cast_precision_loss)]
    fn prune_tool_outputs_oldest_first(&mut self, min_to_free: usize) -> usize {
        let protect = self.context_manager.prune_protect_tokens;
        let mut tail_tokens = 0usize;
        let mut protection_boundary = self.msg.messages.len();
        if protect > 0 {
            for (i, msg) in self.msg.messages.iter().enumerate().rev() {
                tail_tokens += self.metrics.token_counter.count_message_tokens(msg);
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
        for msg in &mut self.msg.messages[..protection_boundary] {
            if freed >= min_to_free {
                break;
            }
            // S1 fix: never prune pinned Knowledge block messages (#1850).
            if msg.metadata.focus_pinned {
                continue;
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
                    // Skip already-archived bodies — they are tiny and irretrievable if pruned.
                    && !body.starts_with("[archived:")
                {
                    freed += self.metrics.token_counter.count_tokens(body);
                    let ref_notice = extract_overflow_ref(body)
                        .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                        .unwrap_or_default();
                    freed -= self.metrics.token_counter.count_tokens(&ref_notice);
                    *compacted_at = Some(now);
                    *body = ref_notice;
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

    /// Compute the protection boundary index for `prune_protect_tokens`.
    ///
    /// Messages at or after the returned index must not be evicted. This mirrors the logic in
    /// `prune_tool_outputs_oldest_first` so all pruning paths enforce the same tail protection.
    fn prune_protection_boundary(&self) -> usize {
        let protect = self.context_manager.prune_protect_tokens;
        if protect == 0 {
            return self.msg.messages.len();
        }
        let mut tail_tokens = 0usize;
        let mut boundary = self.msg.messages.len();
        for (i, msg) in self.msg.messages.iter().enumerate().rev() {
            tail_tokens += self.metrics.token_counter.count_message_tokens(msg);
            if tail_tokens >= protect {
                boundary = i;
                break;
            }
            if i == 0 {
                boundary = 0;
            }
        }
        boundary
    }

    /// Task-aware / MIG pruning: score tool outputs by relevance to the current task goal,
    /// then evict lowest-scoring blocks until `min_to_free` tokens are freed.
    ///
    /// Requires `context-compression` feature. Falls back to `prune_tool_outputs()` otherwise.
    ///
    /// ## `SideQuest` interaction contract (S3 from critic review)
    ///
    /// When both `TaskAware` pruning and `SideQuest` are enabled, `SideQuest` is expected to be
    /// disabled by the caller (set `sidequest.enabled = false` when `pruning_strategy` != Reactive).
    /// This is the "Option A" documented in the critic review: the two systems do not share state
    /// at the pruning level. `SideQuest` uses the same `focus_pinned` protection to avoid evicting
    /// Knowledge block content.
    pub(in crate::agent) fn prune_tool_outputs_scored(&mut self, min_to_free: usize) -> usize {
        use crate::agent::compaction_strategy::score_blocks_task_aware;
        use crate::config::PruningStrategy;

        let goal = match &self.context_manager.compression.pruning_strategy {
            PruningStrategy::TaskAware => self.compression.current_task_goal.clone(),
            _ => None,
        };

        let scores = if let Some(ref goal) = goal {
            score_blocks_task_aware(&self.msg.messages, goal, &self.metrics.token_counter)
        } else {
            // No goal available: fall back to oldest-first directly (not through the
            // dispatcher, which would recurse back here — S4 fix).
            return self.prune_tool_outputs_oldest_first(min_to_free);
        };

        if let Some(ref d) = self.debug_state.debug_dumper {
            d.dump_pruning_scores(&scores);
        }

        // Sort ascending by score: lowest relevance first (best eviction candidates)
        let mut sorted_scores = scores;
        sorted_scores.sort_unstable_by(|a, b| {
            a.relevance
                .partial_cmp(&b.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let protection_boundary = self.prune_protection_boundary();

        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();

        let mut pruned_indices = Vec::new();
        for block in &sorted_scores {
            if freed >= min_to_free {
                break;
            }
            // Respect prune_protect_tokens: skip messages in the protected tail.
            if block.msg_index >= protection_boundary {
                continue;
            }
            let msg = &mut self.msg.messages[block.msg_index];
            if msg.metadata.focus_pinned {
                continue;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let MessagePart::ToolOutput {
                    body, compacted_at, ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                {
                    freed += self.metrics.token_counter.count_tokens(body);
                    let ref_notice = extract_overflow_ref(body)
                        .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                        .unwrap_or_default();
                    freed -= self.metrics.token_counter.count_tokens(&ref_notice);
                    *compacted_at = Some(now);
                    *body = ref_notice;
                    modified = true;
                }
            }
            if modified {
                pruned_indices.push(block.msg_index);
            }
        }

        for &idx in &pruned_indices {
            self.msg.messages[idx].rebuild_content();
        }

        if freed > 0 {
            tracing::info!(
                freed,
                pruned = pruned_indices.len(),
                strategy = "task_aware",
                "task-aware pruned tool outputs"
            );
            self.update_metrics(|m| m.tool_output_prunes += 1);
        }
        freed
    }

    /// MIG-scored pruning. Uses relevance − redundancy scoring to identify the best eviction
    /// candidates. Requires `context-compression` feature.
    pub(in crate::agent) fn prune_tool_outputs_mig(&mut self, min_to_free: usize) -> usize {
        use crate::agent::compaction_strategy::score_blocks_mig;

        let goal = self.compression.current_task_goal.as_deref();
        let mut scores = score_blocks_mig(&self.msg.messages, goal, &self.metrics.token_counter);

        if let Some(ref d) = self.debug_state.debug_dumper {
            d.dump_pruning_scores(&scores);
        }

        // Sort ascending by MIG: most negative MIG = highest eviction priority
        scores.sort_unstable_by(|a, b| {
            a.mig
                .partial_cmp(&b.mig)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let protection_boundary = self.prune_protection_boundary();

        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();

        let mut pruned_indices = Vec::new();
        for block in &scores {
            if freed >= min_to_free {
                break;
            }
            // Respect prune_protect_tokens: skip messages in the protected tail.
            if block.msg_index >= protection_boundary {
                continue;
            }
            let msg = &mut self.msg.messages[block.msg_index];
            if msg.metadata.focus_pinned {
                continue;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let MessagePart::ToolOutput {
                    body, compacted_at, ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                {
                    freed += self.metrics.token_counter.count_tokens(body);
                    let ref_notice = extract_overflow_ref(body)
                        .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                        .unwrap_or_default();
                    freed -= self.metrics.token_counter.count_tokens(&ref_notice);
                    *compacted_at = Some(now);
                    *body = ref_notice;
                    modified = true;
                }
            }
            if modified {
                pruned_indices.push(block.msg_index);
            }
        }

        for &idx in &pruned_indices {
            self.msg.messages[idx].rebuild_content();
        }

        if freed > 0 {
            tracing::info!(
                freed,
                pruned = pruned_indices.len(),
                strategy = "mig",
                "MIG-pruned tool outputs"
            );
            self.update_metrics(|m| m.tool_output_prunes += 1);
        }
        freed
    }

    /// Subgoal-aware pruning: score tool outputs by subgoal tier membership and evict
    /// lowest-scoring blocks (outdated first, then completed, never active).
    ///
    /// Active-subgoal tool outputs receive relevance 1.0 and are effectively protected
    /// from eviction as long as lower-tier outputs can satisfy `min_to_free`.
    pub(in crate::agent) fn prune_tool_outputs_subgoal(&mut self, min_to_free: usize) -> usize {
        use crate::agent::compaction_strategy::score_blocks_subgoal;

        if let Some(ref d) = self.debug_state.debug_dumper {
            d.dump_subgoal_registry(&self.compression.subgoal_registry);
        }

        let scores = score_blocks_subgoal(
            &self.msg.messages,
            &self.compression.subgoal_registry,
            &self.metrics.token_counter,
        );

        if let Some(ref d) = self.debug_state.debug_dumper {
            d.dump_pruning_scores(&scores);
        }

        // Sort ascending: lowest relevance = highest eviction priority.
        let mut sorted = scores;
        sorted.sort_unstable_by(|a, b| {
            a.relevance
                .partial_cmp(&b.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        self.evict_sorted_blocks(&sorted, min_to_free, "subgoal")
    }

    /// Subgoal + MIG hybrid pruning: combines subgoal tier relevance with pairwise
    /// redundancy scoring (MIG = relevance − redundancy).
    pub(in crate::agent) fn prune_tool_outputs_subgoal_mig(&mut self, min_to_free: usize) -> usize {
        use crate::agent::compaction_strategy::score_blocks_subgoal_mig;

        if let Some(ref d) = self.debug_state.debug_dumper {
            d.dump_subgoal_registry(&self.compression.subgoal_registry);
        }

        let mut scores = score_blocks_subgoal_mig(
            &self.msg.messages,
            &self.compression.subgoal_registry,
            &self.metrics.token_counter,
        );

        if let Some(ref d) = self.debug_state.debug_dumper {
            d.dump_pruning_scores(&scores);
        }

        // Sort ascending by MIG: most negative MIG = highest eviction priority.
        scores.sort_unstable_by(|a, b| {
            a.mig
                .partial_cmp(&b.mig)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        self.evict_sorted_blocks(&scores, min_to_free, "subgoal_mig")
    }

    /// Shared eviction loop: given a sorted `BlockScore` slice (ascending priority = most evictable
    /// first), evict tool outputs until `min_to_free` tokens are freed or all candidates are
    /// exhausted. Returns tokens freed.
    ///
    /// Extracted to eliminate duplicate code between `prune_tool_outputs_scored`,
    /// `prune_tool_outputs_mig`, and the new subgoal pruning variants.
    fn evict_sorted_blocks(
        &mut self,
        sorted_scores: &[crate::agent::compaction_strategy::BlockScore],
        min_to_free: usize,
        strategy: &str,
    ) -> usize {
        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();

        let mut pruned_indices = Vec::new();
        for block in sorted_scores {
            if freed >= min_to_free {
                break;
            }
            let msg = &mut self.msg.messages[block.msg_index];
            if msg.metadata.focus_pinned {
                continue;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                if let MessagePart::ToolOutput {
                    body, compacted_at, ..
                } = part
                    && compacted_at.is_none()
                    && !body.is_empty()
                {
                    freed += self.metrics.token_counter.count_tokens(body);
                    let ref_notice = extract_overflow_ref(body)
                        .map(|p| format!("[tool output pruned; use read_overflow {p} to retrieve]"))
                        .unwrap_or_default();
                    freed -= self.metrics.token_counter.count_tokens(&ref_notice);
                    *compacted_at = Some(now);
                    *body = ref_notice;
                    modified = true;
                }
            }
            if modified {
                pruned_indices.push(block.msg_index);
            }
        }

        for &idx in &pruned_indices {
            self.msg.messages[idx].rebuild_content();
        }

        if freed > 0 {
            tracing::info!(
                freed,
                pruned = pruned_indices.len(),
                strategy,
                "pruned tool outputs"
            );
            self.update_metrics(|m| m.tool_output_prunes += 1);
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
        if self.msg.messages.len() <= keep_recent + 1 {
            return 0;
        }
        let boundary = self.msg.messages.len().saturating_sub(keep_recent);
        let mut freed = 0usize;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();
        // Skip system prompt (index 0), prune from 1..boundary.
        // Also skip focus-pinned Knowledge block messages (#1850 S1 fix).
        for msg in &mut self.msg.messages[1..boundary] {
            if msg.metadata.focus_pinned {
                continue;
            }
            let mut modified = false;
            for part in &mut msg.parts {
                match part {
                    MessagePart::ToolOutput {
                        body, compacted_at, ..
                    } if compacted_at.is_none() && !body.is_empty() => {
                        freed += self.metrics.token_counter.count_tokens(body);
                        let ref_notice = extract_overflow_ref(body)
                            .map(|p| {
                                format!("[tool output pruned; use read_overflow {p} to retrieve]")
                            })
                            .unwrap_or_default();
                        freed -= self.metrics.token_counter.count_tokens(&ref_notice);
                        *compacted_at = Some(now);
                        *body = ref_notice;
                        modified = true;
                    }
                    MessagePart::ToolResult { content, .. } => {
                        let tokens = self.metrics.token_counter.count_tokens(content);
                        if tokens > 20 {
                            freed += tokens;
                            let ref_notice = extract_overflow_ref(content).map_or_else(
                                || String::from("[pruned]"),
                                |p| {
                                    format!(
                                        "[tool output pruned; use read_overflow {p} to retrieve]"
                                    )
                                },
                            );
                            freed -= self.metrics.token_counter.count_tokens(&ref_notice);
                            *content = ref_notice;
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
        while i < self.msg.messages.len() {
            let msg = &self.msg.messages[i];
            if !msg.metadata.visibility.is_agent_visible() {
                i += 1;
                continue;
            }
            let is_tool_request = msg.role == Role::Assistant
                && msg
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }));
            if is_tool_request && i + 1 < self.msg.messages.len() {
                let next = &self.msg.messages[i + 1];
                if next.metadata.visibility.is_agent_visible()
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
        while i < self.msg.messages.len() {
            let msg = &self.msg.messages[i];
            if !msg.metadata.visibility.is_agent_visible() {
                i += 1;
                continue;
            }
            let is_tool_request = msg.role == Role::Assistant
                && msg
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }));
            if is_tool_request && i + 1 < self.msg.messages.len() {
                let next = &self.msg.messages[i + 1];
                if next.metadata.visibility.is_agent_visible()
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
        self.msg
            .messages
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
        let cutoff = self.memory_state.persistence.tool_call_cutoff;
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
                &self.msg.messages[req_idx],
                &self.msg.messages[resp_idx],
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
            self.msg.messages[resp_idx].metadata.deferred_summary = Some(summary.clone());
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
        for i in 1..self.msg.messages.len() {
            if self.msg.messages[i].metadata.deferred_summary.is_none() {
                continue;
            }
            // Verify the structural invariant: tool response preceded by matching tool request.
            if self.msg.messages[i].role == Role::User
                && self.msg.messages[i].metadata.visibility.is_agent_visible()
                && i > 0
                && self.msg.messages[i - 1].role == Role::Assistant
                && self.msg.messages[i - 1]
                    .metadata
                    .visibility
                    .is_agent_visible()
                && self.msg.messages[i - 1]
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }))
            {
                let summary = self.msg.messages[i]
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
            let req_db_id = self.msg.messages[req_idx].metadata.db_id;
            let resp_db_id = self.msg.messages[resp_idx].metadata.db_id;

            self.msg.messages[req_idx].metadata.visibility =
                zeph_llm::provider::MessageVisibility::UserOnly;
            self.msg.messages[resp_idx].metadata.visibility =
                zeph_llm::provider::MessageVisibility::UserOnly;
            self.msg.messages[resp_idx].metadata.deferred_summary = None;

            if let (Some(req_id), Some(resp_id)) = (req_db_id, resp_db_id) {
                self.msg.deferred_db_hide_ids.push(req_id);
                self.msg.deferred_db_hide_ids.push(resp_id);
                self.msg.deferred_db_summaries.push(summary.clone());
            }

            let content = format!("[tool summary] {summary}");
            let summary_msg = Message {
                role: Role::Assistant,
                content,
                parts: vec![MessagePart::Summary { text: summary }],
                metadata: MessageMetadata::agent_only(),
            };
            self.msg.messages.insert(resp_idx + 1, summary_msg);
        }

        self.recompute_prompt_tokens();
        tracing::info!(count, "applied deferred tool pair summaries");
        count
    }

    pub(in crate::agent) async fn flush_deferred_summaries(&mut self) {
        if self.msg.deferred_db_hide_ids.is_empty() {
            return;
        }
        let (Some(memory), Some(cid)) = (
            &self.memory_state.persistence.memory,
            self.memory_state.persistence.conversation_id,
        ) else {
            self.msg.deferred_db_hide_ids.clear();
            self.msg.deferred_db_summaries.clear();
            return;
        };
        let hide_ids = std::mem::take(&mut self.msg.deferred_db_hide_ids);
        let summaries = std::mem::take(&mut self.msg.deferred_db_summaries);
        if let Err(e) = memory
            .sqlite()
            .apply_tool_pair_summaries(cid, &hide_ids, &summaries)
            .await
        {
            tracing::warn!(error = %e, "failed to flush deferred summary batch to DB");
        }
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
        let count_pressure = pending >= self.memory_state.persistence.tool_call_cutoff;
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
        // Increment the turn counter unconditionally so every user-message turn is counted
        // regardless of early-return guards (exhaustion, server compaction, cooldown).
        if let Some(ref mut count) = self.context_manager.turns_since_last_hard_compaction {
            *count += 1;
        }

        // Guard 3 — Exhaustion: stop compaction permanently when it cannot reduce context.
        // One-shot warning: flip warned → true on first visit, no-op on subsequent calls.
        if let crate::agent::context_manager::CompactionState::Exhausted { ref mut warned } =
            self.context_manager.compaction
            && !*warned
        {
            *warned = true;
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
        if self.context_manager.compaction.is_exhausted() {
            return Ok(());
        }

        // S1: skip client-side compaction when server compaction is active — unless context
        // has grown past 95% of the budget without a server compaction event (safety fallback).
        if self.providers.server_compaction_active {
            let budget = self
                .context_manager
                .budget
                .as_ref()
                .map_or(0, ContextBudget::max_tokens);
            if budget > 0 {
                let total_tokens: usize = self
                    .msg
                    .messages
                    .iter()
                    .map(|m| self.metrics.token_counter.count_message_tokens(m))
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
        if self.context_manager.compaction.is_compacted_this_turn() {
            return Ok(());
        }
        // Guard 1 — Cooldown: skip Hard-tier LLM compaction for N turns after the last successful
        // compaction. Soft compaction (pruning only) is still allowed during cooldown.
        let in_cooldown = self.context_manager.compaction.cooldown_remaining() > 0;
        if in_cooldown {
            // Decrement the Cooling counter in place.
            if let crate::agent::context_manager::CompactionState::Cooling {
                ref mut turns_remaining,
            } = self.context_manager.compaction
            {
                *turns_remaining -= 1;
                if *turns_remaining == 0 {
                    self.context_manager.compaction =
                        crate::agent::context_manager::CompactionState::Ready;
                }
            }
        }

        match self.compaction_tier() {
            CompactionTier::None => Ok(()),
            CompactionTier::Soft => {
                let _ = self.channel.send_status("soft compacting context...").await;

                // Step 0 (context-compression): apply any completed background goal extraction
                // and schedule a new one if the user message has changed (#1909, #2022).
                {
                    use crate::config::PruningStrategy;
                    match &self.context_manager.compression.pruning_strategy {
                        PruningStrategy::Subgoal | PruningStrategy::SubgoalMig => {
                            self.maybe_refresh_subgoal();
                        }
                        _ => self.maybe_refresh_task_goal(),
                    }
                }

                // Step 1: apply deferred tool summaries (free tokens without LLM).
                let applied = self.apply_deferred_summaries();
                let _ = self.apply_deferred_summaries();

                // Step 1b (S5 fix): rebuild subgoal index map if deferred summaries were applied.
                // Deferred summaries insert messages (shifting indices), invalidating msg_to_subgoal.
                if applied > 0
                    && self
                        .context_manager
                        .compression
                        .pruning_strategy
                        .is_subgoal()
                {
                    self.compression.subgoal_registry.rebuild_after_compaction(
                        &self.msg.messages,
                        0, // 0 = no drain, just repair shifted indices
                    );
                }

                // Step 2: prune tool outputs down to soft threshold.
                let budget = self
                    .context_manager
                    .budget
                    .as_ref()
                    .map_or(0, ContextBudget::max_tokens);
                let soft_threshold =
                    (budget as f32 * self.context_manager.soft_compaction_threshold) as usize;
                let cached =
                    usize::try_from(self.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
                let min_to_free = cached.saturating_sub(soft_threshold);
                if min_to_free > 0 {
                    self.prune_tool_outputs(min_to_free);
                }

                let _ = self.channel.send_status("").await;
                tracing::info!(
                    cached_tokens = self.providers.cached_prompt_tokens,
                    soft_threshold,
                    "soft compaction complete"
                );
                // Soft compaction does NOT set compacted_this_turn, allowing Hard to fire
                // in the same turn if context is still above the hard threshold.
                Ok(())
            }
            CompactionTier::Hard => {
                // Track hard compaction event: finalize the previous segment's turn count
                // and start a new one. Counted regardless of cooldown — captures pressure,
                // not just action. When compaction_hard_count == 0, compaction_turns_after_hard
                // is expected to be empty.
                if let Some(turns) = self.context_manager.turns_since_last_hard_compaction {
                    self.update_metrics(|m| {
                        m.compaction_turns_after_hard.push(turns);
                    });
                }
                self.context_manager.turns_since_last_hard_compaction = Some(0);
                self.update_metrics(|m| {
                    m.compaction_hard_count += 1;
                });

                // Cooldown guard: skip LLM summarization while cooling down.
                if in_cooldown {
                    tracing::debug!(
                        turns_remaining = self.context_manager.compaction.cooldown_remaining(),
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
                let cached =
                    usize::try_from(self.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
                let min_to_free = cached.saturating_sub(hard_threshold);

                let _ = self.channel.send_status("compacting context...").await;

                // Step 1: apply deferred summaries first (free tokens without LLM).
                self.apply_deferred_summaries();

                // Step 2: prune tool outputs.
                let freed = self.prune_tool_outputs(min_to_free);
                if freed >= min_to_free {
                    tracing::info!(freed, "hard compaction: pruning sufficient");
                    self.context_manager.compaction =
                        crate::agent::context_manager::CompactionState::CompactedThisTurn {
                            cooldown: self.context_manager.compaction_cooldown_turns,
                        };
                    self.flush_deferred_summaries().await;
                    let _ = self.channel.send_status("").await;
                    return Ok(());
                }

                // Step 3: Guard 2 — Counterproductive: check if there are enough messages
                // to make LLM summarization worthwhile.
                let preserve_tail = self.context_manager.compaction_preserve_tail;
                let compactable = self.msg.messages.len().saturating_sub(preserve_tail + 1);
                if compactable <= 1 {
                    tracing::warn!(
                        compactable,
                        "hard compaction: too few messages to compact, marking exhausted"
                    );
                    // Only reachable from Ready state (Cooling is guarded by in_cooldown above).
                    self.context_manager.compaction =
                        crate::agent::context_manager::CompactionState::Exhausted { warned: false };
                    let _ = self.channel.send_status("").await;
                    return Ok(());
                }

                // Step 4: fall back to full LLM summarization.
                tracing::info!(
                    freed,
                    min_to_free,
                    "hard compaction: pruning insufficient, falling back to LLM summarization"
                );
                let tokens_before = self.providers.cached_prompt_tokens;
                let outcome = self.compact_context().await?;
                match outcome {
                    CompactionOutcome::ProbeRejected => {
                        // Probe rejected the summary. This is NOT exhaustion — the compactor
                        // can still summarize, but the summary was too lossy.
                        // Set cooldown to prevent immediate retry, but do NOT mark Exhausted.
                        tracing::info!("compaction probe rejected summary — setting cooldown");
                        self.context_manager.compaction =
                            crate::agent::context_manager::CompactionState::CompactedThisTurn {
                                cooldown: self.context_manager.compaction_cooldown_turns,
                            };
                        let _ = self
                            .channel
                            .send("Context compaction skipped this turn — will retry.")
                            .await;
                    }
                    CompactionOutcome::Compacted => {
                        // Guard 2 — Counterproductive: net freed tokens is zero (summary ate all
                        // freed space — no net reduction).
                        let freed_tokens =
                            tokens_before.saturating_sub(self.providers.cached_prompt_tokens);
                        if freed_tokens == 0 {
                            tracing::warn!(
                                "hard compaction: summary consumed all freed tokens — no net \
                                 reduction, marking exhausted"
                            );
                            // Only reachable from Ready state (Cooling is guarded by in_cooldown).
                            self.context_manager.compaction =
                                crate::agent::context_manager::CompactionState::Exhausted {
                                    warned: false,
                                };
                            let _ = self.channel.send_status("").await;
                            return Ok(());
                        }
                        // Guard 3 — Still above threshold: compaction freed some tokens but context
                        // remains above the hard threshold; further LLM attempts are unlikely to help.
                        if matches!(self.compaction_tier(), CompactionTier::Hard) {
                            tracing::warn!(
                                freed_tokens,
                                "hard compaction: context still above hard threshold after \
                                 compaction, marking exhausted"
                            );
                            // Only reachable from Ready state (Cooling is guarded by in_cooldown).
                            self.context_manager.compaction =
                                crate::agent::context_manager::CompactionState::Exhausted {
                                    warned: false,
                                };
                            let _ = self.channel.send_status("").await;
                            return Ok(());
                        }
                        self.context_manager.compaction =
                            crate::agent::context_manager::CompactionState::CompactedThisTurn {
                                cooldown: self.context_manager.compaction_cooldown_turns,
                            };
                    }
                    CompactionOutcome::NoChange => {
                        // compact_context() decided there was nothing to compact.
                        // The compactable <= 1 guard above should have caught this, but handle
                        // it gracefully if the messages changed during the async call.
                    }
                }
                let _ = self.channel.send_status("").await;
                Ok(())
            }
        }
    }

    /// Soft-only compaction for mid-iteration use inside tool execution loops.
    ///
    /// Applies deferred tool summaries and prunes tool outputs down to the soft threshold.
    /// Never triggers Hard tier (no LLM call), never increments
    /// `turns_since_last_hard_compaction`, and never decrements the cooldown counter.
    /// Returns immediately when `compacted_this_turn` is set (hard compaction ran earlier
    /// in this turn) or when context usage is below the soft threshold.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub(in crate::agent) fn maybe_soft_compact_mid_iteration(&mut self) {
        if self.context_manager.compaction.is_compacted_this_turn() {
            return;
        }
        if !matches!(
            self.compaction_tier(),
            CompactionTier::Soft | CompactionTier::Hard
        ) {
            return;
        }
        let budget = self
            .context_manager
            .budget
            .as_ref()
            .map_or(0, ContextBudget::max_tokens);
        let soft_threshold =
            (budget as f32 * self.context_manager.soft_compaction_threshold) as usize;
        let cached = usize::try_from(self.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
        // Step 1: apply deferred summaries.
        self.apply_deferred_summaries();
        // Step 2: prune tool outputs down to soft threshold.
        let min_to_free = cached.saturating_sub(soft_threshold);
        if min_to_free > 0 {
            self.prune_tool_outputs(min_to_free);
        }
        tracing::debug!(
            cached_tokens = self.providers.cached_prompt_tokens,
            soft_threshold,
            "mid-iteration soft compaction complete"
        );
    }

    /// Proactive context compression: fires before reactive compaction when context exceeds
    /// the configured `threshold_tokens`. Mutually exclusive with reactive compaction per turn
    /// (guarded by `compacted_this_turn`).
    pub(in crate::agent) async fn maybe_proactive_compress(
        &mut self,
    ) -> Result<(), super::super::error::AgentError> {
        // S1: skip proactive compression when server compaction is active — unless context
        // has grown past 95% of the budget without a server compaction event (safety fallback).
        if self.providers.server_compaction_active {
            let budget = self
                .context_manager
                .budget
                .as_ref()
                .map_or(0, ContextBudget::max_tokens);
            if budget > 0 {
                let fallback_threshold = (budget * 95 / 100) as u64;
                if self.providers.cached_prompt_tokens <= fallback_threshold {
                    return Ok(());
                }
                tracing::warn!(
                    cached_prompt_tokens = self.providers.cached_prompt_tokens,
                    fallback_threshold,
                    "server compaction active but context at 95%+ — falling back to client-side proactive"
                );
            } else {
                return Ok(());
            }
        }
        let Some((_threshold, max_summary_tokens)) = self
            .context_manager
            .should_proactively_compress(self.providers.cached_prompt_tokens)
        else {
            return Ok(());
        };

        let tokens_before = self.providers.cached_prompt_tokens;
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
            // Proactive compression does not impose a post-compaction cooldown.
            self.context_manager.compaction =
                crate::agent::context_manager::CompactionState::CompactedThisTurn { cooldown: 0 };
            let tokens_saved = tokens_before.saturating_sub(self.providers.cached_prompt_tokens);
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

        if self.msg.messages.len() <= preserve_tail + 1 {
            return Ok(());
        }

        let compact_end = self.msg.messages.len() - preserve_tail;
        let to_compact = &self.msg.messages[1..compact_end];
        if to_compact.is_empty() {
            return Ok(());
        }

        // Memex: archive tool output bodies before compaction (#2432).
        let archived_refs: Vec<String> = self.archive_tool_outputs_for_compaction(to_compact).await;

        let summary = self
            .summarize_messages_with_budget(to_compact, max_summary_tokens)
            .await?;

        let compacted_count = to_compact.len();
        let archive_postfix = if archived_refs.is_empty() {
            String::new()
        } else {
            let refs = archived_refs.join("\n");
            format!("\n\n[archived tool outputs — retrievable via read_overflow]\n{refs}")
        };
        let summary_content = format!(
            "[conversation summary — {compacted_count} messages compacted]\n{summary}{archive_postfix}"
        );
        self.msg.messages.drain(1..compact_end);
        self.msg.messages.insert(
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
            summary_tokens = self.metrics.token_counter.count_tokens(&summary),
            "compacted context (with budget)"
        );

        self.recompute_prompt_tokens();
        self.update_metrics(|m| {
            m.context_compactions += 1;
        });

        if let (Some(memory), Some(cid)) = (
            &self.memory_state.persistence.memory,
            self.memory_state.persistence.conversation_id,
        ) {
            let sqlite = memory.sqlite();
            let ids = sqlite
                .oldest_message_ids(cid, u32::try_from(compacted_count + 1).unwrap_or(u32::MAX))
                .await;
            let mut persist_failed = false;
            match ids {
                Ok(ids) if ids.len() >= 2 => {
                    let start = ids[1];
                    let end = ids[compacted_count.min(ids.len() - 1)];
                    if let Err(e) = sqlite
                        .replace_conversation(cid, start..=end, "system", &summary_content)
                        .await
                    {
                        tracing::warn!("failed to persist compaction in sqlite: {e:#}");
                        persist_failed = true;
                    } else if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary in Qdrant: {e:#}");
                        persist_failed = true;
                    }
                }
                Ok(_) => {
                    if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary: {e:#}");
                        persist_failed = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to get message ids for compaction: {e:#}");
                    if let Err(e) = memory.store_session_summary(cid, &summary).await {
                        tracing::warn!("failed to store session summary: {e:#}");
                    }
                    persist_failed = true;
                }
            }
            if persist_failed {
                let _ = self
                    .channel
                    .send(
                        "Context compaction failed — response quality may be affected in long \
                         sessions.",
                    )
                    .await;
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
        let guidelines = self.load_compression_guidelines_if_enabled().await;

        let chunks = super::chunk_messages(
            messages,
            chunk_token_budget,
            oversized_threshold,
            &self.metrics.token_counter,
        );

        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);

        let try_llm = |msgs: &[Message]| {
            let prompt = Self::build_chunk_prompt(msgs, &guidelines);
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
            // Structured path for single-chunk (IMP-02): mirrors summarize_messages().
            if self.memory_state.compaction.structured_summaries {
                match self.try_summarize_structured(messages, &guidelines).await {
                    Ok(anchored) => {
                        if let Some(ref d) = self.debug_state.debug_dumper {
                            d.dump_anchored_summary(&anchored, false, &self.metrics.token_counter);
                        }
                        return Ok(super::cap_summary(anchored.to_markdown(), 16_000));
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "structured summarization (budget path) failed, falling back to prose"
                        );
                        if let Some(ref d) = self.debug_state.debug_dumper {
                            let empty = AnchoredSummary {
                                session_intent: String::new(),
                                files_modified: vec![],
                                decisions_made: vec![],
                                open_questions: vec![],
                                next_steps: vec![],
                            };
                            d.dump_anchored_summary(&empty, true, &self.metrics.token_counter);
                        }
                    }
                }
            }

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
        self.summarize_messages(messages, &guidelines).await
    }

    /// Refresh the cached task goal when the last user message has changed (#1850, #1909).
    ///
    /// Two-phase non-blocking design (mirrors `maybe_sidequest_eviction`):
    ///
    /// - Phase 1 (apply): if a background extraction task spawned last compaction has finished,
    ///   apply its result to `current_task_goal`.
    /// - Phase 2 (schedule): if the user message hash has changed and no task is in-flight,
    ///   spawn a new background `tokio::spawn` for goal extraction. The current compaction uses
    ///   whatever goal was cached from the previous extraction — never blocks.
    ///
    /// This eliminates the 5-second latency spike on every Soft tier compaction that made
    /// `task_aware`/`mig` strategies non-functional for cloud LLM providers.
    #[allow(clippy::too_many_lines)]
    pub(in crate::agent) fn maybe_refresh_task_goal(&mut self) {
        use std::hash::Hash as _;

        use crate::config::PruningStrategy;

        // Only needed when a task-aware or MIG strategy is active.
        match &self.context_manager.compression.pruning_strategy {
            PruningStrategy::Reactive | PruningStrategy::Subgoal | PruningStrategy::SubgoalMig => {
                return;
            }
            PruningStrategy::TaskAware | PruningStrategy::Mig => {}
        }

        // Phase 1: apply background result if the task has completed.
        if self
            .compression
            .pending_task_goal
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            use futures::FutureExt as _;
            if let Some(handle) = self.compression.pending_task_goal.take() {
                if let Some(Ok(Some(goal))) = handle.now_or_never() {
                    tracing::debug!("extract_task_goal: background result applied");
                    self.compression.current_task_goal = Some(goal);
                }
                // Clear spinner on ALL completion paths (success, None result, or task panic).
                if let Some(ref tx) = self.session.status_tx {
                    let _ = tx.send(String::new());
                }
            }
        }

        // Phase 2: do not spawn a second task while one is already in-flight.
        if self.compression.pending_task_goal.is_some() {
            return;
        }

        // Find the last user message content.
        let last_user_content = self
            .msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == zeph_llm::provider::Role::User)
            .map(|m| m.content.as_str())
            .unwrap_or_default();

        if last_user_content.is_empty() {
            return;
        }

        // Compute a hash of the last user message to detect changes (S5).
        let hash = {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            last_user_content.hash(&mut hasher);
            std::hash::Hasher::finish(&hasher)
        };

        // Cache hit: extraction already scheduled or completed for this user message.
        if self.compression.task_goal_user_msg_hash == Some(hash) {
            return;
        }

        // Cache miss: update hash and spawn background extraction.
        self.compression.task_goal_user_msg_hash = Some(hash);

        // Clone only the data needed by the background task (avoids borrowing self).
        let recent: Vec<(zeph_llm::provider::Role, String)> = self
            .msg
            .messages
            .iter()
            .filter(|m| {
                matches!(
                    m.role,
                    zeph_llm::provider::Role::User | zeph_llm::provider::Role::Assistant
                )
            })
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|m| (m.role, m.content.clone()))
            .collect();

        let provider = self.summary_or_primary_provider().clone();

        let handle = tokio::spawn(async move {
            use zeph_llm::provider::{Message, MessageMetadata, Role};

            if recent.is_empty() {
                return None;
            }

            let mut context_text = String::new();
            for (role, content) in &recent {
                let role_str = match role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => "system",
                };
                let preview = if content.len() > 300 {
                    let end = content.floor_char_boundary(300);
                    &content[..end]
                } else {
                    content.as_str()
                };
                let _ =
                    std::fmt::write(&mut context_text, format_args!("[{role_str}]: {preview}\n"));
            }

            let prompt = format!(
                "Extract the current task goal from this conversation excerpt in one concise \
                 sentence.\nFocus on what the user is trying to accomplish right now.\n\
                 Respond with only the goal sentence, no preamble.\n\n\
                 <conversation>\n{context_text}</conversation>"
            );

            let msgs = [Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];

            match tokio::time::timeout(std::time::Duration::from_secs(30), provider.chat(&msgs))
                .await
            {
                Ok(Ok(goal)) => {
                    let trimmed = goal.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        const MAX_GOAL_CHARS: usize = 500;
                        let capped = if trimmed.len() > MAX_GOAL_CHARS {
                            tracing::warn!(
                                len = trimmed.len(),
                                "extract_task_goal: LLM returned oversized goal; truncating to {MAX_GOAL_CHARS} chars"
                            );
                            let end = trimmed.floor_char_boundary(MAX_GOAL_CHARS);
                            &trimmed[..end]
                        } else {
                            trimmed
                        };
                        Some(capped.to_string())
                    }
                }
                Ok(Err(e)) => {
                    tracing::debug!("extract_task_goal: LLM error: {e:#}");
                    None
                }
                Err(_) => {
                    tracing::debug!("extract_task_goal: timed out");
                    None
                }
            }
        });

        // TODO(I3): this JoinHandle is never `.abort()`ed on agent shutdown. The background task
        // will run to completion (or until the 30-second timeout) even after the agent is dropped.
        // Proper cancellation requires surfacing the handle to the shutdown path — tracked as a
        // separate issue (background tasks not cancelled on agent shutdown).
        self.compression.pending_task_goal = Some(handle);
        tracing::debug!("extract_task_goal: background task spawned");
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send("Extracting task goal...".into());
        }
    }

    /// Refresh the subgoal registry when the last user message has changed (#2022).
    ///
    /// Mirrors the two-phase `maybe_refresh_task_goal` pattern exactly:
    ///
    /// - Phase 1 (apply): if the background extraction task from last turn has finished,
    ///   parse the result and update the subgoal registry.
    /// - Phase 2 (schedule): if the user message hash has changed and no task is in-flight,
    ///   spawn a new background extraction. Current compaction uses the cached registry state.
    ///
    /// Transition detection: the LLM's `COMPLETED:` signal drives transitions (S3 fix).
    /// When `COMPLETED: NONE`, the same subgoal continues (`extend_active`).
    /// When `COMPLETED:` is non-NONE, a new subgoal is created (`complete_active` + `push_active`).
    #[allow(clippy::too_many_lines)]
    pub(in crate::agent) fn maybe_refresh_subgoal(&mut self) {
        use std::hash::Hash as _;

        use crate::config::PruningStrategy;

        // Only needed when a subgoal-aware strategy is active.
        match &self.context_manager.compression.pruning_strategy {
            PruningStrategy::Subgoal | PruningStrategy::SubgoalMig => {}
            _ => return,
        }

        let msg_len = self.msg.messages.len();

        // Phase 1: apply background result if the task has completed.
        if self
            .compression
            .pending_subgoal
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            use futures::FutureExt as _;
            if let Some(handle) = self.compression.pending_subgoal.take() {
                if let Some(Ok(Some(result))) = handle.now_or_never() {
                    // Detect subgoal transition via LLM signal (S3 fix).
                    let is_transition = result.completed.is_some();

                    if is_transition {
                        // Complete the current active subgoal and start a new one.
                        if let Some(completed_desc) = &result.completed {
                            tracing::debug!(
                                completed = completed_desc.as_str(),
                                "subgoal transition detected"
                            );
                        }
                        self.compression
                            .subgoal_registry
                            .complete_active(msg_len.saturating_sub(1));
                        let new_id = self
                            .compression
                            .subgoal_registry
                            .push_active(result.current.clone(), msg_len.saturating_sub(1));
                        self.compression
                            .subgoal_registry
                            .extend_active(msg_len.saturating_sub(1));
                        tracing::debug!(
                            current = result.current.as_str(),
                            id = new_id.0,
                            "new active subgoal registered"
                        );
                    } else {
                        // Same subgoal continues — extend or create first subgoal.
                        let is_first = self.compression.subgoal_registry.subgoals.is_empty();
                        if is_first {
                            // First extraction result: create initial subgoal.
                            let id = self
                                .compression
                                .subgoal_registry
                                .push_active(result.current.clone(), msg_len.saturating_sub(1));
                            // S4 fix: retroactively tag all pre-extraction messages [1..msg_len-1].
                            if msg_len > 2 {
                                self.compression
                                    .subgoal_registry
                                    .tag_range(1, msg_len - 2, id);
                            }
                            self.compression
                                .subgoal_registry
                                .extend_active(msg_len.saturating_sub(1));
                            tracing::debug!(
                                current = result.current.as_str(),
                                id = id.0,
                                retroactive_msgs = msg_len.saturating_sub(2),
                                "first subgoal registered with retroactive tagging"
                            );
                        } else {
                            // Extend existing active subgoal.
                            self.compression
                                .subgoal_registry
                                .extend_active(msg_len.saturating_sub(1));
                            tracing::debug!(
                                current = result.current.as_str(),
                                "active subgoal extended"
                            );
                        }
                    }
                }
                // Clear spinner on ALL completion paths (success, None, or panic).
                if let Some(ref tx) = self.session.status_tx {
                    let _ = tx.send(String::new());
                }
            }
        }

        // Phase 2: do not spawn a second task while one is in-flight.
        if self.compression.pending_subgoal.is_some() {
            return;
        }

        // Find the last user message content and check for hash change.
        let last_user_content = self
            .msg
            .messages
            .iter()
            .rev()
            .find(|m| {
                m.role == zeph_llm::provider::Role::User && m.metadata.visibility.is_agent_visible()
            })
            .map(|m| m.content.as_str())
            .unwrap_or_default();

        if last_user_content.is_empty() {
            return;
        }

        let hash = {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            last_user_content.hash(&mut hasher);
            std::hash::Hasher::finish(&hasher)
        };

        if self.compression.subgoal_user_msg_hash == Some(hash) {
            return;
        }
        self.compression.subgoal_user_msg_hash = Some(hash);

        // Clone the last 6 agent-visible messages (M2 fix: only agent_visible, not invisible
        // [tool summary] placeholders) for the extraction prompt.
        let recent: Vec<(zeph_llm::provider::Role, String)> = self
            .msg
            .messages
            .iter()
            .filter(|m| {
                m.metadata.visibility.is_agent_visible()
                    && matches!(
                        m.role,
                        zeph_llm::provider::Role::User | zeph_llm::provider::Role::Assistant
                    )
            })
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|m| (m.role, m.content.clone()))
            .collect();

        let provider = self.summary_or_primary_provider().clone();

        let handle = tokio::spawn(async move {
            use zeph_llm::provider::{Message, MessageMetadata, Role};

            if recent.is_empty() {
                return None;
            }

            let mut context_text = String::new();
            for (role, content) in &recent {
                let role_str = match role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => "system",
                };
                let preview = if content.len() > 300 {
                    let end = content.floor_char_boundary(300);
                    &content[..end]
                } else {
                    content.as_str()
                };
                let _ =
                    std::fmt::write(&mut context_text, format_args!("[{role_str}]: {preview}\n"));
            }

            let prompt = format!(
                "Given this conversation excerpt, identify the agent's CURRENT subgoal in one \
                 sentence. A subgoal is the immediate objective the agent is working toward right \
                 now, not the overall task.\n\n\
                 If the agent just completed a subgoal (answered a question, finished a subtask), \
                 also state the COMPLETED subgoal.\n\n\
                 Respond in this exact format:\n\
                 CURRENT: <one sentence describing current subgoal>\n\
                 COMPLETED: <one sentence describing just-completed subgoal, or NONE>\n\n\
                 <conversation>\n{context_text}</conversation>"
            );

            let msgs = [Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];

            let response = match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                provider.chat(&msgs),
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::debug!("subgoal_extraction: LLM error: {e:#}");
                    return None;
                }
                Err(_) => {
                    tracing::debug!("subgoal_extraction: timed out");
                    return None;
                }
            };

            Some(parse_subgoal_extraction_response(&response))
        });

        self.compression.pending_subgoal = Some(handle);
        tracing::debug!("subgoal_extraction: background task spawned");
        if let Some(ref tx) = self.session.status_tx {
            let _ = tx.send("Tracking subgoal...".into());
        }
    }
}

/// Parse the structured LLM response for subgoal extraction.
///
/// Expected format:
/// ```text
/// CURRENT: <description>
/// COMPLETED: <description or NONE>
/// ```
///
/// Falls back to treating the entire response as the current subgoal on malformed input.
fn parse_subgoal_extraction_response(
    response: &str,
) -> crate::agent::state::SubgoalExtractionResult {
    use crate::agent::state::SubgoalExtractionResult;

    let trimmed = response.trim();

    // Try to extract CURRENT: and COMPLETED: prefixes.
    if let Some(current_pos) = trimmed.find("CURRENT:") {
        let after_current = &trimmed[current_pos + "CURRENT:".len()..];
        let (current_line_raw, remainder_raw) = after_current
            .split_once('\n')
            .map_or((after_current, ""), |(l, r)| (l, r));
        let current_line = current_line_raw.trim();
        let remainder = remainder_raw.trim();

        if current_line.is_empty() {
            // Malformed: treat entire response as current subgoal.
            return SubgoalExtractionResult {
                current: trimmed.to_string(),
                completed: None,
            };
        }

        let current = current_line.to_string();

        let completed = if let Some(comp_pos) = remainder.find("COMPLETED:") {
            let comp_text = remainder[comp_pos + "COMPLETED:".len()..].trim();
            let comp_line = comp_text
                .split('\n')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if comp_line.is_empty() || comp_line.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(comp_line)
            }
        } else {
            None
        };

        return SubgoalExtractionResult { current, completed };
    }

    // Malformed response: treat entire response as current subgoal.
    SubgoalExtractionResult {
        current: trimmed.to_string(),
        completed: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_overflow_ref_returns_uuid_when_present() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let body = format!(
            "some output\n[full output stored \u{2014} ID: {uuid} \u{2014} 12345 bytes, use read_overflow tool to retrieve]"
        );
        assert_eq!(extract_overflow_ref(&body), Some(uuid));
    }

    #[test]
    fn extract_overflow_ref_returns_none_when_absent() {
        let body = "normal small output without overflow notice";
        assert_eq!(extract_overflow_ref(body), None);
    }

    #[test]
    fn extract_overflow_ref_returns_none_for_empty_body() {
        assert_eq!(extract_overflow_ref(""), None);
    }

    #[test]
    fn extract_overflow_ref_handles_notice_at_start() {
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let body = format!(
            "[full output stored \u{2014} ID: {uuid} \u{2014} 9999 bytes, use read_overflow tool to retrieve]"
        );
        assert_eq!(extract_overflow_ref(&body), Some(uuid));
    }

    // T-CRIT-01: prune_tool_outputs must skip focus_pinned messages.
    #[test]
    fn prune_tool_outputs_skips_focus_pinned_messages() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        // Disable tail protection so the pruner can evict all messages in the test.
        agent.context_manager.prune_protect_tokens = 0;
        // Agent::new prepopulates messages[0] with a system prompt.

        // Pinned knowledge block with a large tool output part
        let mut pinned_meta = MessageMetadata::focus_pinned();
        pinned_meta.focus_pinned = true;
        let big_body = "x".repeat(5000);
        let mut pinned_msg = Message {
            role: Role::System,
            content: big_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: big_body.clone(),
                compacted_at: None,
            }],
            metadata: pinned_meta,
        };
        pinned_msg.rebuild_content();
        agent.msg.messages.push(pinned_msg);

        // Non-pinned message with a large tool output
        let big_body2 = "y".repeat(5000);
        let mut normal_msg = Message {
            role: Role::User,
            content: big_body2.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "shell".into(),
                body: big_body2.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        normal_msg.rebuild_content();
        agent.msg.messages.push(normal_msg);

        let freed = agent.prune_tool_outputs(1);

        // messages[0] = agent system prompt, messages[1] = pinned, messages[2] = normal.
        let pinned = &agent.msg.messages[1];
        if let MessagePart::ToolOutput {
            body, compacted_at, ..
        } = &pinned.parts[0]
        {
            assert_eq!(*body, "x".repeat(5000), "pinned body must not be evicted");
            assert!(
                compacted_at.is_none(),
                "pinned compacted_at must remain None"
            );
        }

        // Non-pinned body must be evicted
        let normal = &agent.msg.messages[2];
        if let MessagePart::ToolOutput { compacted_at, .. } = &normal.parts[0] {
            assert!(compacted_at.is_some(), "non-pinned body must be evicted");
        }

        assert!(freed > 0, "must free tokens from non-pinned message");
    }

    // T-CRIT-03: prune_tool_outputs_oldest_first basic ordering.
    #[test]
    fn prune_tool_outputs_oldest_first_evicts_from_front() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        // Disable tail protection so the pruner can evict all messages in the test.
        agent.context_manager.prune_protect_tokens = 0;
        // Agent::new puts system prompt at messages[0]; tool outputs go to indices 1..=3.

        for i in 0..3 {
            let body = format!("tool output {i} {}", "z".repeat(500));
            let mut msg = Message {
                role: Role::User,
                content: body.clone(),
                parts: vec![MessagePart::ToolOutput {
                    tool_name: "shell".into(),
                    body: body.clone(),
                    compacted_at: None,
                }],
                metadata: MessageMetadata::default(),
            };
            msg.rebuild_content();
            agent.msg.messages.push(msg);
        }

        // Evict just enough for the first message; the last two should be intact.
        agent.prune_tool_outputs_oldest_first(1);

        // messages[0] = agent system prompt, messages[1..=3] = ToolOutput messages.
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(
                compacted_at.is_some(),
                "oldest tool output must be evicted first"
            );
        }
        // Second should be intact (we only freed enough for 1)
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[2].parts[0] {
            assert!(
                compacted_at.is_none(),
                "second tool output must still be intact"
            );
        }
    }

    // --- Structured summarization tests ---

    // T-STR-01: build_anchored_summary_prompt embeds conversation and all 5 JSON field names.
    #[test]
    fn build_anchored_summary_prompt_contains_required_fields_and_history() {
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        let messages = vec![
            Message {
                role: Role::User,
                content: "refactor the auth middleware".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::Assistant,
                content: "I will split it into two modules".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];

        let prompt =
            Agent::<crate::agent::tests::agent_tests::MockChannel>::build_anchored_summary_prompt(
                &messages, "",
            );

        // All 5 JSON field names must appear in the prompt.
        assert!(prompt.contains("session_intent"), "missing session_intent");
        assert!(prompt.contains("files_modified"), "missing files_modified");
        assert!(prompt.contains("decisions_made"), "missing decisions_made");
        assert!(prompt.contains("open_questions"), "missing open_questions");
        assert!(prompt.contains("next_steps"), "missing next_steps");

        // Conversation content must be embedded.
        assert!(
            prompt.contains("refactor the auth middleware"),
            "user message not in prompt"
        );
        assert!(
            prompt.contains("I will split it into two modules"),
            "assistant message not in prompt"
        );
    }

    // T-STR-02: build_anchored_summary_prompt injects guidelines when non-empty.
    #[test]
    fn build_anchored_summary_prompt_includes_guidelines() {
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let prompt =
            Agent::<crate::agent::tests::agent_tests::MockChannel>::build_anchored_summary_prompt(
                &messages,
                "focus on file paths",
            );

        assert!(
            prompt.contains("compression-guidelines"),
            "guidelines section missing"
        );
        assert!(
            prompt.contains("focus on file paths"),
            "guidelines content missing"
        );
    }

    // T-STR-03: try_summarize_structured returns Ok(AnchoredSummary) when mock returns valid JSON.
    #[tokio::test]
    async fn try_summarize_structured_returns_anchored_summary_on_valid_json() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::AnchoredSummary;

        let valid_json = serde_json::to_string(&AnchoredSummary {
            session_intent: "Implement auth middleware".into(),
            files_modified: vec!["src/auth.rs".into()],
            decisions_made: vec!["Decision: use JWT — Reason: stateless".into()],
            open_questions: vec![],
            next_steps: vec!["Write tests".into()],
        })
        .unwrap();

        let mut agent = Agent::new(
            mock_provider(vec![valid_json]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.memory_state.compaction.structured_summaries = true;

        let messages = vec![Message {
            role: Role::User,
            content: "implement auth".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.try_summarize_structured(&messages, "").await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        let summary = result.unwrap();
        assert_eq!(summary.session_intent, "Implement auth middleware");
        assert_eq!(summary.files_modified, vec!["src/auth.rs"]);
        assert!(summary.is_complete());
    }

    // T-STR-04: try_summarize_structured returns Err when mandatory fields are missing.
    #[tokio::test]
    async fn try_summarize_structured_returns_err_when_incomplete() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::AnchoredSummary;

        // next_steps is empty → is_complete() returns false → method must return Err.
        let incomplete_json = serde_json::to_string(&AnchoredSummary {
            session_intent: "Some intent".into(),
            files_modified: vec![],
            decisions_made: vec![],
            open_questions: vec![],
            next_steps: vec![], // missing → incomplete
        })
        .unwrap();

        let mut agent = Agent::new(
            mock_provider(vec![incomplete_json]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.memory_state.compaction.structured_summaries = true;

        let messages = vec![Message {
            role: Role::User,
            content: "do something".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.try_summarize_structured(&messages, "").await;
        assert!(
            result.is_err(),
            "expected Err for incomplete summary, got Ok"
        );
    }

    // T-STR-05: try_summarize_structured returns Err when LLM returns invalid JSON.
    #[tokio::test]
    async fn try_summarize_structured_returns_err_on_malformed_json() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        // chat_typed retries once then returns StructuredParse error on bad JSON.
        let bad_json = "this is not json at all".to_string();
        let mut agent = Agent::new(
            mock_provider(vec![bad_json.clone(), bad_json]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.memory_state.compaction.structured_summaries = true;

        let messages = vec![Message {
            role: Role::User,
            content: "summarize".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.try_summarize_structured(&messages, "").await;
        assert!(result.is_err(), "expected Err for malformed JSON, got Ok");
    }

    // T-STR-06: summarize_messages uses prose path when structured_summaries = false.
    #[tokio::test]
    async fn summarize_messages_uses_prose_when_flag_disabled() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        let prose_response = "1. User Intent: test\n2. Files: none".to_string();
        let agent = Agent::new(
            mock_provider(vec![prose_response.clone()]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        // structured_summaries = false by default in Agent::new()

        let messages = vec![Message {
            role: Role::User,
            content: "do a thing".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.summarize_messages(&messages, "").await;
        assert!(result.is_ok(), "prose path must succeed");
        // Prose path returns the raw LLM output (no markdown section headers from AnchoredSummary).
        assert!(
            !result.unwrap().contains("[anchored summary]"),
            "prose path must not produce anchored summary header"
        );
    }

    // T-STR-07: summarize_messages returns markdown with anchored headers when flag enabled.
    #[tokio::test]
    async fn summarize_messages_returns_anchored_markdown_when_flag_enabled() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::AnchoredSummary;

        let valid_json = serde_json::to_string(&AnchoredSummary {
            session_intent: "Build a CLI tool".into(),
            files_modified: vec!["src/cli.rs".into()],
            decisions_made: vec!["Decision: use clap — Reason: ergonomic API".into()],
            open_questions: vec![],
            next_steps: vec!["Add help text".into()],
        })
        .unwrap();

        let mut agent = Agent::new(
            mock_provider(vec![valid_json]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.memory_state.compaction.structured_summaries = true;

        let messages = vec![Message {
            role: Role::User,
            content: "build CLI".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.summarize_messages(&messages, "").await;
        assert!(result.is_ok(), "structured path must succeed");
        let md = result.unwrap();
        assert!(
            md.contains("[anchored summary]"),
            "output must start with anchored summary header"
        );
        assert!(md.contains("## Session Intent"), "missing Session Intent");
        assert!(md.contains("## Next Steps"), "missing Next Steps");
        assert!(
            md.contains("Build a CLI tool"),
            "session_intent content missing"
        );
    }

    // T-STR-08: dump_anchored_summary creates a file with required JSON fields.
    #[test]
    fn dump_anchored_summary_creates_file_with_required_fields() {
        use crate::debug_dump::{DebugDumper, DumpFormat};
        use zeph_memory::{AnchoredSummary, TokenCounter};

        let dir = tempfile::tempdir().expect("tempdir");
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).expect("dumper creation");
        let summary = AnchoredSummary {
            session_intent: "Test dump".into(),
            files_modified: vec!["a.rs".into(), "b.rs".into()],
            decisions_made: vec!["Decision: async — Reason: performance".into()],
            open_questions: vec![],
            next_steps: vec!["Run tests".into()],
        };
        let counter = TokenCounter::new();
        dumper.dump_anchored_summary(&summary, false, &counter);

        // Find the anchored-summary file.
        let entries: Vec<_> = std::fs::read_dir(dumper.dir())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("-anchored-summary.json"))
            })
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "exactly one anchored-summary.json expected"
        );

        let content = std::fs::read_to_string(&entries[0]).expect("read file");
        let v: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert!(
            v.get("section_completeness").is_some(),
            "missing section_completeness"
        );
        assert!(v.get("total_items").is_some(), "missing total_items");
        assert!(v.get("token_estimate").is_some(), "missing token_estimate");
        assert!(v.get("fallback").is_some(), "missing fallback field");
        assert_eq!(v["fallback"], false, "fallback must be false");

        let sc = &v["section_completeness"];
        assert_eq!(sc["session_intent"], true);
        assert_eq!(sc["files_modified"], true);
        assert_eq!(sc["decisions_made"], true);
        assert_eq!(sc["open_questions"], false);
        assert_eq!(sc["next_steps"], true);
    }

    // T-STR-09: dump_anchored_summary with fallback=true sets fallback field correctly.
    #[test]
    fn dump_anchored_summary_fallback_flag_propagated() {
        use crate::debug_dump::{DebugDumper, DumpFormat};
        use zeph_memory::{AnchoredSummary, TokenCounter};

        let dir = tempfile::tempdir().expect("tempdir");
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).expect("dumper creation");
        let empty = AnchoredSummary {
            session_intent: String::new(),
            files_modified: vec![],
            decisions_made: vec![],
            open_questions: vec![],
            next_steps: vec![],
        };
        let counter = TokenCounter::new();
        dumper.dump_anchored_summary(&empty, true, &counter);

        let entries: Vec<_> = std::fs::read_dir(dumper.dir())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("-anchored-summary.json"))
            })
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "exactly one anchored-summary.json expected"
        );

        let content = std::fs::read_to_string(&entries[0]).expect("read file");
        let v: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(v["fallback"], true, "fallback flag must be true");
        assert_eq!(
            v["total_items"], 0,
            "total_items must be 0 for empty summary"
        );
    }

    // T-CRIT-03: prune_tool_outputs_scored basic — lowest-relevance block evicted first.
    #[test]
    fn prune_tool_outputs_scored_evicts_lowest_relevance_first() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use crate::config::PruningStrategy;
        use zeph_llm::provider::{Message, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compression.pruning_strategy = PruningStrategy::TaskAware;
        agent.compression.current_task_goal =
            Some("authentication middleware session token".to_string());
        // Disable tail protection so the pruner can evict all messages in the test.
        agent.context_manager.prune_protect_tokens = 0;
        // Agent::new puts system prompt at messages[0]; rel_msg goes to index 1, irrel_msg to 2.

        // High-relevance: contains goal keywords
        let rel_body = "authentication middleware session token implementation ".repeat(50);
        let mut rel_msg = Message {
            role: Role::User,
            content: rel_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: rel_body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        rel_msg.rebuild_content();
        agent.msg.messages.push(rel_msg);

        // Low-relevance: unrelated content
        let irrel_body = "database migration schema table column index ".repeat(50);
        let mut irrel_msg = Message {
            role: Role::User,
            content: irrel_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: irrel_body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        irrel_msg.rebuild_content();
        agent.msg.messages.push(irrel_msg);

        agent.prune_tool_outputs_scored(1);

        // messages[0] = agent system prompt, messages[1] = rel_msg, messages[2] = irrel_msg.
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[2].parts[0] {
            assert!(
                compacted_at.is_some(),
                "low-relevance block must be evicted"
            );
        }
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(compacted_at.is_none(), "high-relevance block must survive");
        }
    }

    // T-CRIT-04: prune_tool_outputs_mig evicts blocks with lowest MIG score first.
    #[test]
    fn prune_tool_outputs_mig_evicts_lowest_mig_first() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use crate::config::PruningStrategy;
        use zeph_llm::provider::{Message, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compression.pruning_strategy = PruningStrategy::Mig;
        // Set a goal so MIG scorer has context for relevance scoring.
        agent.compression.current_task_goal = Some("authentication token".to_string());
        // Disable tail protection so the pruner can evict all messages in the test.
        agent.context_manager.prune_protect_tokens = 0;

        // High-relevance: repeated goal keywords → high relevance, low redundancy relative to goal
        let rel_body = "authentication token session middleware ".repeat(50);
        let mut rel_msg = Message {
            role: Role::User,
            content: rel_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: rel_body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        rel_msg.rebuild_content();
        agent.msg.messages.push(rel_msg);

        // Low-relevance: unrelated content → low relevance → low MIG → evicted first
        let irrel_body = "database schema table column index ".repeat(50);
        let mut irrel_msg = Message {
            role: Role::User,
            content: irrel_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: irrel_body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        irrel_msg.rebuild_content();
        agent.msg.messages.push(irrel_msg);

        // Ask to free only 1 token — should evict the lowest-MIG block.
        agent.prune_tool_outputs_mig(1);

        // messages[0] = system prompt, messages[1] = rel_msg, messages[2] = irrel_msg.
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[2].parts[0] {
            assert!(
                compacted_at.is_some(),
                "low-MIG (irrelevant) block must be evicted"
            );
        } else {
            panic!("expected ToolOutput at messages[2]");
        }
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(
                compacted_at.is_none(),
                "high-MIG (relevant) block must survive"
            );
        } else {
            panic!("expected ToolOutput at messages[1]");
        }
    }

    // T-CRIT-05: scored pruning respects prune_protect_tokens.
    #[test]
    fn prune_tool_outputs_scored_respects_protect_tokens() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use crate::config::PruningStrategy;
        use zeph_llm::provider::{Message, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compression.pruning_strategy = PruningStrategy::TaskAware;
        agent.compression.current_task_goal = Some("irrelevant goal".to_string());
        // Protect the entire tail (999_999 tokens) — nothing should be evicted.
        agent.context_manager.prune_protect_tokens = 999_999;

        let body = "unrelated content database schema ".repeat(50);
        let mut msg = Message {
            role: Role::User,
            content: body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        msg.rebuild_content();
        agent.msg.messages.push(msg);

        let freed = agent.prune_tool_outputs_scored(1);
        assert_eq!(
            freed, 0,
            "no tokens should be freed when everything is protected"
        );

        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(
                compacted_at.is_none(),
                "protected block must not be evicted"
            );
        } else {
            panic!("expected ToolOutput at messages[1]");
        }
    }

    // T-CRIT-06: MIG pruning respects prune_protect_tokens.
    #[test]
    fn prune_tool_outputs_mig_respects_protect_tokens() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use crate::config::PruningStrategy;
        use zeph_llm::provider::{Message, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compression.pruning_strategy = PruningStrategy::Mig;
        agent.compression.current_task_goal = Some("irrelevant goal".to_string());
        // Protect the entire tail (999_999 tokens) — nothing should be evicted.
        agent.context_manager.prune_protect_tokens = 999_999;

        let body = "unrelated content database schema ".repeat(50);
        let mut msg = Message {
            role: Role::User,
            content: body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        msg.rebuild_content();
        agent.msg.messages.push(msg);

        let freed = agent.prune_tool_outputs_mig(1);
        assert_eq!(
            freed, 0,
            "no tokens should be freed when everything is protected"
        );

        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(
                compacted_at.is_none(),
                "protected block must not be evicted"
            );
        } else {
            panic!("expected ToolOutput at messages[1]");
        }
    }
}

#[cfg(test)]
mod subgoal_extraction_tests {
    use super::*;

    #[test]
    fn parse_well_formed_with_both() {
        let response = "CURRENT: Implement login\nCOMPLETED: Setup database";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current, "Implement login");
        assert_eq!(result.completed, Some("Setup database".to_string()));
    }

    #[test]
    fn parse_well_formed_no_completed() {
        let response = "CURRENT: Fetch user data\nCOMPLETED: NONE";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current, "Fetch user data");
        assert_eq!(result.completed, None);
    }

    #[test]
    fn parse_malformed_no_current_prefix() {
        let response = "Just some random text about subgoals";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current, "Just some random text about subgoals");
        assert_eq!(result.completed, None);
    }

    #[test]
    fn parse_malformed_empty_current() {
        let response = "CURRENT: \nCOMPLETED: Setup";
        let result = parse_subgoal_extraction_response(response);
        // Empty CURRENT falls back to treating entire response as current
        assert_eq!(result.current.trim(), "CURRENT: \nCOMPLETED: Setup");
        assert_eq!(result.completed, None);
    }
}
