// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures::FutureExt as _;
use zeph_llm::provider::{
    ChatResponse, LlmProvider, Message, MessageMetadata, MessagePart, Role, ThinkingBlock,
    ToolDefinition,
};

use super::super::Agent;
use super::{
    AnomalyOutcome, retry_backoff_ms, strip_tafc_fields, tool_args_hash,
    tool_def_to_definition_with_tafc,
};
use crate::channel::{Channel, StopHint, ToolOutputEvent, ToolStartEvent};
use crate::overflow_tools::OverflowToolExecutor;
use tracing::Instrument;
use zeph_llm::provider::MAX_TOKENS_TRUNCATION_MARKER;
use zeph_sanitizer::{ContentSource, ContentSourceKind};
use zeph_skills::evolution::FailureKind;
use zeph_tools::executor::ToolCall;

type ToolExecFut = futures::future::BoxFuture<
    'static,
    Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
>;

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
                Err(e) if e.is_beta_header_rejected() && attempt + 1 < max_attempts => {
                    // SEC-COMPACT-03: the compact-2026-01-12 beta header was rejected by the API.
                    // The provider already set its internal flag; disable client-side gate and
                    // retry so this turn is not lost.
                    tracing::warn!(
                        attempt,
                        "server compaction beta header rejected; \
                        falling back to client-side compaction and retrying"
                    );
                    self.providers.server_compaction_active = false;
                    let _ = self
                        .channel
                        .send_status(
                            "server compaction unavailable, falling back to client-side...",
                        )
                        .await;
                    let _ = self.channel.send_status("").await;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("loop covers all attempts")
    }

    #[allow(clippy::too_many_lines)] // tool loop with dependency gate, filter, and doom-loop checks
    pub(super) async fn process_response_native_tools(
        &mut self,
    ) -> Result<(), super::super::error::AgentError> {
        self.tool_orchestrator.clear_doom_history();
        self.tool_orchestrator.clear_recent_tool_calls();

        // `mut` required when context-compression is enabled to inject focus tool definitions.
        #[cfg_attr(not(feature = "context-compression"), allow(unused_mut))]
        let tafc = &self.tool_orchestrator.tafc;
        let mut tool_defs: Vec<ToolDefinition> = self
            .tool_executor
            .tool_definitions_erased()
            .iter()
            .map(|def| tool_def_to_definition_with_tafc(def, tafc))
            .collect();

        // Inject focus tool definitions when the feature is enabled and configured (#1850).
        #[cfg(feature = "context-compression")]
        if self.focus.config.enabled {
            tool_defs.extend(super::super::focus::focus_tool_definitions());
        }

        // Inject compress_context tool — always available when context-compression is enabled (#2218).
        #[cfg(feature = "context-compression")]
        tool_defs.push(super::super::focus::compress_context_tool_definition());

        // Pre-compute the full tool set for iterations 1+ before filtering.
        let all_tool_defs = tool_defs.clone();

        // Iteration 0: apply dynamic tool schema filter (#2020) if cached IDs are available.
        if let Some(ref filtered_ids) = self.cached_filtered_tool_ids {
            tool_defs.retain(|d| filtered_ids.contains(&d.name));
            tracing::debug!(
                filtered = tool_defs.len(),
                total = all_tool_defs.len(),
                "tool schema filter: iteration 0 using filtered tool set"
            );
        }

        tracing::debug!(
            tool_count = tool_defs.len(),
            tools = ?tool_defs.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "native tool_use: collected tool definitions"
        );

        let query_embedding = match self.check_response_cache().await? {
            super::CacheCheckResult::Hit(cached) => {
                self.persist_message(Role::Assistant, &cached, &[], false)
                    .await;
                self.msg
                    .messages
                    .push(Message::from_legacy(Role::Assistant, cached.as_str()));
                if cached.contains(MAX_TOKENS_TRUNCATION_MARKER) {
                    let _ = self.channel.send_stop_hint(StopHint::MaxTokens).await;
                }
                self.channel.flush_chunks().await?;
                return Ok(());
            }
            super::CacheCheckResult::Miss { query_embedding } => query_embedding,
        };

        for iteration in 0..self.tool_orchestrator.max_iterations {
            if *self.lifecycle.shutdown.borrow() {
                tracing::info!("native tool loop interrupted by shutdown");
                break;
            }
            if self.lifecycle.cancel_token.is_cancelled() {
                tracing::info!("native tool loop cancelled by user");
                break;
            }
            // Iteration 0 uses filtered tool_defs (schema filter + dependency gates).
            // Iterations 1+ expand to the full set but still apply hard dependency gates
            // so tools with unmet `requires` cannot re-enter through the expansion path (#2024).
            let gated_iter1_defs: Vec<ToolDefinition>;
            let defs_for_turn: &[ToolDefinition] = if iteration == 0 {
                &tool_defs
            } else if let Some(ref dep_graph) = self.dependency_graph
                && !dep_graph.is_empty()
            {
                let names: Vec<&str> = all_tool_defs.iter().map(|d| d.name.as_str()).collect();
                let allowed = dep_graph.filter_tool_names(
                    &names,
                    &self.completed_tool_ids,
                    &self.dependency_always_on,
                );
                let allowed_set: std::collections::HashSet<&str> = allowed.into_iter().collect();
                // Deadlock fallback: if all non-always-on tools would be blocked,
                // use the full set for this iteration.
                let non_ao_allowed = allowed_set
                    .iter()
                    .filter(|n| !self.dependency_always_on.contains(**n))
                    .count();
                let non_ao_total = all_tool_defs
                    .iter()
                    .filter(|d| !self.dependency_always_on.contains(d.name.as_str()))
                    .count();
                if non_ao_allowed == 0 && non_ao_total > 0 {
                    tracing::warn!(
                        iteration,
                        "tool dependency graph: all non-always-on tools gated on iter 1+; \
                         disabling hard gates for this iteration"
                    );
                    &all_tool_defs
                } else {
                    gated_iter1_defs = all_tool_defs
                        .iter()
                        .filter(|d| allowed_set.contains(d.name.as_str()))
                        .cloned()
                        .collect();
                    &gated_iter1_defs
                }
            } else {
                &all_tool_defs
            };
            // None = continue loop, Some(()) = return Ok, Err = propagate
            if self
                .process_single_native_turn(defs_for_turn, iteration, query_embedding.clone())
                .await?
                .is_some()
            {
                return Ok(());
            }
            if self.check_doom_loop(iteration).await? {
                break;
            }
        }

        let _ = self.channel.send_stop_hint(StopHint::MaxTurnRequests).await;
        self.channel.flush_chunks().await?;
        Ok(())
    }

    /// Execute one turn of the native tool loop. Returns `Ok(Some(()))` when the LLM produced
    /// a terminal text response (caller should return `Ok(())`), `Ok(None)` to continue the
    /// loop, or `Err` on a hard error.
    async fn process_single_native_turn(
        &mut self,
        tool_defs: &[ToolDefinition],
        iteration: usize,
        query_embedding: Option<Vec<f32>>,
    ) -> Result<Option<()>, super::super::error::AgentError> {
        self.channel.send_typing().await?;

        // Inject any pending LSP notes as a Role::System message before calling
        // the LLM. Stale notes are cleared unconditionally each iteration so they
        // never accumulate when no new notes were produced.
        // Role::System ensures they are skipped by tool-pair summarization.
        #[cfg(feature = "lsp-context")]
        if self.session.lsp_hooks.is_some() {
            self.remove_lsp_messages();
            let tc = std::sync::Arc::clone(&self.metrics.token_counter);
            if let Some(ref mut lsp) = self.session.lsp_hooks
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
            let used = usize::try_from(self.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
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
                return Ok(Some(()));
            }
        }

        // Show triage status indicator before inference when triage routing is active.
        if matches!(self.provider, zeph_llm::any::AnyProvider::Triage(_)) {
            let _ = self.channel.send_status("Evaluating complexity...").await;
        } else {
            let _ = self.channel.send_status("thinking...").await;
        }
        let chat_result = self.call_chat_with_tools_retry(tool_defs, 2).await?;
        let _ = self.channel.send_status("").await;

        let Some(chat_result) = chat_result else {
            tracing::debug!("chat_with_tools returned None (timeout)");
            return Ok(Some(()));
        };

        tracing::debug!(iteration, ?chat_result, "native tool loop iteration");

        if let ChatResponse::Text(text) = &chat_result {
            // RV-1: response verification before delivery.
            if self.run_response_verification(text) {
                let _ = self
                    .channel
                    .send("[security] Response blocked by injection detection.")
                    .await;
                self.channel.flush_chunks().await?;
                return Ok(Some(()));
            }
            let cleaned = self.scan_output_and_warn(text);
            if !cleaned.is_empty() {
                let display = self.maybe_redact(&cleaned);
                self.channel.send(&display).await?;
            }
            self.store_response_in_cache(&cleaned, query_embedding)
                .await;
            self.persist_message(Role::Assistant, &cleaned, &[], false)
                .await;
            self.msg
                .messages
                .push(Message::from_legacy(Role::Assistant, cleaned.as_str()));
            // Detect context loss after compaction and log failure pair if found.
            #[cfg(feature = "compression-guidelines")]
            self.maybe_log_compression_failure(&cleaned).await;
            if cleaned.contains(MAX_TOKENS_TRUNCATION_MARKER) {
                let _ = self.channel.send_stop_hint(StopHint::MaxTokens).await;
            }
            self.channel.flush_chunks().await?;
            return Ok(Some(()));
        }

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

        // Summarize before pruning; apply deferred summaries after pruning.
        self.maybe_summarize_tool_pair().await;
        let keep_recent = 2 * self.memory_state.tool_call_cutoff + 2;
        self.prune_stale_tool_outputs(keep_recent);
        self.maybe_apply_deferred_summaries();
        self.flush_deferred_summaries().await;
        // Mid-iteration soft compaction: fires after summarization so fresh results are
        // either summarized or protected before pruning. Does not touch turn counters,
        // cooldown, or trigger Hard tier (no LLM call during tool loop).
        self.maybe_soft_compact_mid_iteration();
        self.flush_deferred_summaries().await;

        Ok(None)
    }

    #[allow(clippy::too_many_lines)]
    async fn call_chat_with_tools(
        &mut self,
        tool_defs: &[ToolDefinition],
    ) -> Result<Option<ChatResponse>, super::super::error::AgentError> {
        if let Some(ref tracker) = self.metrics.cost_tracker
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

        let dump_id =
            self.debug_state
                .debug_dumper
                .as_ref()
                .map(|d: &crate::debug_dump::DebugDumper| {
                    d.dump_request(&crate::debug_dump::RequestDebugDump {
                        model_name: &self.runtime.model_name,
                        messages: &self.msg.messages,
                        tools: tool_defs,
                        provider_request: self.provider.debug_request_json(
                            &self.msg.messages,
                            tool_defs,
                            false,
                        ), // lgtm[rust/cleartext-logging]
                    })
                });

        // RuntimeLayer before_chat hooks (MVP: empty vec = zero iterations).
        if !self.runtime_layers.is_empty() {
            let conv_id_str = self.memory_state.conversation_id.map(|id| id.0.to_string());
            let ctx = crate::runtime_layer::LayerContext {
                conversation_id: conv_id_str.as_deref(),
                turn_number: u32::try_from(self.sidequest.turn_counter).unwrap_or(u32::MAX),
            };
            for layer in &self.runtime_layers {
                let hook_result = std::panic::AssertUnwindSafe(layer.before_chat(
                    &ctx,
                    &self.msg.messages,
                    tool_defs,
                ))
                .catch_unwind()
                .await;
                match hook_result {
                    Ok(Some(sc)) => {
                        tracing::debug!("RuntimeLayer short-circuited LLM call");
                        return Ok(Some(sc));
                    }
                    Ok(None) => {}
                    Err(_) => tracing::warn!("RuntimeLayer::before_chat panicked, continuing"),
                }
            }
        }

        // CR-01: open LLM span before the call.
        let trace_guard = self.debug_state.trace_collector.as_ref().and_then(|tc| {
            self.debug_state
                .current_iteration_span_id
                .map(|id| tc.begin_llm_request(id))
        });

        let llm_span = tracing::info_span!("llm_call", model = %self.runtime.model_name);
        let chat_fut = tokio::time::timeout(
            llm_timeout,
            self.provider
                .chat_with_tools(&self.msg.messages, tool_defs)
                .instrument(llm_span),
        );
        let timeout_result = tokio::select! {
            r = chat_fut => r,
            () = self.lifecycle.cancel_token.cancelled() => {
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

        self.record_chat_metrics_and_compact(start, &result).await?;

        // CR-01: close LLM span after the call completes.
        if let Some(guard) = trace_guard
            && let Some(ref mut tc) = self.debug_state.trace_collector
        {
            let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            let (prompt_tokens, completion_tokens) = self.provider.last_usage().unwrap_or((0, 0));
            tc.end_llm_request(
                guard,
                &crate::debug_dump::trace::LlmAttributes {
                    model: self.runtime.model_name.clone(),
                    prompt_tokens,
                    completion_tokens,
                    latency_ms: latency,
                    streaming: false,
                    cache_hit: false,
                },
            );
        }

        self.write_chat_debug_dump(dump_id, &result);

        // RuntimeLayer after_chat hooks (MVP: empty vec = zero iterations).
        if !self.runtime_layers.is_empty() {
            let conv_id_str = self.memory_state.conversation_id.map(|id| id.0.to_string());
            let ctx = crate::runtime_layer::LayerContext {
                conversation_id: conv_id_str.as_deref(),
                turn_number: u32::try_from(self.sidequest.turn_counter).unwrap_or(u32::MAX),
            };
            for layer in &self.runtime_layers {
                let hook_result = std::panic::AssertUnwindSafe(layer.after_chat(&ctx, &result))
                    .catch_unwind()
                    .await;
                if hook_result.is_err() {
                    tracing::warn!("RuntimeLayer::after_chat panicked, continuing");
                }
            }
        }

        Ok(Some(result))
    }

    fn write_chat_debug_dump(&self, dump_id: Option<u32>, result: &ChatResponse) {
        let Some((d, id)) = self.debug_state.debug_dumper.as_ref().zip(dump_id) else {
            return;
        };
        let raw = match result {
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
        let text = if self.security.pii_filter.is_enabled() {
            self.security.pii_filter.scrub(&raw).into_owned()
        } else {
            raw
        };
        d.dump_response(id, &text);
    }

    async fn record_chat_metrics_and_compact(
        &mut self,
        start: std::time::Instant,
        result: &ChatResponse,
    ) -> Result<(), super::super::error::AgentError> {
        let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let prompt_estimate = self.providers.cached_prompt_tokens;
        let completion_heuristic = match result {
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

        // C2: server-side compaction — prune old messages and insert synthetic compaction message.
        if let Some(raw_summary) = self.provider.take_compaction_summary() {
            let _ = self
                .channel
                .send_status("Compacting context (server-side)...")
                .await;
            tracing::info!(
                summary_len = raw_summary.len(),
                messages_before = self.msg.messages.len(),
                "server-side compaction received; pruning old messages"
            );
            // SEC-COMPACT-01: sanitize (McpResponse = ExternalUntrusted).
            let source = ContentSource::new(ContentSourceKind::McpResponse);
            let sanitized = self.security.sanitizer.sanitize(&raw_summary, source);
            let summary = sanitized.body;
            let last_user = self
                .msg
                .messages
                .iter()
                .rposition(|m| m.role == Role::User)
                .unwrap_or(0);
            let tail: Vec<Message> = self.msg.messages.drain(last_user..).collect();
            self.msg.messages.clear();
            self.msg.messages.push(Message {
                role: Role::Assistant,
                content: summary.clone(),
                parts: vec![MessagePart::Compaction {
                    summary: summary.clone(),
                }],
                metadata: MessageMetadata::default(),
            });
            self.msg.messages.extend(tail);
            self.update_metrics(|m| m.server_compaction_events += 1);
            let _ = self.channel.send_status("").await;
        }

        Ok(())
    }

    /// Prepend thinking blocks to the last assistant message in the context as `MessagePart`s.
    ///
    /// The Claude API requires `thinking`/`redacted_thinking` blocks to be preserved verbatim
    /// in the assistant message when tool results are sent back in multi-turn conversations.
    fn preserve_thinking_blocks(&mut self, blocks: Vec<ThinkingBlock>) {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = self.msg.messages.last_mut()
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

    #[allow(clippy::too_many_lines)] // parallel tool execution with DAG scheduling, retry, self-reflection, cancellation — inherently sequential control flow
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
        if let (Some(id), Some(last)) =
            (self.last_persisted_message_id, self.msg.messages.last_mut())
        {
            last.metadata.db_id = Some(id);
        }

        // Build tool calls for all requests, stripping TAFC think fields before execution.
        let tafc_enabled = self.tool_orchestrator.tafc.enabled;
        let calls: Vec<ToolCall> = tool_calls
            .iter()
            .filter_map(|tc| {
                let mut params: serde_json::Map<String, serde_json::Value> =
                    if let serde_json::Value::Object(map) = &tc.input {
                        map.clone()
                    } else {
                        serde_json::Map::new()
                    };
                if tafc_enabled && strip_tafc_fields(&mut params, &tc.name).is_err() {
                    // Model produced only think fields — skip this tool call.
                    return None;
                }
                Some(ToolCall {
                    tool_id: tc.name.clone(),
                    params,
                })
            })
            .collect();

        // Assign stable IDs before execution so ToolStart and ToolOutput share the same ID.
        let tool_call_ids: Vec<String> = tool_calls
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        // tool_started_ats is populated per-tier just before each tier's join_all so that
        // audit timestamps reflect actual execution start rather than pre-build time.
        let mut tool_started_ats: Vec<std::time::Instant> =
            vec![std::time::Instant::now(); tool_calls.len()];

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

        // Pre-execution verification (TrustBench pattern, issue #1630).
        // Runs after exfiltration guard (flag-only) and before repeat-detection.
        // Block: return synthetic error result for this call without executing.
        // Warn: log + emit security event + continue execution.
        let mut pre_exec_blocked: Vec<bool> = vec![false; calls.len()];
        if !self.tool_orchestrator.pre_execution_verifiers.is_empty() {
            for (idx, call) in calls.iter().enumerate() {
                let args_value = serde_json::Value::Object(call.params.clone());
                for verifier in &self.tool_orchestrator.pre_execution_verifiers {
                    match verifier.verify(&call.tool_id, &args_value) {
                        zeph_tools::VerificationResult::Allow => {}
                        zeph_tools::VerificationResult::Block { reason } => {
                            tracing::warn!(
                                tool = %call.tool_id,
                                verifier = verifier.name(),
                                %reason,
                                "pre-execution verifier blocked tool call"
                            );
                            self.update_metrics(|m| m.pre_execution_blocks += 1);
                            self.push_security_event(
                                crate::metrics::SecurityEventCategory::PreExecutionBlock,
                                &call.tool_id,
                                format!("{}: {}", verifier.name(), reason),
                            );
                            if let Some(ref logger) = self.tool_orchestrator.audit_logger {
                                let args_json =
                                    serde_json::to_string(&args_value).unwrap_or_default();
                                let entry = zeph_tools::AuditEntry {
                                    timestamp: zeph_tools::chrono_now(),
                                    tool: call.tool_id.clone(),
                                    command: args_json,
                                    result: zeph_tools::AuditResult::Blocked {
                                        reason: format!("{}: {}", verifier.name(), reason),
                                    },
                                    duration_ms: 0,
                                    error_category: Some("pre_execution_block".to_owned()),
                                    error_domain: Some("security".to_owned()),
                                    error_phase: Some(
                                        zeph_tools::error_taxonomy::ToolInvocationPhase::Setup
                                            .label()
                                            .to_owned(),
                                    ),
                                    claim_source: None,
                                    mcp_server_id: None,
                                    injection_flagged: false,
                                    embedding_anomalous: false,
                                    cross_boundary_mcp_to_acp: false,
                                };
                                let logger = std::sync::Arc::clone(logger);
                                tokio::spawn(async move { logger.log(&entry).await });
                            }
                            pre_exec_blocked[idx] = true;
                            break;
                        }
                        zeph_tools::VerificationResult::Warn { message } => {
                            tracing::warn!(
                                tool = %call.tool_id,
                                verifier = verifier.name(),
                                %message,
                                "pre-execution verifier warning (not blocked)"
                            );
                            self.update_metrics(|m| m.pre_execution_warnings += 1);
                            self.push_security_event(
                                crate::metrics::SecurityEventCategory::PreExecutionWarn,
                                &call.tool_id,
                                format!("{}: {}", verifier.name(), message),
                            );
                        }
                    }
                }
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
        // Cache hits are also pushed here (P1 invariant): a cached tool called N times must
        // still trigger repeat-detection to prevent infinite loops if the LLM keeps requesting it.
        for (call, &hash) in calls.iter().zip(args_hashes.iter()) {
            self.tool_orchestrator.push_tool_call(&call.tool_id, hash);
        }

        // Cache lookup: for each non-repeat, cacheable call, check result cache before dispatch.
        // Hits are stored as pre-built results; cache store happens after join_all completes.
        let cache_hits: Vec<Option<zeph_tools::ToolOutput>> = calls
            .iter()
            .zip(args_hashes.iter())
            .zip(repeat_blocked.iter())
            .map(|((call, &hash), &blocked)| {
                if blocked || !zeph_tools::is_cacheable(&call.tool_id) {
                    return None;
                }
                let key = zeph_tools::CacheKey::new(&call.tool_id, hash);
                self.tool_orchestrator.result_cache.get(&key)
            })
            .collect();

        // Inject active skill secrets before tool execution
        self.inject_active_skill_env();

        // Execute tool calls with retry for transient errors.
        // Retries do NOT produce a new LLM turn and therefore do NOT consume the outer
        // max_tool_iterations budget — the budget only decrements on LLM round-trips.
        // The retry budget per tool call is bounded independently by max_tool_retries.
        let max_retries = self.tool_orchestrator.max_tool_retries;
        // Clamp to 1 to prevent Semaphore(0) deadlock when config is set to 0.
        let max_parallel = self.runtime.timeouts.max_parallel_tools.max(1);
        let cancel = self.lifecycle.cancel_token.clone();

        // Causal IPI pre-probe: record behavioral baseline before tool batch dispatch.
        // Per-batch (not per-tool): 1 LLM call for the entire batch.
        // On error: log WARN + skip causal analysis for this batch. Never block dispatch.
        let causal_pre_response = if let Some(ref analyzer) = self.security.causal_analyzer {
            let context_summary = self.build_causal_context_summary();
            match analyzer.probe(&context_summary).await {
                Ok(resp) => Some((resp, context_summary)),
                Err(e) => {
                    tracing::warn!(error = %e, "causal IPI pre-probe failed, skipping analysis");
                    None
                }
            }
        } else {
            None
        };

        // Phase 1: Tiered parallel execution bounded by a shared semaphore.
        //
        // Build a dependency DAG over tool_use_id references in call arguments. When the
        // DAG is trivial (no dependencies — the common case), we execute all calls in a
        // single tier with zero overhead. When dependencies exist, we partition calls into
        // topological tiers and execute each tier in parallel, awaiting the previous tier
        // before starting the next.
        //
        // ToolStartEvent is sent at the beginning of each tier so the UI reflects actual
        // execution start time rather than pre-build time.
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_parallel));
        let dag = super::tool_call_dag::ToolCallDag::build(tool_calls);
        let trivial = dag.is_trivial();
        let tiers = dag.tiers();
        let tier_count = tiers.len();
        tracing::debug!(
            trivial,
            tier_count,
            tool_count = tool_calls.len(),
            "tool dispatch: partitioned into tiers"
        );

        // Pre-allocate result vector; slots are filled as tiers complete.
        let mut tool_results: Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>> =
            (0..tool_calls.len()).map(|_| Ok(None)).collect();

        // Pre-process focus tool calls (#1850) and compress_context (#2218).
        // These need &mut self and cannot run inside the parallel tier futures.
        // Pre-populate their results so the tier loop skips them.
        #[cfg(feature = "context-compression")]
        {
            for (idx, tc) in tool_calls.iter().enumerate() {
                let is_focus_tool = self.focus.config.enabled
                    && (tc.name == "start_focus" || tc.name == "complete_focus");
                let is_compress = tc.name == "compress_context";
                if is_focus_tool || is_compress {
                    let result = if is_compress {
                        self.handle_compress_context().await
                    } else {
                        self.handle_focus_tool(&tc.name, &tc.input)
                    };
                    tool_results[idx] = Ok(Some(zeph_tools::ToolOutput {
                        tool_name: tc.name.clone(),
                        summary: result,
                        blocks_executed: 1,
                        filter_stats: None,
                        diff: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                        claim_source: None,
                    }));
                }
            }
        }

        // Track which indices have a failed/ConfirmationRequired prerequisite so that
        // dependent calls in later tiers receive a synthetic error instead of executing.
        // IMP-02: ConfirmationRequired is treated as a failure for dependency propagation —
        // a dependent tool must not proceed when its prerequisite is awaiting user approval.
        let mut failed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (tier_idx, tier) in tiers.into_iter().enumerate() {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("tool execution cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls).await;
                return Ok(());
            }

            if tier_count > 1 {
                let _ = self
                    .channel
                    .send_status(&format!(
                        "Executing tools (tier {}/{})\u{2026}",
                        tier_idx + 1,
                        tier_count
                    ))
                    .await;
            }

            // Mark execution start time for this tier before sending ToolStartEvent.
            let tier_start = std::time::Instant::now();
            for &idx in &tier.indices {
                tool_started_ats[idx] = tier_start;
            }

            // Send ToolStartEvent per-tier (section 3.7): accurate timing for TUI.
            for &idx in &tier.indices {
                let tc = &tool_calls[idx];
                let tool_call_id = &tool_call_ids[idx];
                self.channel
                    .send_tool_start(ToolStartEvent {
                        tool_name: &tc.name,
                        tool_call_id,
                        params: Some(tc.input.clone()),
                        parent_tool_use_id: self.session.parent_tool_use_id.clone(),
                    })
                    .await?;
            }

            // Build futures for this tier. Calls whose prerequisite failed get a synthetic
            // error result immediately (IMP-02: includes ConfirmationRequired dependencies).
            let mut tier_futs: Vec<(usize, ToolExecFut)> = Vec::with_capacity(tier.indices.len());

            // Rate limiter: atomic batch-reserve for this tier (S5 fix).
            // check_batch() reserves slots before any future is dispatched, preventing
            // parallel calls from all passing the check before any records the use.
            let tier_tool_names: Vec<&str> = tier
                .indices
                .iter()
                .map(|&i| tool_calls[i].name.as_str())
                .collect();
            let rate_results = self.runtime.rate_limiter.check_batch(&tier_tool_names);

            for (tier_local_idx, &idx) in tier.indices.iter().enumerate() {
                let tc = &tool_calls[idx];
                let call = &calls[idx];

                // Skip focus tools and compress_context pre-handled above (they already have results).
                #[cfg(feature = "context-compression")]
                if tc.name == "compress_context"
                    || (self.focus.config.enabled
                        && (tc.name == "start_focus" || tc.name == "complete_focus"))
                {
                    continue;
                }

                // Check if this call has a failed/blocked prerequisite.
                // We look up which tool_use_ids this call references using values
                // pre-extracted during DAG construction (no redundant JSON traversal).
                let has_failed_dep = dag
                    .string_values_for(idx)
                    .iter()
                    .any(|v| failed_ids.contains(v));

                if has_failed_dep {
                    // IMP-02: inject synthetic error so the LLM learns the dependency chain broke.
                    let msg =
                        "[error] Skipped: a prerequisite tool failed or requires confirmation"
                            .to_string();
                    let out = zeph_tools::ToolOutput {
                        tool_name: tc.name.clone(),
                        summary: msg,
                        blocks_executed: 0,
                        filter_stats: None,
                        diff: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                        claim_source: None,
                    };
                    tier_futs.push((idx, Box::pin(std::future::ready(Ok(Some(out))))));
                    continue;
                }

                if pre_exec_blocked[idx] {
                    let msg = format!(
                        "[error] Tool call to {} was blocked by pre-execution verifier. \
                         The requested operation is not permitted.",
                        tc.name
                    );
                    let out = zeph_tools::ToolOutput {
                        tool_name: tc.name.clone(),
                        summary: msg,
                        blocks_executed: 0,
                        filter_stats: None,
                        diff: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                        claim_source: None,
                    };
                    tier_futs.push((idx, Box::pin(std::future::ready(Ok(Some(out))))));
                    continue;
                }

                if repeat_blocked[idx] {
                    let msg = format!(
                        "[error] Repeated identical call to {} detected. \
                         Use different arguments or a different approach.",
                        tc.name
                    );
                    let out = zeph_tools::ToolOutput {
                        tool_name: tc.name.clone(),
                        summary: msg,
                        blocks_executed: 0,
                        filter_stats: None,
                        diff: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                        claim_source: None,
                    };
                    tier_futs.push((idx, Box::pin(std::future::ready(Ok(Some(out))))));
                    continue;
                }

                // Cache hit: return pre-computed result without executing the tool.
                // TUI events (ToolStartEvent already sent above) will still be emitted for cache
                // hits in the result processing loop below, maintaining Start/Output pairing.
                if let Some(cached_output) = cache_hits[idx].clone() {
                    tracing::debug!(
                        tool = %tc.name,
                        "[tool-cache] returning cached result, skipping execution"
                    );
                    tier_futs.push((idx, Box::pin(std::future::ready(Ok(Some(cached_output))))));
                    continue;
                }

                // Rate limiter: check the pre-computed batch result for this call.
                if let Some(ref exceeded) = rate_results[tier_local_idx] {
                    tracing::warn!(
                        tool = %tc.name,
                        category = exceeded.category.as_str(),
                        limit = exceeded.limit,
                        "tool rate limiter: blocking call"
                    );
                    self.update_metrics(|m| m.rate_limit_trips += 1);
                    self.push_security_event(
                        crate::metrics::SecurityEventCategory::RateLimit,
                        &tc.name,
                        format!(
                            "{} calls exceeded {}/min",
                            exceeded.category.as_str(),
                            exceeded.limit
                        ),
                    );
                    let out = zeph_tools::ToolOutput {
                        tool_name: tc.name.clone(),
                        summary: exceeded.to_error_message(),
                        blocks_executed: 0,
                        filter_stats: None,
                        diff: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                        claim_source: None,
                    };
                    tier_futs.push((idx, Box::pin(std::future::ready(Ok(Some(out))))));
                    continue;
                }

                // RuntimeLayer before_tool hooks: may short-circuit execution.
                if !self.runtime_layers.is_empty() {
                    let conv_id_str = self.memory_state.conversation_id.map(|id| id.0.to_string());
                    let ctx = crate::runtime_layer::LayerContext {
                        conversation_id: conv_id_str.as_deref(),
                        turn_number: u32::try_from(self.sidequest.turn_counter).unwrap_or(u32::MAX),
                    };
                    let mut sc_result: crate::runtime_layer::BeforeToolResult = None;
                    for layer in &self.runtime_layers {
                        let hook_result =
                            std::panic::AssertUnwindSafe(layer.before_tool(&ctx, call))
                                .catch_unwind()
                                .await;
                        match hook_result {
                            Ok(Some(r)) => {
                                sc_result = Some(r);
                                break;
                            }
                            Ok(None) => {}
                            Err(_) => {
                                tracing::warn!("RuntimeLayer::before_tool panicked, continuing");
                            }
                        }
                    }
                    if let Some(r) = sc_result {
                        tier_futs.push((idx, Box::pin(std::future::ready(r))));
                        continue;
                    }
                }

                let sem = std::sync::Arc::clone(&semaphore);
                let executor = std::sync::Arc::clone(&self.tool_executor);
                let call = call.clone();
                let tool_name = tc.name.clone();
                let tool_id = tc.id.clone();
                let fut = async move {
                    let _permit = sem.acquire().await.map_err(|_| {
                        zeph_tools::ToolError::Execution(std::io::Error::other(
                            "semaphore closed during tool execution",
                        ))
                    })?;
                    executor
                        .execute_tool_call_erased(&call)
                        .instrument(tracing::info_span!(
                            "tool_exec",
                            tool_name = %tool_name,
                            idx = %tool_id
                        ))
                        .await
                };
                tier_futs.push((idx, Box::pin(fut)));
            }

            // Execute all futures in this tier concurrently via join_all.
            // Note: join_all provides cooperative (tokio task) concurrency, not OS-thread
            // parallelism. Futures yield at .await points and are scheduled by the tokio
            // runtime. For CPU-bound tool work, the semaphore limits oversubscription.
            let (indices, futs): (Vec<usize>, Vec<ToolExecFut>) = tier_futs.into_iter().unzip();

            let tier_results = tokio::select! {
                results = futures::future::join_all(futs) => results,
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

            // Store results and collect failed tool_use_ids for dependency propagation.
            for (idx, result) in indices.into_iter().zip(tier_results) {
                // IMP-02: Err(_) covers all error variants including ConfirmationRequired —
                // no need to match individual variants. Ok(Some(out)) with "[error]" prefix
                // covers synthetic/blocked results that arrived as Ok but signal failure.
                let is_failed = match &result {
                    Err(_) => true,
                    Ok(Some(out)) => out.summary.starts_with("[error]"),
                    Ok(None) => false,
                };
                if is_failed {
                    failed_ids.insert(tool_calls[idx].id.clone());
                }

                // Store successful, non-cached results in the tool result cache.
                // Skip if this was already a cache hit (no point caching a cached result).
                if !is_failed
                    && cache_hits[idx].is_none()
                    && zeph_tools::is_cacheable(&tool_calls[idx].name)
                    && let Ok(Some(ref out)) = result
                {
                    let key = zeph_tools::CacheKey::new(&tool_calls[idx].name, args_hashes[idx]);
                    self.tool_orchestrator.result_cache.put(key, out.clone());
                }

                // Record successful tool completions for the dependency graph (#2024).
                // Only record on success (non-error) so `requires` chains work correctly.
                if !is_failed && self.dependency_graph.is_some() {
                    self.completed_tool_ids.insert(tool_calls[idx].name.clone());
                }

                // RuntimeLayer after_tool hooks.
                if !self.runtime_layers.is_empty() {
                    let conv_id_str = self.memory_state.conversation_id.map(|id| id.0.to_string());
                    let ctx = crate::runtime_layer::LayerContext {
                        conversation_id: conv_id_str.as_deref(),
                        turn_number: u32::try_from(self.sidequest.turn_counter).unwrap_or(u32::MAX),
                    };
                    for layer in &self.runtime_layers {
                        let hook_result = std::panic::AssertUnwindSafe(layer.after_tool(
                            &ctx,
                            &calls[idx],
                            &result,
                        ))
                        .catch_unwind()
                        .await;
                        if hook_result.is_err() {
                            tracing::warn!("RuntimeLayer::after_tool panicked, continuing");
                        }
                    }
                }

                tool_results[idx] = result;
            }

            if tier_count > 1 {
                let _ = self.channel.send_status("").await;
            }
        }

        // Pad with empty results if needed (defensive; should not happen).
        while tool_results.len() < tool_calls.len() {
            tool_results.push(Ok(None));
        }

        // Phase 2a: Handle ConfirmationRequired results.
        // ConfirmationRequired requires an interactive channel.confirm() prompt which needs
        // &mut self — it cannot run inside the parallel Phase 1 futures. Handled here
        // sequentially after join_all, same as transient retry in Phase 2.
        for idx in 0..tool_results.len() {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("tool execution cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls).await;
                return Ok(());
            }

            let new_result =
                if let Err(zeph_tools::ToolError::ConfirmationRequired { ref command }) =
                    tool_results[idx]
                {
                    let tc = &tool_calls[idx];
                    let prompt = if command.is_empty() {
                        format!("Allow tool: {}?", tc.name)
                    } else {
                        format!("Allow command: {command}?")
                    };
                    Some(if self.channel.confirm(&prompt).await? {
                        // execute_tool_call_confirmed_erased bypasses check_trust; a second
                        // ConfirmationRequired here indicates a misconfigured executor stack
                        // and is treated as a regular tool error.
                        self.tool_executor
                            .execute_tool_call_confirmed_erased(&calls[idx])
                            .await
                    } else {
                        // User declined — not an error, just a cancellation.
                        Ok(Some(zeph_tools::ToolOutput {
                            tool_name: tc.name.clone(),
                            summary: "[cancelled by user]".to_owned(),
                            blocks_executed: 0,
                            filter_stats: None,
                            diff: None,
                            streamed: false,
                            terminal_id: None,
                            locations: None,
                            raw_response: None,
                            claim_source: None,
                        }))
                    })
                } else {
                    None
                };
            if let Some(result) = new_result {
                if let Err(ref e) = result
                    && let Some(ref d) = self.debug_state.debug_dumper
                {
                    d.dump_tool_error(&tool_calls[idx].name, e);
                }
                tool_results[idx] = result;
            }
        }

        // Phase 2: Sequential retry for transient failures on retryable executors.
        // Only idempotent operations (e.g. HTTP GET via WebScrapeExecutor) are retried.
        // Shell commands and other non-idempotent tools keep their error result as-is.
        // Multiple transient failures are retried sequentially; parallel retry adds complexity
        // for minimal gain in the rare case of multiple simultaneous transient failures.
        if max_retries > 0 {
            let max_retry_duration_secs = self.tool_orchestrator.max_retry_duration_secs;
            let retry_base_ms = self.tool_orchestrator.retry_base_ms;
            let retry_max_ms = self.tool_orchestrator.retry_max_ms;
            for idx in 0..tool_results.len() {
                if cancel.is_cancelled() {
                    self.tool_executor.set_skill_env(None);
                    tracing::info!("tool execution cancelled by user");
                    self.update_metrics(|m| m.cancellations += 1);
                    self.channel.send("[Cancelled]").await?;
                    self.persist_cancelled_tool_results(tool_calls).await;
                    return Ok(());
                }

                let is_transient = matches!(
                    tool_results[idx],
                    Err(ref e) if e.kind() == zeph_tools::ErrorKind::Transient
                );
                if !is_transient {
                    continue;
                }

                let tc = &tool_calls[idx];
                if !self.tool_executor.is_tool_retryable_erased(&tc.name) {
                    continue;
                }

                let call = &calls[idx];
                let mut attempt = 0_usize;
                let retry_start = std::time::Instant::now();
                let result = loop {
                    let exec_result = tokio::select! {
                        r = self.tool_executor.execute_tool_call_erased(call).instrument(
                            tracing::info_span!("tool_exec_retry", tool_name = %tc.name, idx = %tc.id)
                        ) => r,
                        () = cancel.cancelled() => {
                            self.tool_executor.set_skill_env(None);
                            tracing::info!("tool retry cancelled by user");
                            self.update_metrics(|m| m.cancellations += 1);
                            self.channel.send("[Cancelled]").await?;
                            self.persist_cancelled_tool_results(tool_calls).await;
                            return Ok(());
                        }
                    };

                    match exec_result {
                        Err(ref e)
                            if e.kind() == zeph_tools::ErrorKind::Transient
                                && attempt < max_retries =>
                        {
                            let elapsed_secs = retry_start.elapsed().as_secs();
                            if max_retry_duration_secs > 0
                                && elapsed_secs >= max_retry_duration_secs
                            {
                                tracing::warn!(
                                    tool = %tc.name,
                                    elapsed_secs,
                                    max_retry_duration_secs,
                                    "tool retry budget exceeded, aborting retries"
                                );
                                break exec_result;
                            }
                            attempt += 1;
                            let delay_ms =
                                retry_backoff_ms(attempt - 1, retry_base_ms, retry_max_ms);
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
                            // Interruptible backoff sleep: cancelled if agent shuts down.
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
                tool_results[idx] = result;
            }
        }

        // Phase 3: Parameter reformat path for InvalidParameters / TypeMismatch errors.
        //
        // When `parameter_reformat_provider` is configured, ask a (cheap) LLM provider to
        // reformat the tool arguments and retry ONCE. If the reformat call or the retry fails,
        // the original error is kept. This path is budget-aware: we check the same
        // `budget_secs` wall-clock limit that applies to transient retries.
        //
        // Cancellation safety (B3): both the reformat LLM call and the retry execution are
        // wrapped in `tokio::select!` with the cancellation token, matching the pattern from
        // Phase 2. On cancellation, a synthetic error `tool_result` is persisted before returning
        // so the TOOL_RESULT_GUARANTEE invariant is upheld for every `tool_call_id`.
        if !self
            .tool_orchestrator
            .parameter_reformat_provider
            .is_empty()
        {
            let budget_secs = self.tool_orchestrator.max_retry_duration_secs;
            for idx in 0..tool_results.len() {
                if cancel.is_cancelled() {
                    self.tool_executor.set_skill_env(None);
                    tracing::info!("parameter reformat phase cancelled by user");
                    self.update_metrics(|m| m.cancellations += 1);
                    self.channel.send("[Cancelled]").await?;
                    self.persist_cancelled_tool_results(tool_calls).await;
                    return Ok(());
                }

                let needs_reformat = matches!(
                    tool_results[idx],
                    Err(ref e) if e.category().needs_parameter_reformat()
                );
                if !needs_reformat {
                    continue;
                }

                let tc = &tool_calls[idx];
                let reformat_start = std::time::Instant::now();
                tracing::warn!(
                    tool = %tc.name,
                    "parameter error detected; parameter reformat path is reserved for future LLM-based reformat implementation"
                );

                // Budget check: if the wall-clock budget is already exhausted, skip.
                if budget_secs > 0 && reformat_start.elapsed().as_secs() >= budget_secs {
                    tracing::warn!(
                        tool = %tc.name,
                        "parameter reformat budget exhausted, skipping"
                    );
                    continue;
                }

                // The reformat LLM call and retry are placeholders — the actual provider
                // resolution and prompt construction will be implemented in a follow-up once
                // the provider registry lookup API stabilizes. For now, we log and skip
                // so the original error is propagated unchanged to the LLM as structured feedback.
                let _ = self
                    .channel
                    .send_status(&format!(
                        "Reformat for {} pending provider integration…",
                        tc.name
                    ))
                    .await;
                let _ = self.channel.send_status("").await;
            }
        }

        self.tool_executor.set_skill_env(None);

        // Sync cache counters to metrics after all tool execution is complete.
        {
            let hits = self.tool_orchestrator.result_cache.hits();
            let misses = self.tool_orchestrator.result_cache.misses();
            let entries = self.tool_orchestrator.result_cache.len();
            self.update_metrics(|m| {
                m.tool_cache_hits = hits;
                m.tool_cache_misses = misses;
                m.tool_cache_entries = entries;
            });
        }

        // Collect (name, params, output) for LSP hooks. Built during the results loop below.
        #[cfg(feature = "lsp-context")]
        let mut lsp_tool_calls: Vec<(String, serde_json::Value, String)> = Vec::new();

        // Process results sequentially (metrics, channel sends, message parts).
        // self_reflection is deferred until after all result_parts are assembled and user_msg
        // is pushed to history. Calling it inside the loop would insert a reflection dialogue
        // (User{prompt} + Assistant{response}) between Assistant{ToolUse} and User{ToolResults},
        // violating the OpenAI/Claude API message ordering protocol → HTTP 400.
        let mut result_parts: Vec<MessagePart> = Vec::new();
        // Accumulates injection flags across all tools in the batch (Bug #1490 fix).
        let mut has_any_injection_flags = false;
        // Deferred self-reflection: set to the sanitized error output of the first failing tool
        // that is eligible for reflection. Consumed after user_msg is pushed to history.
        let mut pending_reflection: Option<String> = None;
        for idx in 0..tool_calls.len() {
            let tc = &tool_calls[idx];
            let tool_call_id = &tool_call_ids[idx];
            let started_at = &tool_started_ats[idx];
            let tool_result = std::mem::replace(&mut tool_results[idx], Ok(None));
            let anomaly_outcome;
            // True only for InvalidParams errors — semantic failures attributable to model quality.
            // Network, transient, timeout, and policy errors are excluded.
            let is_quality_failure;
            let mut tool_err_category: Option<zeph_tools::error_taxonomy::ToolErrorCategory> = None;
            let (output, is_error, diff, inline_stats, _, kept_lines, locations) = match tool_result
            {
                Ok(Some(out)) => {
                    is_quality_failure = false;
                    anomaly_outcome =
                        if out.summary.contains("[error]") || out.summary.contains("[stderr]") {
                            AnomalyOutcome::Error
                        } else {
                            AnomalyOutcome::Success
                        };
                    if let Some(ref fs) = out.filter_stats {
                        self.record_filter_metrics(fs);
                    }
                    let inline_stats = out.filter_stats.as_ref().and_then(|fs| {
                        (fs.filtered_chars < fs.raw_chars).then(|| fs.format_inline(&tc.name))
                    });
                    let kept = out
                        .filter_stats
                        .as_ref()
                        .and_then(|fs| (!fs.kept_lines.is_empty()).then(|| fs.kept_lines.clone()));
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
                Ok(None) => {
                    is_quality_failure = false;
                    anomaly_outcome = AnomalyOutcome::Success;
                    (
                        "(no output)".to_owned(),
                        false,
                        None,
                        None,
                        false,
                        None,
                        None,
                    )
                }
                Err(ref e) => {
                    let category = e.category();
                    // Quality failures are errors attributable to LLM output (invalid params,
                    // type mismatch, tool not found). Infrastructure errors (network, timeout,
                    // server, rate limit) are not the model's fault.
                    is_quality_failure = category.is_quality_failure();
                    tool_err_category = Some(category);
                    anomaly_outcome = if matches!(e, zeph_tools::ToolError::Blocked { .. }) {
                        AnomalyOutcome::Blocked
                    } else if is_quality_failure
                        && zeph_tools::is_reasoning_model(self.provider.name())
                    {
                        AnomalyOutcome::ReasoningQualityFailure {
                            model: self.provider.name().to_owned(),
                            tool: tc.name.clone(),
                        }
                    } else {
                        AnomalyOutcome::Error
                    };
                    if let Some(ref d) = self.debug_state.debug_dumper {
                        d.dump_tool_error(&tc.name, e);
                    }
                    // Count memory write validation rejections.
                    if tc.name == "memory_save"
                        && matches!(e, zeph_tools::ToolError::InvalidParams { .. })
                        && e.to_string().contains("memory write rejected")
                    {
                        self.update_metrics(|m| m.memory_validation_failures += 1);
                        self.push_security_event(
                            crate::metrics::SecurityEventCategory::MemoryValidation,
                            "memory_save",
                            e.to_string(),
                        );
                    }
                    let feedback = zeph_tools::ToolErrorFeedback {
                        category,
                        message: e.to_string(),
                        retryable: category.is_retryable(),
                    };
                    (
                        feedback.format_for_llm(),
                        true,
                        None,
                        None,
                        false,
                        None,
                        None,
                    )
                }
            };

            // CR-01: emit a tool span for each completed tool call.
            if let Some(ref mut trace_coll) = self.debug_state.trace_collector
                && let Some(iter_span_id) = self.debug_state.current_iteration_span_id
            {
                let latency = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                let guard = trace_coll.begin_tool_call_at(&tc.name, iter_span_id, started_at);
                let error_kind = if is_error {
                    Some(output.chars().take(200).collect::<String>())
                } else {
                    None
                };
                trace_coll.end_tool_call(
                    guard,
                    &tc.name,
                    crate::debug_dump::trace::ToolAttributes {
                        latency_ms: latency,
                        is_error,
                        error_kind,
                    },
                );
            }

            // Record skill learning outcomes for the native tool path (mirrors legacy path in
            // handle_tool_result). Capture the first eligible error for deferred self_reflection
            // (called after user_msg is pushed to history to preserve API message ordering).
            if output.contains("[error]") || output.contains("[exit code") {
                let kind = tool_err_category
                    .take()
                    .map_or_else(|| FailureKind::from_error(&output), FailureKind::from);
                self.record_skill_outcomes("tool_failure", Some(&output), Some(kind.as_str()))
                    .await;
                // Record quality failure for reputation scoring only when the model produced
                // invalid tool arguments (semantic failure). Network errors and transient
                // failures are not attributable to model quality.
                if is_quality_failure {
                    self.provider
                        .record_quality_outcome(self.provider.name(), false);
                }
                // Self-reflection is only useful for quality failures (LLM produced wrong params,
                // used wrong tool name, etc.). Infrastructure errors (network, timeout, server,
                // rate limit) are not attributable to LLM output — reflecting on them wastes
                // tokens without improving future behavior. This is a behavioral change from the
                // prior implementation which triggered reflection for any [error]-prefixed output.
                if pending_reflection.is_none()
                    && !self.learning_engine.was_reflection_used()
                    && is_quality_failure
                {
                    // Sanitize before passing to self_reflection: tool output from native calls
                    // can contain untrusted content with injection patterns.
                    let sanitized_out = self
                        .security
                        .sanitizer
                        .sanitize(&output, ContentSource::new(ContentSourceKind::ToolResult))
                        .body;
                    pending_reflection = Some(sanitized_out);
                }
            } else {
                self.record_skill_outcomes("success", None, None).await;
                // Record quality success for reputation scoring.
                self.provider
                    .record_quality_outcome(self.provider.name(), true);
            }
            // Ignore channel errors so ToolResult assembly is never abandoned (#2197 secondary).
            let _ = self.record_anomaly_outcome(anomaly_outcome).await;

            // read_overflow returns the full stored content and must not be re-overflowed.
            let processed = if tc.name == OverflowToolExecutor::TOOL_NAME {
                output.clone()
            } else {
                self.maybe_summarize_tool_output(&output).await
            };
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
                    parent_tool_use_id: self.session.parent_tool_use_id.clone(),
                    raw_response: None,
                    started_at: Some(*started_at),
                })
                .await?;

            // Sanitize tool output before inserting into LLM message history (Bug #1490 fix).
            // sanitize_tool_output is the sole sanitization point for tool output data flows.
            // channel send above uses body_display (redacted for privacy); LLM sees sanitize_tool_output output.
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

        // Causal IPI post-probe: compare behavioral state after tool batch results.
        // Uses tool output snippets (first 200 chars each) — never the full sanitized content.
        if let Some((pre_response, context_summary)) = causal_pre_response {
            // Collect snippets from the sanitized content in result_parts.
            let snippets: Vec<String> = result_parts
                .iter()
                .filter_map(|p| {
                    if let MessagePart::ToolResult {
                        content, is_error, ..
                    } = p
                    {
                        if *is_error {
                            Some(zeph_sanitizer::causal_ipi::format_error_snippet(content))
                        } else {
                            Some(zeph_sanitizer::causal_ipi::format_tool_snippet(content))
                        }
                    } else {
                        None
                    }
                })
                .collect();
            let tool_snippets = if snippets.is_empty() {
                "[empty]".to_owned()
            } else {
                snippets.join("---")
            };
            if let Some(ref analyzer) = self.security.causal_analyzer {
                match analyzer.post_probe(&context_summary, &tool_snippets).await {
                    Ok(post_response) => {
                        let analysis = analyzer.analyze(&pre_response, &post_response);
                        if analysis.is_flagged {
                            let pre_excerpt =
                                &pre_response[..pre_response.floor_char_boundary(100)];
                            let post_excerpt =
                                &post_response[..post_response.floor_char_boundary(100)];
                            tracing::warn!(
                                deviation_score = analysis.deviation_score,
                                threshold = analyzer.threshold(),
                                pre = %pre_excerpt,
                                post = %post_excerpt,
                                "causal IPI: behavioral deviation detected at tool-return boundary"
                            );
                            self.update_metrics(|m| m.causal_ipi_flags += 1);
                            self.push_security_event(
                                crate::metrics::SecurityEventCategory::CausalIpiFlag,
                                "tool_batch",
                                format!("deviation={:.3}", analysis.deviation_score),
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "causal IPI post-probe failed, skipping analysis");
                    }
                }
            }
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
        if let (Some(id), Some(last)) =
            (self.last_persisted_message_id, self.msg.messages.last_mut())
        {
            last.metadata.db_id = Some(id);
        }

        // Deferred self-reflection: user_msg is now in history so the reflection dialogue
        // (User{prompt} + Assistant{response}) appends after User{ToolResults}, preserving
        // API message ordering. Only the first eligible error per batch triggers reflection.
        if let Some(sanitized_out) = pending_reflection {
            match self
                .attempt_self_reflection(&sanitized_out, &sanitized_out)
                .await
            {
                Ok(_) | Err(_) => {
                    // Whether reflection succeeded, declined, or errored: the ToolResults are
                    // already committed to history. Return Ok regardless so the caller continues
                    // the tool loop normally (#2197).
                }
            }
        }

        // Fire LSP hooks for each completed tool call (non-blocking: diagnostics fetch
        // is spawned in background; hover calls are awaited but short-lived).
        // `lsp_tool_calls` collects (name, params, output) tuples built during the
        // results loop above. They are captured into a separate Vec so we can call
        // `&mut self.session.lsp_hooks` without conflicting borrows.
        #[cfg(feature = "lsp-context")]
        if self.session.lsp_hooks.is_some() {
            let tc_arc = std::sync::Arc::clone(&self.metrics.token_counter);
            let sanitizer = self.security.sanitizer.clone();
            for (name, input, output) in lsp_tool_calls {
                if let Some(ref mut lsp) = self.session.lsp_hooks {
                    lsp.after_tool(&name, &input, &output, &tc_arc, &sanitizer)
                        .await;
                }
            }
        }

        Ok(())
    }

    /// Handle a focus tool call (`start_focus` / `complete_focus`) directly on the Agent (#1850).
    ///
    /// Returns the tool result string. On error the string begins with `[error]`.
    ///
    /// ## S4 fix
    ///
    /// If `complete_focus` is called without an active focus session, or the checkpoint marker
    /// is not found in the message history, an `[error]` result is returned to the LLM so it
    /// knows the state is invalid rather than silently succeeding.
    #[cfg(feature = "context-compression")]
    pub(crate) fn handle_focus_tool(
        &mut self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> String {
        match tool_name {
            "start_focus" => {
                let scope = input
                    .get("scope")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unspecified)")
                    .to_string();

                if self.focus.is_active() {
                    return "[error] A focus session is already active. Call complete_focus first."
                        .to_string();
                }

                let marker = self.focus.start(scope.clone());

                // Insert a checkpoint message carrying the marker UUID so complete_focus can
                // locate the boundary even after intervening compaction.
                // S5 fix: focus_pinned=true ensures compaction never evicts this message.
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
                self.push_message(checkpoint_msg);

                format!("Focus session started. Checkpoint ID: {marker}. Scope: {scope}")
            }

            "complete_focus" => {
                let summary = input
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                // S4: verify focus session is active.
                if !self.focus.is_active() {
                    return "[error] No active focus session. Call start_focus first.".to_string();
                }

                let Some(marker) = self.focus.active_marker else {
                    return "[error] Internal error: active_marker is None.".to_string();
                };

                // S4: find the checkpoint message by marker UUID.
                let checkpoint_pos = self
                    .msg
                    .messages
                    .iter()
                    .position(|m| m.metadata.focus_marker_id == Some(marker));
                let Some(checkpoint_pos) = checkpoint_pos else {
                    return format!(
                        "[error] Checkpoint marker {marker} not found in message history. \
                         The focus session may have been evicted by compaction."
                    );
                };

                // Collect messages since the checkpoint (exclusive of the checkpoint itself)
                // up to the end, minus any messages added in this very tool call turn.
                // The checkpoint itself and the bracketed messages are removed from history.
                let messages_to_summarize = self.msg.messages[checkpoint_pos + 1..].to_vec();

                // Sanitize the LLM-supplied summary before storing it to the pinned Knowledge
                // block. The summary may summarize transitive external content (web scrapes,
                // MCP responses), so use WebScrape (ExternalUntrusted trust level) for stricter
                // spotlighting than ToolResult (SEC-CC-03).
                let sanitized_summary = self
                    .security
                    .sanitizer
                    .sanitize(
                        &summary,
                        zeph_sanitizer::ContentSource::new(
                            zeph_sanitizer::ContentSourceKind::WebScrape,
                        ),
                    )
                    .body;

                // The LLM-supplied summary is the primary knowledge entry; the bracketed messages
                // are removed to free context (not re-summarized here to avoid LLM overhead).
                let _ = messages_to_summarize; // messages available for future semantic use
                self.focus.append_knowledge(sanitized_summary.clone());
                if let Some(ref d) = self.debug_state.debug_dumper {
                    let kb = self.focus.knowledge_blocks.join("\n---\n");
                    d.dump_focus_knowledge(&kb);
                }
                self.focus.complete();

                // Remove the checkpoint and all messages after it (bracketed phase cleanup).
                self.msg.messages.truncate(checkpoint_pos);
                self.recompute_prompt_tokens();
                // C1 fix: mark compacted so maybe_compact() does not double-fire this turn.
                // cooldown=0: focus truncation does not impose post-compaction cooldown.
                self.context_manager.compaction =
                    crate::agent::context_manager::CompactionState::CompactedThisTurn {
                        cooldown: 0,
                    };

                // Rebuild/insert the pinned Knowledge block message.
                // Remove any existing Knowledge block (focus_pinned=true, no marker_id).
                // Checkpoints have focus_marker_id set and must be preserved here
                // (they were already truncated above along with the bracketed messages).
                self.msg
                    .messages
                    .retain(|m| !(m.metadata.focus_pinned && m.metadata.focus_marker_id.is_none()));
                if let Some(kb_msg) = self.focus.build_knowledge_message() {
                    // Insert the Knowledge block right after the system prompt (index 1).
                    if self.msg.messages.is_empty() {
                        self.msg.messages.push(kb_msg);
                    } else {
                        self.msg.messages.insert(1, kb_msg);
                    }
                }
                self.recompute_prompt_tokens();

                format!("Focus session complete. Knowledge block updated with: {sanitized_summary}")
            }

            other => format!("[error] Unknown focus tool: {other}"),
        }
    }

    /// Handle the `compress_context` tool call (#2218).
    ///
    /// Summarizes non-pinned conversation history, appends to the Knowledge block, and removes
    /// the compressed messages from context. Returns a string result to the LLM.
    ///
    /// Guards:
    /// - Returns error if a focus session is active (would interfere with focus boundaries).
    /// - Returns error if a compression is already in progress (concurrency guard).
    #[cfg(feature = "context-compression")]
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn handle_compress_context(&mut self) -> String {
        use zeph_llm::provider::LlmProvider as _;

        // Guard: no active focus session.
        if self.focus.is_active() {
            return "[error] Cannot compress context while a focus session is active. \
                    Call complete_focus first."
                .to_string();
        }

        // Guard: concurrency — no double compression.
        if !self.focus.try_acquire_compression() {
            return "[error] A context compression is already in progress.".to_string();
        }

        // Collect indices of non-pinned, non-system messages (candidates for compression),
        // then select the head slice (excluding the preserve tail) as the removal set.
        let preserve_tail = self.context_manager.compaction_preserve_tail;
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
            self.focus.release_compression();
            return format!(
                "Not enough messages to compress (found {total}, need at least {}).",
                preserve_tail + 4
            );
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

        // Build summary prompt from the messages to compress.
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

        let system_content = "You are a context compression agent. \
            Summarize the following conversation messages into a concise, information-dense summary. \
            Preserve key facts, decisions, and context. Strip filler and small talk. \
            Output ONLY the summary — no headers, no preamble.";

        let summary_messages = vec![
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
        ];

        let compress_provider = self
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
                self.focus.release_compression();
                return format!("[error] Compression LLM call failed: {e}");
            }
            Err(_) => {
                self.focus.release_compression();
                return "[error] Compression LLM call timed out.".to_string();
            }
        };

        if summary.trim().is_empty() {
            self.focus.release_compression();
            return "[error] Compression produced an empty summary.".to_string();
        }

        let tokens_freed = to_compress
            .iter()
            .map(|m| m.content.len() / 4)
            .sum::<usize>();

        // Append summary to Knowledge block.
        self.focus.append_knowledge(summary.trim().to_owned());

        // Remove compressed messages from in-memory history using their original indices.
        // Index-based removal avoids false positives when two messages share identical content.
        let mut remove_idx = to_remove_indices.iter().copied().collect::<Vec<_>>();
        remove_idx.sort_unstable_by(|a, b| b.cmp(a)); // reverse order to preserve earlier indices
        for idx in remove_idx {
            if idx < self.msg.messages.len() {
                self.msg.messages.remove(idx);
            }
        }

        // Rebuild Knowledge block message.
        self.msg
            .messages
            .retain(|m| !(m.metadata.focus_pinned && m.metadata.focus_marker_id.is_none()));
        if let Some(kb_msg) = self.focus.build_knowledge_message() {
            if self.msg.messages.is_empty() {
                self.msg.messages.push(kb_msg);
            } else {
                self.msg.messages.insert(1, kb_msg);
            }
        }
        self.recompute_prompt_tokens();
        self.context_manager.compaction =
            crate::agent::context_manager::CompactionState::CompactedThisTurn { cooldown: 0 };

        self.focus.release_compression();

        format!(
            "Compressed {compressed_count} messages into a summary (~{tokens_freed} tokens freed). \
             Knowledge block updated.",
            compressed_count = to_compress.len()
        )
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

// T-CRIT-02: handle_focus_tool tests — happy path, error paths, checkpoint pinning (S5 fix).
#[cfg(all(test, feature = "context-compression"))]
mod tests {
    use crate::agent::Agent;
    use crate::agent::tests::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use zeph_llm::provider::{Message, Role};

    fn make_agent() -> Agent<MockChannel> {
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.focus.config.enabled = true;
        // System prompt at index 0 (required by complete_focus insert logic)
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::System, "system"));
        agent
    }

    #[test]
    fn start_focus_happy_path_inserts_pinned_checkpoint() {
        let mut agent = make_agent();
        let input = serde_json::json!({"scope": "reading auth files"});
        let result = agent.handle_focus_tool("start_focus", &input);

        assert!(
            !result.starts_with("[error]"),
            "start_focus must not return error: {result}"
        );
        assert!(
            agent.focus.is_active(),
            "focus session must be active after start_focus"
        );

        // Checkpoint message must exist and be pinned (S5 fix)
        let checkpoint = agent
            .msg
            .messages
            .iter()
            .find(|m| m.metadata.focus_marker_id.is_some());
        assert!(checkpoint.is_some(), "checkpoint message must be inserted");
        let checkpoint = checkpoint.unwrap();
        assert!(
            checkpoint.metadata.focus_pinned,
            "checkpoint message must have focus_pinned=true (S5 fix)"
        );
    }

    #[test]
    fn start_focus_errors_when_already_active() {
        let mut agent = make_agent();
        let input = serde_json::json!({"scope": "first"});
        agent.handle_focus_tool("start_focus", &input);
        let result =
            agent.handle_focus_tool("start_focus", &serde_json::json!({"scope": "second"}));
        assert!(
            result.starts_with("[error]"),
            "second start_focus must return error: {result}"
        );
    }

    #[test]
    fn complete_focus_errors_when_no_active_session() {
        let mut agent = make_agent();
        let result =
            agent.handle_focus_tool("complete_focus", &serde_json::json!({"summary": "done"}));
        assert!(
            result.starts_with("[error]"),
            "complete_focus without active session must error: {result}"
        );
    }

    #[test]
    fn complete_focus_happy_path_clears_session_and_appends_knowledge() {
        let mut agent = make_agent();
        agent.handle_focus_tool("start_focus", &serde_json::json!({"scope": "test"}));
        // Add some messages in the focus window
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::User, "some work"));
        let result = agent.handle_focus_tool(
            "complete_focus",
            &serde_json::json!({"summary": "learned stuff"}),
        );
        assert!(
            !result.starts_with("[error]"),
            "complete_focus must not error: {result}"
        );
        assert!(
            !agent.focus.is_active(),
            "focus session must be cleared after complete_focus"
        );
        assert!(
            !agent.focus.knowledge_blocks.is_empty(),
            "knowledge must be appended"
        );
    }

    #[test]
    fn complete_focus_marker_not_found_returns_error() {
        let mut agent = make_agent();
        agent.handle_focus_tool("start_focus", &serde_json::json!({"scope": "test"}));
        // Remove checkpoint by hand to simulate marker eviction
        agent
            .msg
            .messages
            .retain(|m| m.metadata.focus_marker_id.is_none());
        let result =
            agent.handle_focus_tool("complete_focus", &serde_json::json!({"summary": "done"}));
        assert!(
            result.starts_with("[error]"),
            "must return error when checkpoint not found (S4): {result}"
        );
    }

    #[test]
    fn complete_focus_truncates_bracketed_messages() {
        let mut agent = make_agent();
        agent.handle_focus_tool("start_focus", &serde_json::json!({"scope": "test"}));
        let before_len = agent.msg.messages.len();
        // Add 3 messages in the focus window
        for i in 0..3 {
            agent
                .msg
                .messages
                .push(Message::from_legacy(Role::User, format!("msg {i}")));
        }
        agent.handle_focus_tool("complete_focus", &serde_json::json!({"summary": "done"}));
        // Messages after complete_focus: [system prompt, knowledge block] at minimum
        // Checkpoint + bracketed messages must be gone
        assert!(
            agent.msg.messages.len() < before_len + 3,
            "bracketed messages must be truncated after complete_focus"
        );
    }

    #[test]
    fn min_messages_per_focus_guard_not_enforced_in_tool() {
        // The guard for min_messages_per_focus is advisory (reminder injection path).
        // handle_focus_tool itself does not enforce it — the LLM decides when to call.
        let mut agent = make_agent();
        agent.focus.config.min_messages_per_focus = 100; // very high, but tool doesn't check
        let result = agent.handle_focus_tool("start_focus", &serde_json::json!({"scope": "x"}));
        assert!(
            !result.starts_with("[error]"),
            "tool must not enforce min_messages_per_focus: {result}"
        );
    }
}
