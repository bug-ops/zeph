// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tracing::Instrument;
#[cfg(test)]
use zeph_common::text::estimate_tokens;
use zeph_llm::provider::{
    ChatResponse, LlmProvider, MessagePart, Role, ThinkingBlock, ToolDefinition,
};

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    #[tracing::instrument(
        name = "core.tool.chat_retry",
        skip_all,
        level = "debug",
        fields(max_attempts),
        err
    )]
    pub(super) async fn call_chat_with_tools_retry(
        &mut self,
        tool_defs: &[ToolDefinition],
        max_attempts: usize,
    ) -> Result<Option<ChatResponse>, crate::agent::error::AgentError> {
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
                    let _ = self.compact_context().await?;
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
                    self.runtime.providers.server_compaction_active = false;
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

    #[tracing::instrument(name = "core.tool.call_chat", skip_all, level = "debug", err)]
    pub(super) async fn call_chat_with_tools(
        &mut self,
        tool_defs: &[ToolDefinition],
    ) -> Result<Option<ChatResponse>, crate::agent::error::AgentError> {
        if let Some(ref tracker) = self.runtime.metrics.cost_tracker
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
        let llm_timeout = std::time::Duration::from_secs(self.runtime.config.timeouts.llm_seconds);
        let start = std::time::Instant::now();

        let memcot_state_for_dump =
            match self.services.memory.extraction.memcot_accumulator.as_ref() {
                Some(acc) => acc.current_state().await,
                None => None,
            };
        let dump_id = self.prepare_chat_debug_dump(tool_defs, memcot_state_for_dump.as_deref());

        // RuntimeLayer before_chat hooks (MVP: empty vec = zero iterations).
        if let Some(sc) = self.run_before_chat_layers(tool_defs).await? {
            return Ok(Some(sc));
        }

        // Inject accumulated LSP notes (hover, diagnostics) as a Role::System message
        // immediately before the LLM call. At this point all tool results from the previous
        // iteration are committed to history and there is no pending ToolUse/ToolResult pair,
        // so inserting a System message is safe for all providers (OpenAI, Claude, Ollama).
        // Stale notes from a prior call_chat_with_tools invocation are removed first so they
        // never accumulate; Role::System is skipped by tool-pair summarization.
        if self.services.session.lsp_hooks.is_some() {
            self.remove_lsp_messages();
            let tc = std::sync::Arc::clone(&self.runtime.metrics.token_counter);
            if let Some(ref mut lsp) = self.services.session.lsp_hooks
                && let Some(note_text) = lsp.drain_notes(&tc)
            {
                self.push_message(zeph_llm::provider::Message::from_legacy(
                    zeph_llm::provider::Role::System,
                    &note_text,
                ));
                self.recompute_prompt_tokens();
            }
        }

        // CR-01: open LLM span before the call.
        let trace_guard = self.runtime.debug.trace_collector.as_ref().and_then(|tc| {
            self.runtime
                .debug
                .current_iteration_span_id
                .map(|id| tc.begin_llm_request(id))
        });

        let llm_span = tracing::info_span!(
            "llm.turn_call",
            model = %self.runtime.config.model_name,
            provider = self.provider.name(),
        );

        let Some(result) = self
            .dispatch_chat_with_tools(tool_defs, llm_timeout, llm_span)
            .await?
        else {
            return Ok(None);
        };

        self.record_chat_metrics_and_compact(start, &result).await?;

        // Accumulate LLM chat latency into the per-turn timing accumulator (#2820).
        let llm_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.runtime.metrics.pending_timings.llm_chat_ms = self
            .runtime
            .metrics
            .pending_timings
            .llm_chat_ms
            .saturating_add(llm_ms);

        // CR-01: close LLM span after the call completes.
        self.record_llm_trace_span_close(trace_guard, start);

        self.runtime.debug.write_chat_debug_dump(
            dump_id,
            &result,
            &self.services.security.pii_filter,
        );

        // RuntimeLayer after_chat hooks (MVP: empty vec = zero iterations).
        self.run_after_chat_layers(&result).await;

        Ok(Some(result))
    }

    #[tracing::instrument(name = "core.tool.dispatch_chat", skip_all, level = "debug", err)]
    async fn dispatch_chat_with_tools(
        &mut self,
        tool_defs: &[ToolDefinition],
        llm_timeout: std::time::Duration,
        llm_span: tracing::Span,
    ) -> Result<Option<ChatResponse>, crate::agent::error::AgentError> {
        let use_speculative_stream = self.services.speculation_engine.as_ref().is_some_and(|e| {
            matches!(
                e.mode(),
                zeph_config::tools::SpeculationMode::Decoding
                    | zeph_config::tools::SpeculationMode::Both
            )
        });

        if use_speculative_stream
            && let Ok(stream) = self
                .provider
                .chat_with_tools_stream(&self.msg.messages, tool_defs)
                .await
        {
            let engine =
                std::sync::Arc::clone(self.services.speculation_engine.as_ref().expect(
                    "invariant: speculation_engine is Some (checked via is_some_and on L961)",
                ));
            let threshold = engine.confidence_threshold();
            let drainer = crate::agent::speculative::stream_drainer::SpeculativeStreamDrainer::new(
                stream, engine, threshold,
            );
            let drain_fut = tokio::time::timeout(llm_timeout, drainer.drive().instrument(llm_span));
            let timeout_result = tokio::select! {
                r = drain_fut => r,
                () = self.runtime.lifecycle.cancel_token.cancelled() => {
                    tracing::info!("chat_with_tools (streaming) cancelled by user");
                    self.update_metrics(|m| m.cancellations += 1);
                    self.channel.send("[Cancelled]").await?;
                    return Ok(None);
                }
            };
            return match timeout_result {
                Ok(Ok(resp)) => Ok(Some(resp)),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "speculative SSE stream failed, falling back");
                    self.call_non_streaming(tool_defs, llm_timeout).await
                }
                Err(_) => {
                    self.channel
                        .send("LLM request timed out. Please try again.")
                        .await?;
                    Ok(None)
                }
            };
        }
        // Provider does not support tool streaming or speculative mode is off — normal path.
        self.call_non_streaming_with_span(tool_defs, llm_timeout, llm_span)
            .await
    }

    #[tracing::instrument(name = "core.tool.call_non_streaming", skip_all, level = "debug", err)]
    async fn call_non_streaming(
        &mut self,
        tool_defs: &[ToolDefinition],
        llm_timeout: std::time::Duration,
    ) -> Result<Option<ChatResponse>, crate::agent::error::AgentError> {
        let chat_fut = tokio::time::timeout(
            llm_timeout,
            self.provider.chat_with_tools(&self.msg.messages, tool_defs),
        );
        let timeout_result = tokio::select! {
            r = chat_fut => r,
            () = self.runtime.lifecycle.cancel_token.cancelled() => {
                tracing::info!("chat_with_tools cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                return Ok(None);
            }
        };
        match timeout_result {
            Ok(Ok(r)) => Ok(Some(r)),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                self.channel
                    .send("LLM request timed out. Please try again.")
                    .await?;
                Ok(None)
            }
        }
    }

    async fn call_non_streaming_with_span(
        &mut self,
        tool_defs: &[ToolDefinition],
        llm_timeout: std::time::Duration,
        llm_span: tracing::Span,
    ) -> Result<Option<ChatResponse>, crate::agent::error::AgentError> {
        let chat_fut = tokio::time::timeout(
            llm_timeout,
            self.provider
                .chat_with_tools(&self.msg.messages, tool_defs)
                .instrument(llm_span),
        );
        let timeout_result = tokio::select! {
            r = chat_fut => r,
            () = self.runtime.lifecycle.cancel_token.cancelled() => {
                tracing::info!("chat_with_tools cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                return Ok(None);
            }
        };
        match timeout_result {
            Ok(Ok(r)) => Ok(Some(r)),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                self.channel
                    .send("LLM request timed out. Please try again.")
                    .await?;
                Ok(None)
            }
        }
    }

    fn prepare_chat_debug_dump(
        &self,
        tool_defs: &[ToolDefinition],
        memcot_state: Option<&str>,
    ) -> Option<u32> {
        self.runtime
            .debug
            .debug_dumper
            .as_ref()
            .map(|d: &crate::debug_dump::DebugDumper| {
                // Skip expensive serialization when Trace format returns early without using it.
                let provider_request = if d.is_trace_format() {
                    serde_json::Value::Null
                } else {
                    self.provider
                        .debug_request_json(&self.msg.messages, tool_defs, false) // lgtm[rust/cleartext-logging]
                };
                d.dump_request(&crate::debug_dump::RequestDebugDump {
                    model_name: &self.runtime.config.model_name,
                    messages: &self.msg.messages,
                    tools: tool_defs,
                    provider_request,
                    memcot_state,
                })
            })
    }

    pub(super) fn preserve_thinking_blocks(&mut self, blocks: Vec<ThinkingBlock>) {
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

    #[cfg(test)]
    pub(super) async fn call_llm_with_timeout(
        &mut self,
    ) -> Result<Option<String>, crate::agent::error::AgentError> {
        if self.runtime.lifecycle.cancel_token.is_cancelled() {
            return Ok(None);
        }

        if let Some(ref tracker) = self.runtime.metrics.cost_tracker
            && let Err(e) = tracker.check_budget()
        {
            self.channel
                .send(&format!("Budget limit reached: {e}"))
                .await?;
            return Ok(None);
        }

        let query_embedding = match self.check_response_cache().await? {
            super::CacheCheckResult::Hit(resp) => return Ok(Some(resp)),
            super::CacheCheckResult::Miss { query_embedding } => query_embedding,
        };

        let llm_timeout = std::time::Duration::from_secs(self.runtime.config.timeouts.llm_seconds);
        let start = std::time::Instant::now();
        let prompt_estimate = self.runtime.providers.cached_prompt_tokens;

        let memcot_state = match self.services.memory.extraction.memcot_accumulator.as_ref() {
            Some(acc) => acc.current_state().await,
            None => None,
        };
        let dump_id =
            self.runtime
                .debug
                .debug_dumper
                .as_ref()
                .map(|d: &crate::debug_dump::DebugDumper| {
                    let provider_request = if d.is_trace_format() {
                        serde_json::Value::Null
                    } else {
                        self.provider.debug_request_json(
                            &self.msg.messages,
                            &[],
                            self.provider.supports_streaming(),
                        )
                    };
                    d.dump_request(&crate::debug_dump::RequestDebugDump {
                        model_name: &self.runtime.config.model_name,
                        messages: &self.msg.messages,
                        tools: &[],
                        provider_request,
                        memcot_state: memcot_state.as_deref(),
                    })
                });

        let trace_guard = self.runtime.debug.trace_collector.as_ref().and_then(|tc| {
            self.runtime
                .debug
                .current_iteration_span_id
                .map(|id| tc.begin_llm_request(id))
        });

        let llm_span = tracing::info_span!(
            "llm.turn_call",
            model = %self.runtime.config.model_name,
            provider = self.provider.name(),
        );
        let result = self
            .call_llm_non_streaming(
                llm_timeout,
                start,
                prompt_estimate,
                dump_id,
                llm_span,
                query_embedding,
            )
            .await;

        if let Some(guard) = trace_guard
            && let Some(ref mut tc) = self.runtime.debug.trace_collector
        {
            let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            let (prompt_tokens, completion_tokens) =
                self.provider.last_usage().unwrap_or((prompt_estimate, 0));
            tc.end_llm_request(
                guard,
                &crate::debug_dump::trace::LlmAttributes {
                    model: self.runtime.config.model_name.clone(),
                    prompt_tokens,
                    completion_tokens,
                    latency_ms: latency,
                    streaming: false,
                    cache_hit: false,
                },
            );
        }

        result
    }

    #[cfg(test)]
    async fn call_llm_non_streaming(
        &mut self,
        llm_timeout: std::time::Duration,
        start: std::time::Instant,
        prompt_estimate: u64,
        dump_id: Option<u32>,
        llm_span: tracing::Span,
        query_embedding: Option<Vec<f32>>,
    ) -> Result<Option<String>, crate::agent::error::AgentError> {
        let cancel = self.runtime.lifecycle.cancel_token.clone();
        let chat_fut = self.provider.chat(&self.msg.messages).instrument(llm_span);
        let result = tokio::select! {
            r = tokio::time::timeout(llm_timeout, chat_fut) => r,
            () = cancel.cancelled() => {
                tracing::info!("LLM call cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                return Ok(None);
            }
        };
        match result {
            Ok(Ok(resp)) => {
                let elapsed = start.elapsed();
                let latency = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                let completion_heuristic = estimate_tokens(&resp) as u64;
                let (final_prompt, final_completion) = self
                    .provider
                    .last_usage()
                    .unwrap_or((prompt_estimate, completion_heuristic));
                self.update_metrics(|m| {
                    m.api_calls += 1;
                    m.last_llm_latency_ms = latency;
                    m.context_tokens = final_prompt;
                    m.prompt_tokens += final_prompt;
                    m.completion_tokens += final_completion;
                    m.total_tokens = m.prompt_tokens + m.completion_tokens;
                });
                self.record_cost_and_cache(final_prompt, final_completion);
                self.record_successful_task();
                if let Some(ref recorder) = self.runtime.metrics.histogram_recorder {
                    recorder.observe_llm_latency(elapsed);
                }
                if self.run_response_verification(&resp) {
                    let _ = self
                        .channel
                        .send("[security] Response blocked by injection detection.")
                        .await;
                    return Ok(None);
                }
                let cleaned = self.scan_output_and_warn(&resp);
                if let (Some(d), Some(id)) = (self.runtime.debug.debug_dumper.as_ref(), dump_id) {
                    d.dump_response(id, &cleaned);
                }
                let display = self.maybe_redact(&cleaned);
                self.channel.send(&display).await?;
                self.store_response_in_cache(&cleaned, query_embedding)
                    .await;
                Ok(Some(cleaned))
            }
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                self.channel
                    .send("LLM request timed out. Please try again.")
                    .await?;
                Ok(None)
            }
        }
    }

    #[cfg(test)]
    pub(in crate::agent) async fn call_llm_with_retry(
        &mut self,
        max_attempts: usize,
    ) -> Result<Option<String>, crate::agent::error::AgentError> {
        for attempt in 0..max_attempts {
            match self.call_llm_with_timeout().await {
                Ok(result) => return Ok(result),
                Err(e) if e.is_context_length_error() && attempt + 1 < max_attempts => {
                    tracing::warn!(
                        attempt,
                        "LLM context length exceeded, compacting and retrying"
                    );
                    let _ = self
                        .channel
                        .send_status("context too long, compacting...")
                        .await;
                    let _ = self.compact_context().await?;
                    let _ = self.channel.send_status("").await;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("loop covers all attempts")
    }
}
