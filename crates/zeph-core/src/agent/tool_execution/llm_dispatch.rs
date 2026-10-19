// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tracing::Instrument;
use zeph_llm::provider::{ChatResponse, LlmProvider, ToolDefinition};

use crate::agent::Agent;
use crate::agent::speculative::stream_drainer::DrainedTurn;
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
        let mut thinking_stripped = false;
        for attempt in 0..max_attempts {
            match self.call_chat_with_tools(tool_defs).await {
                Ok(result) => return Ok(result),
                Err(e)
                    if e.is_thinking_binding_mismatch()
                        && !thinking_stripped
                        && attempt + 1 < max_attempts =>
                {
                    // Recovery when `drop_block` is unavailable (beta latched off, unknown model)
                    // or a history rewrite escaped the seam: drop all thinking, retry once.
                    tracing::warn!(
                        attempt,
                        "thinking binding mismatch; stripping thinking from history and retrying"
                    );
                    zeph_llm::provider::strip_thinking_from(&mut self.msg.messages, 0);
                    thinking_stripped = true;
                }
                Err(e) if e.is_context_length_error() && attempt + 1 < max_attempts => {
                    tracing::warn!(
                        attempt,
                        "chat_with_tools context length exceeded, compacting and retrying"
                    );
                    self.channel
                        .send_status_best_effort("context too long, compacting...")
                        .await;
                    let _ = self.compact_context().await?;
                    self.channel.send_status_best_effort("").await;
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
                    self.channel
                        .send_status_best_effort(
                            "server compaction unavailable, falling back to client-side...",
                        )
                        .await;
                    self.channel.send_status_best_effort("").await;
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
            self.update_metrics(|m| m.cost_budget_exhausted += 1);
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

        // RuntimeLayer before_chat hooks (MVP: empty vec = zero iterations).
        if let Some(sc) = self.run_before_chat_layers(tool_defs).await? {
            return Ok(Some(sc));
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

        // PAAC secret masking (#5437) is a structural choke point at the provider boundary
        // (`AnyProvider::masked`/`MaskedProvider` in `zeph-llm`), not a per-call-site concern —
        // `self.provider` masks registered secrets from `messages` transparently before every
        // `chat*`/`debug_request_json` call, so no explicit masking step is needed here.
        let dump_id = self.prepare_chat_debug_dump(tool_defs, memcot_state_for_dump.as_deref());

        let Some(mut result) = self
            .dispatch_chat_with_tools(tool_defs, llm_timeout, llm_span)
            .await?
        else {
            return Ok(None);
        };

        self.sync_secret_mask_metric();
        self.record_chat_metrics_and_compact(start, &mut result)
            .await?;

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
            )
            .with_gate(self.dispatch_gate());
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
                Ok(Ok(DrainedTurn {
                    response: resp,
                    ttft_ms,
                })) => {
                    // Issue #6549: true TTFT from the speculative stream, consumed by the
                    // next build_usage_record call in record_chat_metrics_and_compact.
                    self.runtime.metrics.stream_ttft_ms = ttft_ms;
                    Ok(Some(resp))
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "speculative SSE stream failed, falling back");
                    self.call_non_streaming(tool_defs, llm_timeout, tracing::Span::none())
                        .await
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
        self.call_non_streaming(tool_defs, llm_timeout, llm_span)
            .await
    }

    /// Dispatch a single non-streaming `chat_with_tools` call under a timeout, racing
    /// cancellation. `llm_span` instruments the chat future; pass `tracing::Span::none()`
    /// (a documented no-op for `.instrument()`) when no outer span should wrap the call,
    /// e.g. the speculative-stream fallback path which is not part of the main LLM span.
    async fn call_non_streaming(
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

    /// Update the `secret_mask_applied` metric from the primary provider's running total
    /// (#5437). Masking is a structural choke point at the provider boundary now
    /// (`AnyProvider::masked_call_count`), so this mirrors that counter into the metrics
    /// snapshot rather than incrementing per-call-site — `masked_call_count` returns `None`
    /// for an unwrapped (unmasked) provider, in which case the metric stays at 0.
    pub(crate) fn sync_secret_mask_metric(&mut self) {
        if let Some(count) = self.provider.masked_call_count() {
            self.update_metrics(|m| m.secret_mask_applied = count);
        }
    }

    fn prepare_chat_debug_dump(
        &self,
        tool_defs: &[ToolDefinition],
        memcot_state: Option<&str>,
    ) -> Option<u32> {
        // `self.provider.debug_request_json` masks registered secrets internally when wrapped
        // via `AnyProvider::masked` (#5437) — but `RequestDebugDump.messages` is ALSO serialized
        // directly by `json_dump`/`raw_dump` (independent of `provider_request`, for providers
        // whose wire format doesn't carry a full messages array or for the JSON dump's own
        // "messages" field), so it needs its own masked view; the provider abstraction can't
        // cover this since it's a local file-serialization concern, not an outbound wire call.
        // Computed lazily inside the `debug_dumper.is_some()` branch below — `debug_dumper` is
        // `None` in ordinary production runs, and `mask_messages` is not free even with its own
        // non-cloning pre-scan (it still walks every message's text), so this must not run on
        // every dispatch when there is no dump to build.
        self.runtime
            .debug
            .debug_dumper
            .as_ref()
            .map(|d: &crate::debug_dump::DebugDumper| {
                let masked_for_dump = self
                    .services
                    .security
                    .secret_registry
                    .as_deref()
                    .and_then(|r| zeph_llm::mask_messages(r, &self.msg.messages));
                let messages_for_dump = masked_for_dump.as_deref().unwrap_or(&self.msg.messages);
                // Skip expensive serialization when Trace format returns early without using it.
                let provider_request = if d.is_trace_format() {
                    serde_json::Value::Null
                } else {
                    self.provider
                        .debug_request_json(messages_for_dump, tool_defs, false) // lgtm[rust/cleartext-logging]
                };
                d.dump_request(&crate::debug_dump::RequestDebugDump {
                    model_name: &self.runtime.config.model_name,
                    messages: messages_for_dump,
                    tools: tool_defs,
                    provider_request,
                    memcot_state,
                })
            })
    }
}
