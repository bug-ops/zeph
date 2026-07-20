// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_common::text::estimate_tokens;
use zeph_llm::provider::{Message, MessagePart, Role};

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    pub(crate) fn handle_focus_tool(
        &mut self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> (String, Option<zeph_llm::provider::Message>) {
        match tool_name {
            "start_focus" => self.start_focus_tool(input),
            "complete_focus" => self.complete_focus_tool(input),
            other => (format!("[error] Unknown focus tool: {other}"), None),
        }
    }

    /// Execute the `start_focus` branch: activate a focus session and return the checkpoint message.
    fn start_focus_tool(
        &mut self,
        input: &serde_json::Value,
    ) -> (String, Option<zeph_llm::provider::Message>) {
        let scope = input
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("(unspecified)")
            .to_string();

        if self.services.focus.is_active() {
            return (
                "[error] A focus session is already active. Call complete_focus first.".to_string(),
                None,
            );
        }

        let marker = self.services.focus.start(scope.clone());

        // Build a checkpoint message carrying the marker UUID so complete_focus can
        // locate the boundary even after intervening compaction.
        // S5 fix: focus_pinned=true ensures compaction never evicts this message.
        // Returned as a pending side-effect so it is inserted AFTER the tool-result
        // User message, maintaining valid OpenAI message ordering (#3262).
        let checkpoint_msg = zeph_llm::provider::Message {
            role: zeph_llm::provider::Role::System,
            content: format!("[focus checkpoint: {scope}]"),
            parts: vec![],
            metadata: zeph_llm::provider::MessageMetadata {
                focus_pinned: true,
                focus_marker_id: Some(marker),
                ..zeph_llm::provider::MessageMetadata::agent_only()
            },
        };

        (
            format!("Focus session started. Checkpoint ID: {marker}. Scope: {scope}"),
            Some(checkpoint_msg),
        )
    }

    /// Execute the `complete_focus` branch: finalize the session and rebuild the knowledge block.
    fn complete_focus_tool(
        &mut self,
        input: &serde_json::Value,
    ) -> (String, Option<zeph_llm::provider::Message>) {
        let summary = input
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // S4: verify focus session is active.
        if !self.services.focus.is_active() {
            return (
                "[error] No active focus session. Call start_focus first.".to_string(),
                None,
            );
        }

        let Some(marker) = self.services.focus.active_marker else {
            return (
                "[error] Internal error: active_marker is None.".to_string(),
                None,
            );
        };

        // S4: find the checkpoint message by marker UUID.
        let checkpoint_pos = self
            .msg
            .messages
            .iter()
            .position(|m| m.metadata.focus_marker_id == Some(marker));
        let Some(checkpoint_pos) = checkpoint_pos else {
            return (
                format!(
                    "[error] Checkpoint marker {marker} not found in message history. \
                     The focus session may have been evicted by compaction."
                ),
                None,
            );
        };

        // The checkpoint and bracketed messages are removed from history.
        // The slice is available for future semantic use but not re-summarized here
        // to avoid LLM overhead.
        let _ = self.msg.messages[checkpoint_pos + 1..].to_vec();

        // Sanitize the LLM-supplied summary before storing it to the pinned Knowledge
        // block. The summary may summarize transitive external content (web scrapes,
        // MCP responses), so use WebScrape (ExternalUntrusted trust level) for stricter
        // spotlighting than ToolResult (SEC-CC-03).
        let sanitized_summary = self
            .services
            .security
            .sanitizer
            .sanitize(
                &summary,
                zeph_sanitizer::ContentSource::new(zeph_sanitizer::ContentSourceKind::WebScrape),
            )
            .body;

        self.services
            .focus
            .append_llm_knowledge(sanitized_summary.clone());
        if let Some(ref d) = self.runtime.debug.debug_dumper {
            let kb = self
                .services
                .focus
                .knowledge_blocks
                .iter()
                .map(|b| b.content.as_str())
                .collect::<Vec<_>>()
                .join("\n---\n");
            d.dump_focus_knowledge(&kb);
        }
        self.services.focus.complete();

        // Remove the checkpoint and all messages after it (bracketed phase cleanup).
        // Guard: when complete_focus is called in the same batch as other tools, the
        // current turn's assistant message (tool_calls) was already pushed at an index
        // > checkpoint_pos and would be erased by truncate(). Preserve it so the
        // subsequent tool results have a valid parent message (OpenAI 422 guard — #3476).
        let current_turn_assistant = {
            let last_idx = self.msg.messages.len().saturating_sub(1);
            if last_idx >= checkpoint_pos {
                self.msg.messages.last().and_then(|m| {
                    if m.role == Role::Assistant
                        && m.parts
                            .iter()
                            .any(|p| matches!(p, MessagePart::ToolUse { .. }))
                    {
                        Some(m.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        };
        self.msg.messages.truncate(checkpoint_pos);
        if let Some(assistant_msg) = current_turn_assistant {
            // TODO(critic): focus checkpoint re-pushes a ToolUse-only assistant message;
            // no pairable ToolResult present, so magic-doc detection is intentionally
            // skipped here.
            self.msg.messages.push(assistant_msg);
        }
        self.msg.recompute_non_system_count();
        self.recompute_prompt_tokens();
        // C1 fix: mark compacted so maybe_compact() does not double-fire this turn.
        // cooldown=0: focus truncation does not impose post-compaction cooldown.
        self.context_manager.set_compaction_state(
            crate::agent::context_manager::CompactionState::CompactedThisTurn { cooldown: 0 },
        );

        self.rebuild_knowledge_block();

        (
            format!("Focus session complete. Knowledge block updated with: {sanitized_summary}"),
            None,
        )
    }

    /// Remove any existing (non-checkpoint) Knowledge block and insert an updated one after the
    /// system prompt. Called after focus completion and context compression.
    pub(crate) fn rebuild_knowledge_block(&mut self) {
        // Remove any existing Knowledge block (focus_pinned=true, no marker_id).
        // Checkpoints have focus_marker_id set and must be preserved.
        self.msg
            .messages
            .retain(|m| !(m.metadata.focus_pinned && m.metadata.focus_marker_id.is_none()));
        if let Some(kb_msg) = self.services.focus.build_knowledge_message() {
            // Insert the Knowledge block right after the system prompt (index 1).
            if self.msg.messages.is_empty() {
                self.msg.messages.push(kb_msg);
            } else {
                self.msg.messages.insert(1, kb_msg);
            }
        }
        self.msg.recompute_non_system_count();
        self.recompute_prompt_tokens();
    }

    /// Handle the `compress_context` tool call (#2218).
    ///
    /// Summarizes non-pinned conversation history, appends to the Knowledge block, and removes
    /// the compressed messages from context. Returns a string result to the LLM.
    ///
    /// Guards:
    /// - Returns error if a focus session is active (would interfere with focus boundaries).
    /// - Returns error if a compression is already in progress (concurrency guard).
    #[tracing::instrument(name = "core.tool.handle_compress_context", skip_all, level = "debug")]
    pub(crate) async fn handle_compress_context(&mut self) -> String {
        use zeph_llm::provider::LlmProvider as _;

        if self.services.focus.is_active() {
            return "[error] Cannot compress context while a focus session is active. \
                    Call complete_focus first."
                .to_string();
        }
        if !self.services.focus.try_acquire_compression() {
            return "[error] A context compression is already in progress.".to_string();
        }

        let preserve_tail = self.context_manager.compaction_preserve_tail;
        let (to_remove_indices, to_compress) =
            match self.select_messages_for_compression(preserve_tail) {
                Ok(pair) => pair,
                Err(total) => {
                    self.services.focus.release_compression();
                    return format!(
                        "Not enough messages to compress (found {total}, need at least {}).",
                        preserve_tail + 4
                    );
                }
            };

        let compress_total = to_compress.len();
        let summary_messages = build_compression_prompt(&to_compress);
        // PAAC secret masking (#5437) is structural at the provider boundary — `compress_provider`
        // (or the primary provider fallback) masks registered secrets from `summary_messages`
        // transparently before this dispatch leaves the process.
        let compress_provider = self
            .runtime
            .providers
            .compress_provider
            .as_ref()
            .unwrap_or(&self.provider);
        let summary = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            compress_provider.chat(&summary_messages),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                self.services.focus.release_compression();
                return format!("[error] Compression LLM call failed: {e}");
            }
            Err(_) => {
                self.services.focus.release_compression();
                return "[error] Compression LLM call timed out.".to_string();
            }
        };

        if summary.trim().is_empty() {
            self.services.focus.release_compression();
            return "[error] Compression produced an empty summary.".to_string();
        }

        let tokens_freed = to_compress
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum::<usize>();

        // Sanitize the LLM-supplied summary before storing it to the pinned Knowledge
        // block, mirroring complete_focus_tool (SEC-CC-03). The summary may transitively
        // carry injected instructions from compressed tool/web content, so use WebScrape
        // (ExternalUntrusted trust level) for stricter spotlighting than ToolResult.
        let sanitized_summary = self
            .services
            .security
            .sanitizer
            .sanitize(
                summary.trim(),
                zeph_sanitizer::ContentSource::new(zeph_sanitizer::ContentSourceKind::WebScrape),
            )
            .body;

        self.services.focus.append_llm_knowledge(sanitized_summary);
        self.apply_compression_removals(to_remove_indices);

        self.context_manager.set_compaction_state(
            crate::agent::context_manager::CompactionState::CompactedThisTurn { cooldown: 0 },
        );
        self.services.focus.release_compression();

        format!(
            "Compressed {compress_total} messages into a summary (~{tokens_freed} tokens freed). \
             Knowledge block updated."
        )
    }

    /// Collect the set of message indices and cloned messages eligible for compression.
    ///
    /// Returns `None` (with the compressible count) when the history is too short (fewer than
    /// `preserve_tail + 4` compressible messages). Returns `Some` with the removal set and
    /// the messages to summarize when compression can proceed.
    pub(crate) fn select_messages_for_compression(
        &self,
        preserve_tail: usize,
    ) -> Result<
        (
            std::collections::HashSet<usize>,
            Vec<zeph_llm::provider::Message>,
        ),
        usize,
    > {
        let compressible_indices: Vec<usize> = self
            .msg
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| !m.metadata.focus_pinned && m.role != zeph_llm::provider::Role::System)
            .map(|(i, _)| i)
            .collect();

        let total = compressible_indices.len();
        if total <= preserve_tail + 3 {
            return Err(total);
        }

        let to_remove_indices: std::collections::HashSet<usize> = compressible_indices
            [..total.saturating_sub(preserve_tail)]
            .iter()
            .copied()
            .collect();

        let mut sorted_indices: Vec<usize> = to_remove_indices.iter().copied().collect();
        sorted_indices.sort_unstable();
        let to_compress: Vec<zeph_llm::provider::Message> = sorted_indices
            .iter()
            .map(|&i| self.msg.messages[i].clone())
            .collect();

        Ok((to_remove_indices, to_compress))
    }

    /// Remove messages at the given indices (in reverse order) then rebuild the Knowledge block.
    fn apply_compression_removals(&mut self, to_remove_indices: std::collections::HashSet<usize>) {
        // Reverse-order removal preserves earlier indices.
        let mut remove_idx = to_remove_indices.into_iter().collect::<Vec<_>>();
        remove_idx.sort_unstable_by(|a, b| b.cmp(a));
        for idx in remove_idx {
            if idx < self.msg.messages.len() {
                self.msg.messages.remove(idx);
            }
        }
        self.msg.recompute_non_system_count();
        self.rebuild_knowledge_block();
    }

    /// Persist a tombstone `ToolResult` (`is_error=true`) for every tool call in `tool_calls` that
    /// does not already have one.
    ///
    /// Called on early-return cancellation paths where the assistant `ToolUse` message was already
    /// persisted but the matching user `ToolResult` message was not yet written. Without this, the
    /// DB contains an orphaned `ToolUse` that will trigger a Claude API 400 on the next session.
    ///
    /// Idempotency guard (#5513): skips any `tool_use_id` that already has a `ToolResult` (real or
    /// tombstone) earlier in the *current turn*, so a caller that (mistakenly, or via a future
    /// defect) invokes this more than once for the same batch cannot write duplicate/contradicting
    /// results.
    ///
    /// The scan is scoped to messages from the current turn's assistant `ToolUse` message onward
    /// (found as the most recent `Role::Assistant` message, mirroring
    /// `shutdown::flush_orphaned_tool_use_on_shutdown`), not the whole history. Some providers
    /// (e.g. Ollama, which assigns `tool_call` ids as `format!("call_{i}")` by batch index) reuse
    /// the same `tool_use_id` across turns; scanning full history would treat an earlier turn's
    /// legitimate result as covering this turn's call and wrongly skip its tombstone.
    ///
    /// `insert_at` places the tombstone at a specific index instead of the true end of history —
    /// required by `flush_orphaned_tool_use_on_shutdown`, which can run after a later turn's
    /// message has already been appended past the still-orphaned assistant `ToolUse`; splicing the
    /// tombstone immediately after that assistant message (rather than after the later message)
    /// preserves the "`ToolUse` immediately followed by `ToolResult`" invariant. All in-turn
    /// callers (`tier_loop.rs`) pass `None`: nothing can have been appended after the `ToolUse`
    /// message yet at that point in the turn, so append-at-end and insert-after-orphan coincide.
    #[tracing::instrument(
        name = "core.tool.persist_cancelled_tool_results",
        skip_all,
        level = "debug"
    )]
    pub(crate) async fn persist_cancelled_tool_results(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        insert_at: Option<usize>,
    ) {
        let turn_start = self
            .msg
            .messages
            .iter()
            .rposition(|m| m.role == Role::Assistant)
            .unwrap_or(0);
        let already_resolved: std::collections::HashSet<&str> = self.msg.messages[turn_start..]
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| {
                if let MessagePart::ToolResult { tool_use_id, .. } = p {
                    Some(tool_use_id.as_str())
                } else {
                    None
                }
            })
            .collect();

        let result_parts: Vec<MessagePart> = tool_calls
            .iter()
            .filter(|tc| !already_resolved.contains(tc.id.as_str()))
            .map(|tc| MessagePart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: "[Cancelled]".to_owned(),
                is_error: true,
            })
            .collect();
        if result_parts.is_empty() {
            return;
        }
        let user_msg = Message::from_parts(Role::User, result_parts);
        self.persist_message(Role::User, &user_msg.content, &user_msg.parts, false)
            .await;
        match insert_at {
            Some(index) => self.insert_message(index, user_msg),
            None => self.push_message(user_msg),
        }
    }

    /// Handle the `request_compaction` tool call (ARC #4020).
    ///
    /// Delegates to `handle_compress_context` with additional guards:
    /// - Rate limit: reuses `CompactionState` — fires at most once per turn.
    /// - Minimum context threshold: only fires when above the soft compaction threshold.
    /// - Reason is truncated to 256 chars before logging (critic note N5).
    #[tracing::instrument(name = "agent.request_compaction", skip_all, level = "info")]
    pub(crate) async fn handle_request_compaction(&mut self, input: &serde_json::Value) -> String {
        let raw_reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("(no reason provided)");

        // Truncate reason to 256 chars at a valid UTF-8 char boundary.
        let reason = &raw_reason[..raw_reason.floor_char_boundary(256.min(raw_reason.len()))];

        // CompactionState is the single authority for per-turn rate limiting.
        if self
            .context_manager
            .compaction_state()
            .is_compacted_this_turn()
        {
            return "[error] Compaction already performed this turn. Try again next turn."
                .to_string();
        }

        // Only compact when context usage is above the soft threshold.
        let cached = self.runtime.providers.cached_prompt_tokens;
        let tier = self.context_manager.compaction_tier(cached);
        if matches!(tier, zeph_context::manager::CompactionTier::None) {
            return format!(
                "Context usage is below the compaction threshold. \
                 No compaction needed at this time ({cached} tokens cached)."
            );
        }

        tracing::info!(reason, "agent requested compaction (ARC)");

        self.handle_compress_context().await
    }
}

