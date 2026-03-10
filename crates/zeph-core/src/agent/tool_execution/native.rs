// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{
    ChatResponse, LlmProvider, Message, MessagePart, Role, ThinkingBlock, ToolDefinition,
};

use super::super::Agent;
use super::{retry_backoff_ms, tool_args_hash, tool_def_to_definition};
use crate::channel::{Channel, StopHint, ToolOutputEvent, ToolStartEvent};
use crate::sanitizer::{ContentSource, ContentSourceKind};
use tracing::Instrument;
use zeph_llm::provider::MAX_TOKENS_TRUNCATION_MARKER;
use zeph_skills::evolution::FailureKind;
use zeph_tools::executor::ToolCall;

impl<C: Channel> Agent<C> {
    pub(super) async fn call_chat_with_tools_retry(
        &mut self,
        tool_defs: &[ToolDefinition],
        max_attempts: usize,
    ) -> Result<Option<ChatResponse>, super::super::error::AgentError> {
        for attempt in 0..max_attempts {
            match self.call_chat_with_tools(tool_defs).await {
                Ok(result) => return Ok(result),
                Err(e) if e.is_context_length_error() && attempt + 1 < max_attempts => {
                    tracing::warn!(
                        attempt,
                        "chat_with_tools context length exceeded, compacting and retrying"
                    );
                    let _ = self
                        .channel
                        .send_status("context too long, compacting...")
                        .await;
                    self.compact_context().await?;
                    let _ = self.channel.send_status("").await;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("loop covers all attempts")
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn process_response_native_tools(
        &mut self,
    ) -> Result<(), super::super::error::AgentError> {
        self.tool_orchestrator.clear_doom_history();
        self.tool_orchestrator.clear_recent_tool_calls();

        let tool_defs: Vec<ToolDefinition> = self
            .tool_executor
            .tool_definitions_erased()
            .iter()
            .map(tool_def_to_definition)
            .collect();

        tracing::debug!(
            tool_count = tool_defs.len(),
            tools = ?tool_defs.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "native tool_use: collected tool definitions"
        );

        if let Some(cached) = self.check_response_cache().await? {
            self.persist_message(Role::Assistant, &cached, &[], false)
                .await;
            self.messages
                .push(Message::from_legacy(Role::Assistant, cached.as_str()));
            if cached.contains(MAX_TOKENS_TRUNCATION_MARKER) {
                let _ = self.channel.send_stop_hint(StopHint::MaxTokens).await;
            }
            self.channel.flush_chunks().await?;
            return Ok(());
        }

        for iteration in 0..self.tool_orchestrator.max_iterations {
            if *self.shutdown.borrow() {
                tracing::info!("native tool loop interrupted by shutdown");
                break;
            }
            if self.cancel_token.is_cancelled() {
                tracing::info!("native tool loop cancelled by user");
                break;
            }

            self.channel.send_typing().await?;

            // Inject any pending LSP notes as a Role::System message before calling
            // the LLM. Stale notes are cleared unconditionally each iteration so they
            // never accumulate when no new notes were produced.
            // Role::System ensures they are skipped by tool-pair summarization.
            #[cfg(feature = "lsp-context")]
            if self.lsp_hooks.is_some() {
                // Clear stale notes before borrowing lsp_hooks mutably (borrow checker).
                self.remove_lsp_messages();
                let tc = std::sync::Arc::clone(&self.token_counter);
                if let Some(ref mut lsp) = self.lsp_hooks
                    && let Some(note_text) = lsp.drain_notes(&tc)
                {
                    self.push_message(zeph_llm::provider::Message::from_legacy(
                        zeph_llm::provider::Role::System,
                        &note_text,
                    ));
                    self.recompute_prompt_tokens();
                }
            }

            if let Some(ref budget) = self.context_manager.budget {
                let used = usize::try_from(self.cached_prompt_tokens).unwrap_or(usize::MAX);
                let threshold = budget.max_tokens() * 4 / 5;
                if used >= threshold {
                    tracing::warn!(
                        iteration,
                        used,
                        threshold,
                        "stopping tool loop: context budget nearing limit"
                    );
                    self.channel
                        .send("Stopping: context window is nearly full.")
                        .await?;
                    break;
                }
            }

            let _ = self.channel.send_status("thinking...").await;
            let chat_result = self.call_chat_with_tools_retry(&tool_defs, 2).await?;
            let _ = self.channel.send_status("").await;

            let Some(chat_result) = chat_result else {
                tracing::debug!("chat_with_tools returned None (timeout)");
                return Ok(());
            };

            tracing::debug!(iteration, ?chat_result, "native tool loop iteration");

            // Text → display and return
            if let ChatResponse::Text(text) = &chat_result {
                // S4 / M4: scan LLM text output for markdown image exfiltration.
                let cleaned = self.scan_output_and_warn(text);
                if !cleaned.is_empty() {
                    let display = self.maybe_redact(&cleaned);
                    self.channel.send(&display).await?;
                }
                self.store_response_in_cache(&cleaned).await;
                self.persist_message(Role::Assistant, &cleaned, &[], false)
                    .await;
                self.messages
                    .push(Message::from_legacy(Role::Assistant, cleaned.as_str()));
                // Emit MaxTokens stop hint before flush when the provider truncated the response.
                if cleaned.contains(MAX_TOKENS_TRUNCATION_MARKER) {
                    let _ = self.channel.send_stop_hint(StopHint::MaxTokens).await;
                }
                self.channel.flush_chunks().await?;
                return Ok(());
            }

            // ToolUse → execute tools and loop
            let ChatResponse::ToolUse {
                text,
                tool_calls,
                thinking_blocks,
            } = chat_result
            else {
                unreachable!();
            };
            self.preserve_thinking_blocks(thinking_blocks);
            self.handle_native_tool_calls(text.as_deref(), &tool_calls)
                .await?;

            // Summarize before pruning: summarizer must see intact tool output content.
            // Pruning runs after so it never destroys content the summarizer needs.
            // Apply deferred summaries immediately after pruning: once a pair's content is
            // replaced with "[pruned]", the cache prefix for that pair is gone and the
            // pre-computed summary should be visible to the LLM on the very next iteration.
            self.maybe_summarize_tool_pair().await;
            let keep_recent = 2 * self.memory_state.tool_call_cutoff + 2;
            self.prune_stale_tool_outputs(keep_recent);
            self.maybe_apply_deferred_summaries();

            if self.check_doom_loop(iteration).await? {
                break;
            }
        }

        // Signal that the turn ended because the iteration limit was reached,
        // not because the model produced a natural end-of-turn response.
        let _ = self.channel.send_stop_hint(StopHint::MaxTurnRequests).await;
        self.channel.flush_chunks().await?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn call_chat_with_tools(
        &mut self,
        tool_defs: &[ToolDefinition],
    ) -> Result<Option<ChatResponse>, super::super::error::AgentError> {
        if let Some(ref tracker) = self.cost_tracker
            && let Err(e) = tracker.check_budget()
        {
            self.channel
                .send(&format!("Budget limit reached: {e}"))
                .await?;
            return Ok(None);
        }

        tracing::debug!(
            tool_count = tool_defs.len(),
            provider_name = self.provider.name(),
            "call_chat_with_tools"
        );
        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);
        let start = std::time::Instant::now();

        let dump_id = self
            .debug_state
            .debug_dumper
            .as_ref()
            .map(|d: &crate::debug_dump::DebugDumper| d.dump_request(&self.messages));

        let llm_span = tracing::info_span!("llm_call", model = %self.runtime.model_name);
        let chat_fut = tokio::time::timeout(
            llm_timeout,
            self.provider
                .chat_with_tools(&self.messages, tool_defs)
                .instrument(llm_span),
        );
        let timeout_result = tokio::select! {
            r = chat_fut => r,
            () = self.cancel_token.cancelled() => {
                tracing::info!("chat_with_tools cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                return Ok(None);
            }
        };
        let result = if let Ok(result) = timeout_result {
            result?
        } else {
            self.channel
                .send("LLM request timed out. Please try again.")
                .await?;
            return Ok(None);
        };

        let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let prompt_estimate = self.cached_prompt_tokens;
        let completion_heuristic = match &result {
            ChatResponse::Text(t) => u64::try_from(t.len()).unwrap_or(0) / 4,
            ChatResponse::ToolUse {
                text, tool_calls, ..
            } => {
                let text_len = text.as_deref().map_or(0, str::len);
                let calls_len: usize = tool_calls
                    .iter()
                    .map(|c| c.name.len() + c.input.to_string().len())
                    .sum();
                u64::try_from(text_len + calls_len).unwrap_or(0) / 4
            }
        };
        let (final_prompt, final_completion) = self
            .provider
            .last_usage()
            .unwrap_or((prompt_estimate, completion_heuristic));
        let router_stats = self.provider.router_thompson_stats();
        self.update_metrics(|m| {
            m.api_calls += 1;
            m.last_llm_latency_ms = latency;
            m.context_tokens = final_prompt;
            m.prompt_tokens += final_prompt;
            m.completion_tokens += final_completion;
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
            if !router_stats.is_empty() {
                m.router_thompson_stats = router_stats;
            }
        });
        self.record_cache_usage();
        self.record_cost(final_prompt, final_completion);

        if let Some((input_tokens, output_tokens)) = self.provider.last_usage() {
            let context_window =
                u64::try_from(self.provider.context_window().unwrap_or(0)).unwrap_or(0);
            let _ = self
                .channel
                .send_usage(input_tokens, output_tokens, context_window)
                .await;
        }

        if let (Some(d), Some(id)) = (self.debug_state.debug_dumper.as_ref(), dump_id) {
            let dump_text = match &result {
                ChatResponse::Text(t) => t.clone(),
                ChatResponse::ToolUse {
                    text, tool_calls, ..
                } => {
                    let calls = serde_json::to_string_pretty(tool_calls).unwrap_or_default();
                    format!(
                        "{}\n\n---TOOL_CALLS---\n{calls}",
                        text.as_deref().unwrap_or("")
                    )
                }
            };
            d.dump_response(id, &dump_text);
        }

        Ok(Some(result))
    }

    /// Prepend thinking blocks to the last assistant message in the context as `MessagePart`s.
    ///
    /// The Claude API requires `thinking`/`redacted_thinking` blocks to be preserved verbatim
    /// in the assistant message when tool results are sent back in multi-turn conversations.
    fn preserve_thinking_blocks(&mut self, blocks: Vec<ThinkingBlock>) {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = self.messages.last_mut()
            && last.role == Role::Assistant
        {
            let mut thinking_parts: Vec<MessagePart> = blocks
                .into_iter()
                .map(|b| match b {
                    ThinkingBlock::Thinking {
                        thinking,
                        signature,
                    } => MessagePart::ThinkingBlock {
                        thinking,
                        signature,
                    },
                    ThinkingBlock::Redacted { data } => MessagePart::RedactedThinkingBlock { data },
                })
                .collect();
            // Thinking blocks must appear before text/tool_use in the assistant message.
            thinking_parts.append(&mut last.parts);
            last.parts = thinking_parts;
            last.rebuild_content();
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn handle_native_tool_calls(
        &mut self,
        text: Option<&str>,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<(), super::super::error::AgentError> {
        // S4: scan text accompanying ToolUse responses for markdown image exfiltration.
        let cleaned_text: Option<String> = if let Some(t) = text
            && !t.is_empty()
        {
            Some(self.scan_output_and_warn(t))
        } else {
            None
        };

        if let Some(ref t) = cleaned_text
            && !t.is_empty()
        {
            let display = self.maybe_redact(t);
            self.channel.send(&display).await?;
        }

        let mut parts: Vec<MessagePart> = Vec::new();
        if let Some(ref t) = cleaned_text
            && !t.is_empty()
        {
            parts.push(MessagePart::Text { text: t.clone() });
        }
        for tc in tool_calls {
            parts.push(MessagePart::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.input.clone(),
            });
        }
        let assistant_msg = Message::from_parts(Role::Assistant, parts);
        self.persist_message(
            Role::Assistant,
            &assistant_msg.content,
            &assistant_msg.parts,
            false,
        )
        .await;
        self.push_message(assistant_msg);

        // Build tool calls for all requests
        let calls: Vec<ToolCall> = tool_calls
            .iter()
            .map(|tc| {
                let params: serde_json::Map<String, serde_json::Value> =
                    if let serde_json::Value::Object(map) = &tc.input {
                        map.clone()
                    } else {
                        serde_json::Map::new()
                    };
                ToolCall {
                    tool_id: tc.name.clone(),
                    params,
                }
            })
            .collect();

        // Assign stable IDs before execution so ToolStart and ToolOutput share the same ID.
        let tool_call_ids: Vec<String> = tool_calls
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        let tool_started_ats: Vec<std::time::Instant> = tool_calls
            .iter()
            .map(|_| std::time::Instant::now())
            .collect();
        for (tc, tool_call_id) in tool_calls.iter().zip(tool_call_ids.iter()) {
            let raw_params = tc.input.clone();
            self.channel
                .send_tool_start(ToolStartEvent {
                    tool_name: &tc.name,
                    tool_call_id,
                    params: Some(raw_params),
                    parent_tool_use_id: self.parent_tool_use_id.clone(),
                })
                .await?;
        }

        // Validate tool call arguments against URLs seen in flagged untrusted content (flag-only).
        for tc in tool_calls {
            let args_json = tc.input.to_string();
            let url_events = self.security.exfiltration_guard.validate_tool_call(
                &tc.name,
                &args_json,
                &self.security.flagged_urls,
            );
            if !url_events.is_empty() {
                tracing::warn!(
                    tool = %tc.name,
                    count = url_events.len(),
                    "exfiltration guard: suspicious URLs in tool arguments (flag-only, not blocked)"
                );
                self.update_metrics(|m| {
                    m.exfiltration_tool_urls_flagged += url_events.len() as u64;
                });
                self.push_security_event(
                    crate::metrics::SecurityEventCategory::ExfiltrationBlock,
                    &tc.name,
                    format!(
                        "{} suspicious URL(s) flagged in tool args",
                        url_events.len()
                    ),
                );
            }
        }

        // Repeat-detection (CRIT-3): record LLM-initiated calls BEFORE execution.
        // Retry re-executions must NOT be pushed here — they are handled inside the retry loop.
        // Build args hashes and check for repeats. Blocked calls get a pre-built error result.
        let args_hashes: Vec<u64> = calls.iter().map(|c| tool_args_hash(&c.params)).collect();
        let repeat_blocked: Vec<bool> = calls
            .iter()
            .zip(args_hashes.iter())
            .map(|(call, &hash)| {
                let blocked = self.tool_orchestrator.is_repeat(&call.tool_id, hash);
                if blocked {
                    tracing::warn!(
                        tool = %call.tool_id,
                        "[repeat-detect] identical tool call detected, skipping execution"
                    );
                }
                blocked
            })
            .collect();
        // Push LLM-initiated calls into the repeat-detection window (even if blocked).
        for (call, &hash) in calls.iter().zip(args_hashes.iter()) {
            self.tool_orchestrator.push_tool_call(&call.tool_id, hash);
        }

        // Inject active skill secrets before tool execution
        self.inject_active_skill_env();

        // Execute tool calls with retry for transient errors.
        // Retries do NOT produce a new LLM turn and therefore do NOT consume the outer
        // max_tool_iterations budget — the budget only decrements on LLM round-trips.
        // The retry budget per tool call is bounded independently by max_tool_retries.
        let max_retries = self.tool_orchestrator.max_tool_retries;
        let max_parallel = self.runtime.timeouts.max_parallel_tools;
        let cancel = self.cancel_token.clone();

        // For repeat-blocked calls, produce error result immediately without execution.
        // For other calls, execute with retry for transient errors.
        // We run each call sequentially when retry is needed to avoid complex async bookkeeping
        // for the retry state across parallel futures.
        let mut tool_results: Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>> =
            Vec::with_capacity(calls.len());

        for ((call, tc), &blocked) in calls
            .iter()
            .zip(tool_calls.iter())
            .zip(repeat_blocked.iter())
        {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("tool execution cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                // Persist tombstone ToolResult for all tool_calls so the assistant ToolUse
                // persisted above is always paired in the DB (prevents cross-session orphan).
                self.persist_cancelled_tool_results(tool_calls).await;
                return Ok(());
            }

            if blocked {
                let msg = format!(
                    "[error] Repeated identical call to {} detected. \
                     Use different arguments or a different approach.",
                    tc.name
                );
                tool_results.push(Ok(Some(zeph_tools::ToolOutput {
                    tool_name: tc.name.clone(),
                    summary: msg,
                    blocks_executed: 0,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                })));
                continue;
            }

            // Execute with retry for transient errors.
            let mut attempt = 0_usize;
            let result = loop {
                let exec_result = tokio::select! {
                    r = self.tool_executor.execute_tool_call_erased(call).instrument(
                        tracing::info_span!("tool_exec", tool_name = %tc.name, idx = %tc.id)
                    ) => r,
                    () = cancel.cancelled() => {
                        self.tool_executor.set_skill_env(None);
                        tracing::info!("tool execution cancelled by user");
                        self.update_metrics(|m| m.cancellations += 1);
                        self.channel.send("[Cancelled]").await?;
                        // Persist tombstone ToolResult for all tool_calls so the assistant ToolUse
                        // persisted above is always paired in the DB (prevents cross-session orphan).
                        self.persist_cancelled_tool_results(tool_calls).await;
                        return Ok(());
                    }
                };

                match exec_result {
                    Err(ref e)
                        if e.kind() == zeph_tools::ErrorKind::Transient
                            && attempt < max_retries =>
                    {
                        attempt += 1;
                        let delay_ms = retry_backoff_ms(attempt - 1);
                        tracing::warn!(
                            tool = %tc.name,
                            attempt,
                            delay_ms,
                            error = %e,
                            "transient tool error, retrying with backoff"
                        );
                        let _ = self
                            .channel
                            .send_status(&format!("Retrying {}...", tc.name))
                            .await;
                        // Interruptible backoff sleep (IMP-3): cancelled if agent shuts down.
                        tokio::select! {
                            () = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                            () = cancel.cancelled() => {
                                self.tool_executor.set_skill_env(None);
                                tracing::info!("retry backoff interrupted by cancellation");
                                self.update_metrics(|m| m.cancellations += 1);
                                self.channel.send("[Cancelled]").await?;
                                return Ok(());
                            }
                        }
                        let _ = self.channel.send_status("").await;
                        // NOTE: retry re-executions are NOT recorded in repeat-detection (CRIT-3).
                    }
                    result => break result,
                }
            };

            if let Err(ref e) = result
                && let Some(ref d) = self.debug_state.debug_dumper
            {
                d.dump_tool_error(&tc.name, e);
            }
            tool_results.push(result);
        }

        // Pad with empty results if needed (should not happen, but defensive)
        while tool_results.len() < tool_calls.len() {
            tool_results.push(Ok(None));
        }

        self.tool_executor.set_skill_env(None);

        // NOTE: parallel execution is intentionally replaced with sequential retry-aware
        // execution above. For the common case (max_retries=0 or no transient errors),
        // performance is equivalent. Parallel optimization can be restored in a future PR
        // if profiling shows it matters. The max_parallel config is preserved for future use.
        let _ = max_parallel;

        // Collect (name, params, output) for LSP hooks. Built during the results loop below.
        #[cfg(feature = "lsp-context")]
        let mut lsp_tool_calls: Vec<(String, serde_json::Value, String)> = Vec::new();

        // Process results sequentially (metrics, channel sends, message parts)
        let mut result_parts: Vec<MessagePart> = Vec::new();
        // Accumulates injection flags across all tools in the batch (Bug #1490 fix).
        let mut has_any_injection_flags = false;
        for ((((idx, tc), tool_result), tool_call_id), started_at) in tool_calls
            .iter()
            .enumerate()
            .zip(tool_results)
            .zip(tool_call_ids.iter())
            .zip(tool_started_ats.iter())
        {
            let (output, is_error, diff, inline_stats, _, kept_lines, locations) =
                match tool_result {
                    Ok(Some(out)) => {
                        if let Some(ref fs) = out.filter_stats {
                            let saved = fs.estimated_tokens_saved() as u64;
                            let raw = (fs.raw_chars / 4) as u64;
                            let confidence = fs.confidence;
                            let was_filtered = fs.filtered_chars < fs.raw_chars;
                            self.update_metrics(|m| {
                                m.filter_raw_tokens += raw;
                                m.filter_saved_tokens += saved;
                                m.filter_applications += 1;
                                m.filter_total_commands += 1;
                                if was_filtered {
                                    m.filter_filtered_commands += 1;
                                }
                                if let Some(c) = confidence {
                                    match c {
                                        zeph_tools::FilterConfidence::Full => {
                                            m.filter_confidence_full += 1;
                                        }
                                        zeph_tools::FilterConfidence::Partial => {
                                            m.filter_confidence_partial += 1;
                                        }
                                        zeph_tools::FilterConfidence::Fallback => {
                                            m.filter_confidence_fallback += 1;
                                        }
                                    }
                                }
                            });
                        }
                        let inline_stats = out.filter_stats.as_ref().and_then(|fs| {
                            (fs.filtered_chars < fs.raw_chars).then(|| fs.format_inline(&tc.name))
                        });
                        let kept = out.filter_stats.as_ref().and_then(|fs| {
                            (!fs.kept_lines.is_empty()).then(|| fs.kept_lines.clone())
                        });
                        let streamed = out.streamed;
                        let locations = out.locations;
                        (
                            out.summary,
                            false,
                            out.diff,
                            inline_stats,
                            streamed,
                            kept,
                            locations,
                        )
                    }
                    Ok(None) => (
                        "(no output)".to_owned(),
                        false,
                        None,
                        None,
                        false,
                        None,
                        None,
                    ),
                    Err(e) => (format!("[error] {e}"), true, None, None, false, None, None),
                };

            // Record skill learning outcomes for the native tool path (mirrors legacy path in
            // handle_tool_result). Must happen before processing so self_reflection can inject
            // corrective context before the result is persisted to message history.
            if output.contains("[error]") || output.contains("[exit code") {
                let kind = FailureKind::from_error(&output);
                self.record_skill_outcomes("tool_failure", Some(&output), Some(kind.as_str()))
                    .await;
                // Sanitize before passing to self_reflection: tool output from native calls can
                // contain untrusted content with injection patterns. Use ToolResult (ExternalUntrusted)
                // as the appropriate source kind for native tool call output.
                let sanitized_out = self
                    .security
                    .sanitizer
                    .sanitize(&output, ContentSource::new(ContentSourceKind::ToolResult))
                    .body;
                if !self.learning_engine.was_reflection_used() {
                    match self
                        .attempt_self_reflection(&sanitized_out, &sanitized_out)
                        .await
                    {
                        Ok(true) => {
                            // Push ToolResult for the current (failing) tool and synthetic results
                            // for any remaining unexecuted tools so every ToolUse in the assistant
                            // message has a matching ToolResult. Without this the conversation history
                            // contains orphaned ToolUse blocks that cause Claude API 400 errors on the
                            // next request (issue #1512).
                            result_parts.push(MessagePart::ToolResult {
                                tool_use_id: tc.id.clone(),
                                content: sanitized_out.clone(),
                                is_error: true,
                            });
                            for remaining_tc in tool_calls.iter().skip(idx + 1) {
                                result_parts.push(MessagePart::ToolResult {
                                    tool_use_id: remaining_tc.id.clone(),
                                    content: "[skipped: prior tool failed]".to_owned(),
                                    is_error: true,
                                });
                            }
                            let user_msg = Message::from_parts(Role::User, result_parts);
                            // has_injection_flags=false is safe here: `sanitized_out` already passed
                            // through ContentSanitizer above (line ~1763), and "[skipped: prior tool
                            // failed]" is a hardcoded constant with no external content.
                            self.persist_message(
                                Role::User,
                                &user_msg.content,
                                &user_msg.parts,
                                false,
                            )
                            .await;
                            self.push_message(user_msg);
                            return Ok(());
                        }
                        Ok(false) => {
                            // Self-reflection declined or not applicable; continue normal processing.
                        }
                        Err(e) => {
                            // Self-reflection failed. Push ToolResults for all tool calls so
                            // the conversation is never left with orphaned ToolUse blocks (#1517).
                            result_parts.push(MessagePart::ToolResult {
                                tool_use_id: tc.id.clone(),
                                content: output.clone(),
                                is_error,
                            });
                            for remaining_tc in tool_calls.iter().skip(idx + 1) {
                                result_parts.push(MessagePart::ToolResult {
                                    tool_use_id: remaining_tc.id.clone(),
                                    content:
                                        "[error] self-reflection failed before result was processed"
                                            .to_owned(),
                                    is_error: true,
                                });
                            }
                            let user_msg = Message::from_parts(Role::User, result_parts);
                            self.persist_message(
                                Role::User,
                                &user_msg.content,
                                &user_msg.parts,
                                false,
                            )
                            .await;
                            self.push_message(user_msg);
                            return Err(e);
                        }
                    }
                }
            } else {
                self.record_skill_outcomes("success", None, None).await;
            }

            let processed = self.maybe_summarize_tool_output(&output).await;
            let body = if let Some(ref stats) = inline_stats {
                format!("{stats}\n{processed}")
            } else {
                processed.clone()
            };
            let body_display = self.maybe_redact(&body);
            self.channel
                .send_tool_output(ToolOutputEvent {
                    tool_name: &tc.name,
                    body: &body_display,
                    diff,
                    filter_stats: inline_stats,
                    kept_lines,
                    locations,
                    tool_call_id,
                    is_error,
                    parent_tool_use_id: self.parent_tool_use_id.clone(),
                    raw_response: None,
                    started_at: Some(*started_at),
                })
                .await?;

            // Sanitize tool output before inserting into LLM message history (Bug #1490 fix).
            // sanitize_tool_output is the sole sanitization point for tool output data flows.
            // channel send above uses raw `body`; LLM sees only the sanitized version.
            let (llm_content, tool_had_injection_flags) =
                self.sanitize_tool_output(&processed, &tc.name).await;
            has_any_injection_flags |= tool_had_injection_flags;

            // Capture tool call details for LSP hooks before building result part.
            #[cfg(feature = "lsp-context")]
            if !is_error {
                lsp_tool_calls.push((tc.name.clone(), tc.input.clone(), llm_content.clone()));
            }

            result_parts.push(MessagePart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: llm_content,
                is_error,
            });
        }

        let user_msg = Message::from_parts(Role::User, result_parts);
        // flagged_urls accumulates across ALL tools in this batch (cross-tool trust boundary).
        // A URL from tool N's output can flag tool M's arguments even if tool M returned clean
        // output. has_any_injection_flags covers pure text injections (no URL); flagged_urls
        // covers URL-based exfiltration. Both are OR-combined for conservative guarding.
        // Individual per-tool granularity would require separate persist_message calls per
        // result, which would change message history structure.
        let tool_results_have_flags =
            has_any_injection_flags || !self.security.flagged_urls.is_empty();
        self.persist_message(
            Role::User,
            &user_msg.content,
            &user_msg.parts,
            tool_results_have_flags,
        )
        .await;
        self.push_message(user_msg);

        // Fire LSP hooks for each completed tool call (non-blocking: diagnostics fetch
        // is spawned in background; hover calls are awaited but short-lived).
        // `lsp_tool_calls` collects (name, params, output) tuples built during the
        // results loop above. They are captured into a separate Vec so we can call
        // `&mut self.lsp_hooks` without conflicting borrows.
        #[cfg(feature = "lsp-context")]
        if self.lsp_hooks.is_some() {
            let tc_arc = std::sync::Arc::clone(&self.token_counter);
            let sanitizer = self.security.sanitizer.clone();
            for (name, input, output) in lsp_tool_calls {
                if let Some(ref mut lsp) = self.lsp_hooks {
                    lsp.after_tool(&name, &input, &output, &tc_arc, &sanitizer)
                        .await;
                }
            }
        }

        Ok(())
    }

    /// Persist a tombstone `ToolResult` (`is_error=true`) for every tool call in `tool_calls`.
    ///
    /// Called on early-return cancellation paths where the assistant `ToolUse` message was already
    /// persisted but the matching user `ToolResult` message was not yet written. Without this, the
    /// DB contains an orphaned `ToolUse` that will trigger a Claude API 400 on the next session.
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
