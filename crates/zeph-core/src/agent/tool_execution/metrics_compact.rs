// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_common::text::estimate_tokens;
use zeph_llm::provider::{
    AssistantBlock, ChatResponse, LlmProvider, Message, MessageMetadata, MessageOrigin,
    MessagePart, Role,
};
use zeph_sanitizer::{ContentSource, ContentSourceKind, ContentTrustLevel};

use crate::agent::Agent;
use crate::channel::Channel;

/// Rebuild history after a server-side compaction block was received.
///
/// Keeps every `System` message in order (system prompt, focus Knowledge block, recall, client
/// summary), replaces all other history with one `Assistant` message carrying the compaction
/// block, and keeps only the latest [`MessageOrigin::TurnInput`] message so the model still
/// sees the user's request. Mid-tool-loop the last `User` message is a tool-result carrier, so
/// cutting at it (the old behavior) orphaned or duplicated tool pairs.
///
/// The compaction message is tagged `ExternalUntrusted`: the summary is a sanitized
/// `McpResponse` and distils content of every dropped message, including tagged tool results,
/// so `context_max_trust_level` must not fall back to `Trusted` (#6558).
fn mirror_server_compaction(history: Vec<Message>, summary: String) -> Vec<Message> {
    let turn_input = history
        .iter()
        .rposition(|m| m.metadata.origin == Some(MessageOrigin::TurnInput));
    let mut out = Vec::with_capacity(history.len().min(8));
    let mut kept_input = None;
    for (i, msg) in history.into_iter().enumerate() {
        if msg.role == Role::System {
            out.push(msg);
        } else if Some(i) == turn_input {
            kept_input = Some(msg);
        }
    }
    out.push(Message {
        role: Role::Assistant,
        content: summary.clone(),
        parts: vec![MessagePart::Compaction { summary }],
        metadata: MessageMetadata {
            trust_level: Some(ContentTrustLevel::ExternalUntrusted as u8),
            ..MessageMetadata::default()
        },
    });
    out.extend(kept_input);
    out
}