/// Build the LLM prompt messages used to summarize a slice of conversation messages.
///
/// The returned vec contains a system instruction and a user message with a numbered
/// bullet list of the messages to summarize (each truncated to 500 chars).
///
/// Untrusted tool-result content is already spotlight-wrapped (`<tool-output>`/
/// `<external-data>`, see `ContentSanitizer::apply_spotlight`) at write time by
/// `sanitize_tool_output` — the sole sanitization point for tool output — before it ever
/// reaches `Message.content`, so it is not raw/unfiltered as originally assumed. A single
/// message can even bundle multiple concatenated wrappers of different kinds, from a
/// multi-tool-call turn. The blind 500-char truncation here can sever a trailing wrapper's
/// closing tag for long untrusted content, leaving an opened-but-never-closed spotlight
/// block in the compression prompt (#6584). This repairs wrapper integrity by re-closing
/// the tag when truncation cut it off; it does not add new sanitization or filtering —
/// that already happened upstream.
fn build_compression_prompt(
    to_compress: &[zeph_llm::provider::Message],
) -> Vec<zeph_llm::provider::Message> {
    let role_label = |role: &zeph_llm::provider::Role| match role {
        zeph_llm::provider::Role::Assistant => "assistant",
        zeph_llm::provider::Role::System => "system",
        zeph_llm::provider::Role::User | _ => "user",
    };
    let bullet_list: String = to_compress
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let truncated: String = m.content.chars().take(500).collect();
            let content = repair_truncated_spotlight_wrapper(truncated, m.metadata.trust_level);
            format!("{}. [{}] {}", i + 1, role_label(&m.role), content)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let total = to_compress.len();
    let system_content = "You are a context compression agent. \
        Summarize the following conversation messages into a concise, information-dense summary. \
        Preserve key facts, decisions, and context. Strip filler and small talk. \
        Output ONLY the summary — no headers, no preamble.";

    vec![
        zeph_llm::provider::Message {
            role: zeph_llm::provider::Role::System,
            content: system_content.to_owned(),
            parts: vec![],
            metadata: zeph_llm::provider::MessageMetadata::default(),
        },
        zeph_llm::provider::Message {
            role: zeph_llm::provider::Role::User,
            content: format!("Summarize these {total} conversation messages:\n\n{bullet_list}"),
            parts: vec![],
            metadata: zeph_llm::provider::MessageMetadata::default(),
        },
    ]
}

