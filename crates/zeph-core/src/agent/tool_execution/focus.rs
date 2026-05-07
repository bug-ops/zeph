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
            self.msg.messages.push(assistant_msg);
        }
        self.recompute_prompt_tokens();
        // C1 fix: mark compacted so maybe_compact() does not double-fire this turn.
        // cooldown=0: focus truncation does not impose post-compaction cooldown.
        self.context_manager.compaction =
            crate::agent::context_manager::CompactionState::CompactedThisTurn { cooldown: 0 };

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

        self.services
            .focus
            .append_llm_knowledge(summary.trim().to_owned());
        self.apply_compression_removals(to_remove_indices);

        self.context_manager.compaction =
            crate::agent::context_manager::CompactionState::CompactedThisTurn { cooldown: 0 };
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
    fn select_messages_for_compression(
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

        let to_compress: Vec<zeph_llm::provider::Message> = to_remove_indices
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
        self.rebuild_knowledge_block();
    }

    /// Persist a tombstone `ToolResult` (`is_error=true`) for every tool call in `tool_calls`.
    ///
    /// Called on early-return cancellation paths where the assistant `ToolUse` message was already
    /// persisted but the matching user `ToolResult` message was not yet written. Without this, the
    /// DB contains an orphaned `ToolUse` that will trigger a Claude API 400 on the next session.
    #[tracing::instrument(
        name = "core.tool.persist_cancelled_tool_results",
        skip_all,
        level = "debug"
    )]
    pub(crate) async fn persist_cancelled_tool_results(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) {
        let result_parts: Vec<MessagePart> = tool_calls
            .iter()
            .map(|tc| MessagePart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: "[Cancelled]".to_owned(),
                is_error: true,
            })
            .collect();
        let user_msg = Message::from_parts(Role::User, result_parts);
        self.persist_message(Role::User, &user_msg.content, &user_msg.parts, false)
            .await;
        self.push_message(user_msg);
    }
}

/// Build the LLM prompt messages used to summarize a slice of conversation messages.
///
/// The returned vec contains a system instruction and a user message with a numbered
/// bullet list of the messages to summarize (each truncated to 500 chars).
fn build_compression_prompt(
    to_compress: &[zeph_llm::provider::Message],
) -> Vec<zeph_llm::provider::Message> {
    let role_label = |role: &zeph_llm::provider::Role| match role {
        zeph_llm::provider::Role::User => "user",
        zeph_llm::provider::Role::Assistant => "assistant",
        zeph_llm::provider::Role::System => "system",
    };
    let bullet_list: String = to_compress
        .iter()
        .enumerate()
        .map(|(i, m)| {
            format!(
                "{}. [{}] {}",
                i + 1,
                role_label(&m.role),
                m.content.chars().take(500).collect::<String>()
            )
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