impl<C: Channel> Agent<C> {
    /// Close the CR-01 LLM trace span after the chat call completes.
    pub(super) fn record_llm_trace_span_close(
        &mut self,
        guard: Option<crate::debug_dump::trace::SpanGuard>,
        start: std::time::Instant,
    ) {
        if let Some(guard) = guard
            && let Some(ref mut tc) = self.runtime.debug.trace_collector
        {
            let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            let (prompt_tokens, completion_tokens) = self.provider.last_usage().unwrap_or((0, 0));
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
    }

    #[tracing::instrument(name = "core.tool.metrics_compact", skip_all, level = "debug", err)]
    pub(super) async fn record_chat_metrics_and_compact(
        &mut self,
        start: std::time::Instant,
        result: &mut ChatResponse,
    ) -> Result<(), crate::agent::error::AgentError> {
        let elapsed = start.elapsed();
        let latency = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let prompt_estimate = self.runtime.providers.cached_prompt_tokens;
        let completion_heuristic = match result {
            ChatResponse::Text(t) => estimate_tokens(t) as u64,
            ChatResponse::ToolUse { content, .. } => {
                let text_tokens: usize = content
                    .blocks()
                    .iter()
                    .map(|b| match b {
                        AssistantBlock::Text(t) => estimate_tokens(t),
                        _ => 0,
                    })
                    .sum();
                let calls_tokens: usize = content
                    .tool_calls()
                    .map(|c| {
                        estimate_tokens(c.name.as_str()) + estimate_tokens(&c.input.to_string())
                    })
                    .sum();
                (text_tokens + calls_tokens) as u64
            }
            _ => 0,
        };
        let (final_prompt, final_completion) = self
            .provider
            .last_usage()
            .unwrap_or((prompt_estimate, completion_heuristic));
        let reasoning = self.provider.last_reasoning_tokens().unwrap_or(0);
        let router_stats = self.provider.router_thompson_stats();
        self.update_metrics(|m| {
            m.api_calls += 1;
            m.last_llm_latency_ms = latency;
            m.context_tokens = final_prompt;
            m.prompt_tokens += final_prompt;
            m.completion_tokens += final_completion;
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
            m.reasoning_tokens += reasoning;
            if !router_stats.is_empty() {
                m.router_thompson_stats = router_stats;
            }
        });
        // Track per-turn LLM call count for the notification gate.
        self.runtime.lifecycle.turn_llm_requests =
            self.runtime.lifecycle.turn_llm_requests.saturating_add(1);
        self.record_cost_and_cache(final_prompt, final_completion);
        self.record_successful_task();

        // Issue #6549: snapshot this call's usage for the durable per-message ledger.
        // Consumed at the next assistant message persist (store.rs eager-take, S2) — the
        // message_id isn't known until that SQLite INSERT returns.
        let conversation_id = self.services.memory.persistence.conversation_id;
        let stream_ttft_ms = self.runtime.metrics.stream_ttft_ms.take();
        self.runtime.metrics.pending_usage = self.build_usage_record(
            &self.provider,
            zeph_memory::UsageSource::Conversation,
            None,
            conversation_id,
            final_prompt,
            final_completion,
            latency,
            stream_ttft_ms,
        );

        if let Some(ref recorder) = self.runtime.metrics.histogram_recorder {
            recorder.observe_llm_latency(elapsed);
        }

        self.emit_usage_event().await;

        // C2: server-side compaction — prune old messages and insert synthetic compaction message.
        // The summary rides on this very response, so no other caller of a shared provider
        // can supply one.
        if let Some(raw_summary) = result.take_compaction() {
            self.channel
                .send_status_best_effort("Compacting context (server-side)...")
                .await;
            tracing::info!(
                summary_len = raw_summary.len(),
                messages_before = self.msg.messages.len(),
                "server-side compaction received; pruning old messages"
            );
            if self.apply_raw_server_compaction(&raw_summary) {
                // The response's thinking was produced under the pre-compaction prefix, which
                // the mirror just replaced; it must not be pushed behind the new prefix.
                result.strip_thinking();
            }
            self.update_metrics(|m| m.server_compaction_events += 1);
            self.channel.send_status_best_effort("").await;
        }

        Ok(())
    }

    /// Sanitize a raw server-compaction summary and mirror it into history.
    ///
    /// SEC-COMPACT-01: the summary is sanitized as `McpResponse` (`ExternalUntrusted`).
    ///
    /// Returns `false` and leaves history untouched when the summary is blank:
    /// mirroring it would replace the whole conversation with nothing.
    pub(in crate::agent) fn apply_raw_server_compaction(&mut self, raw_summary: &str) -> bool {
        if raw_summary.trim().is_empty() {
            tracing::warn!("server compaction summary is empty; keeping history");
            return false;
        }
        let source = ContentSource::new(ContentSourceKind::McpResponse);
        let sanitized = self
            .services
            .security
            .sanitizer
            .sanitize(raw_summary, source);
        self.apply_server_compaction(sanitized.body);
        true
    }

    /// Replace the in-memory history with its server-compaction mirror and refresh the derived
    /// counters (non-system count, cached prompt tokens, subgoal registry).
    pub(in crate::agent) fn apply_server_compaction(&mut self, summary: String) {
        let history = std::mem::take(&mut self.msg.messages);
        self.msg.messages = mirror_server_compaction(history, summary);
        self.services
            .compression
            .subgoal_registry
            .prune(&self.msg.messages);
        self.msg.recompute_non_system_count();
        self.recompute_prompt_tokens();
    }

    /// Emit `send_usage` with token counts and cumulative cost from the last LLM call.
    async fn emit_usage_event(&mut self) {
        let Some((input_tokens, output_tokens)) = self.provider.last_usage() else {
            return;
        };
        let context_window =
            u64::try_from(self.provider.context_window().unwrap_or(0)).unwrap_or(0);
        let (cache_read_tokens, cache_write_tokens) =
            self.provider.last_cache_usage().unwrap_or((0, 0));
        // Read cumulative cost after record_cost_and_cache updated the metrics snapshot.
        let cost_cents = self
            .runtime
            .metrics
            .metrics_tx
            .as_ref()
            .map_or(0.0, |tx| tx.borrow().cost_spent_cents);
        let _ = self
            .channel
            .send_usage(
                input_tokens,
                output_tokens,
                context_window,
                cache_read_tokens,
                cache_write_tokens,
                cost_cents,
            )
            .await;
    }
}