/// Re-close spotlight wrappers (`<tool-output>`/`<external-data>`) whose closing tag was
/// severed by the 500-char truncation in [`build_compression_prompt`] (#6584).
///
/// A single compressed message can bundle multiple concatenated tool-result wrappers of
/// *different* kinds: `tier_loop.rs` flattens every `ToolResult` part of a multi-tool-call
/// turn into one `Message`, and `build_tool_output_source` maps different tools to different
/// wrapper kinds (e.g. `shell` → `<tool-output>`, `web_search` → `<external-data>`), tagged
/// with the batch's overall worst-case `trust_level`. So both wrapper types are checked
/// independently by open/close tag count, regardless of which trust tier the message carries
/// as a whole — presence-only checks or gating on `trust_level` to pick a single tag family
/// would miss an earlier-in-batch wrapper of the other kind. Truncation can only ever leave
/// the trailing wrapper unclosed (anything fully preceding it in the string was complete
/// before truncation reached it), so "open count > close count" reliably identifies exactly
/// one trailing instance needing repair, per wrapper type.
///
/// `trust_level` is used only as a cheap short-circuit: `Trusted` content (`None`/`Some(0)`)
/// is never wrapped at write time, so skip the scan entirely. Detecting an open tag without
/// its matching close is safe against spoofing: `ContentSanitizer::escape_delimiter_tags`
/// HTML-entity-escapes any `<tool-output`/`</tool-output`/`<external-data`/`</external-data`
/// look-alike substrings found *within* the real tool content before the genuine wrapper is
/// applied around it, so a literal, unescaped tag can only be one `apply_spotlight` added —
/// never attacker-controlled content impersonating one.
fn repair_truncated_spotlight_wrapper(mut content: String, trust_level: Option<u8>) -> String {
    if matches!(trust_level, None | Some(0)) {
        return content;
    }
    if content.matches("<tool-output").count() > content.matches("</tool-output>").count() {
        content.push_str("\n\n[END OF TOOL OUTPUT]\n</tool-output>");
    }
    if content.matches("<external-data").count() > content.matches("</external-data>").count() {
        content.push_str("\n\n[END OF EXTERNAL DATA]\n</external-data>");
    }
    content
}

#[cfg(test)]
mod tests {
    use super::repair_truncated_spotlight_wrapper;

    #[test]
    fn repairs_severed_tool_output_close() {
        let content = "<tool-output source=\"tool_result\" name=\"shell\" trust=\"local\">\
                        \n[NOTE: ...]\n\nsome truncated shell output"
            .to_owned();
        let result = repair_truncated_spotlight_wrapper(content, Some(1));
        assert!(result.ends_with("</tool-output>"));
        assert_eq!(result.matches("<tool-output").count(), 1);
        assert_eq!(result.matches("</tool-output>").count(), 1);
    }

    #[test]
    fn repairs_severed_external_data_close() {
        let content = "<external-data source=\"web_scrape\" ref=\"http://example.com\" \
                        trust=\"untrusted\">\n[IMPORTANT: ...]\n\nsome truncated page content"
            .to_owned();
        let result = repair_truncated_spotlight_wrapper(content, Some(2));
        assert!(result.ends_with("</external-data>"));
        assert_eq!(result.matches("<external-data").count(), 1);
        assert_eq!(result.matches("</external-data>").count(), 1);
    }

    #[test]
    fn repairs_only_the_severed_kind_in_a_mixed_batch() {
        // A balanced <tool-output> wrapper followed by a severed <external-data> wrapper,
        // as produced by a multi-tool-call turn (e.g. shell + web_search) flattened into one
        // message and tagged with the batch's worst-case trust_level (ExternalUntrusted).
        let content = "<tool-output source=\"tool_result\" name=\"shell\" trust=\"local\">\
                        \n\nls output\n\n[END OF TOOL OUTPUT]\n</tool-output>\
                        <external-data source=\"web_scrape\" ref=\"http://example.com\" \
                        trust=\"untrusted\">\n[IMPORTANT: ...]\n\ntruncated page"
            .to_owned();
        let result = repair_truncated_spotlight_wrapper(content, Some(2));
        // The already-balanced tool-output wrapper must not be touched again.
        assert_eq!(result.matches("<tool-output").count(), 1);
        assert_eq!(result.matches("</tool-output>").count(), 1);
        // The severed external-data wrapper must be repaired exactly once.
        assert_eq!(result.matches("<external-data").count(), 1);
        assert_eq!(result.matches("</external-data>").count(), 1);
        assert!(result.ends_with("</external-data>"));
    }

    #[test]
    fn trusted_content_is_a_short_circuit_noop_even_with_tag_like_text() {
        // Trusted (None/Some(0)) content is never wrapped at write time, so the function
        // must not scan or mutate it even if it happens to contain tag-like substrings
        // (e.g. the user pasted literal wrapper syntax into a chat message).
        let content = "<tool-output> looks like a wrapper but isn't, and is unbalanced".to_owned();
        assert_eq!(
            repair_truncated_spotlight_wrapper(content.clone(), None),
            content
        );
        assert_eq!(
            repair_truncated_spotlight_wrapper(content.clone(), Some(0)),
            content
        );
    }

    #[test]
    fn intact_wrapper_under_500_chars_is_not_double_appended() {
        let content = "<tool-output source=\"tool_result\" name=\"shell\" trust=\"local\">\
                        \n\nshort output\n\n[END OF TOOL OUTPUT]\n</tool-output>"
            .to_owned();
        let result = repair_truncated_spotlight_wrapper(content.clone(), Some(1));
        assert_eq!(
            result, content,
            "already-balanced wrapper must be left untouched"
        );
    }
}
