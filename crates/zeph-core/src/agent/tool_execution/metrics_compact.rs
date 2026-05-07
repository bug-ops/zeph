// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_common::text::estimate_tokens;
use zeph_llm::provider::{ChatResponse, LlmProvider, Message, MessageMetadata, MessagePart, Role};
use zeph_sanitizer::{ContentSource, ContentSourceKind};

use crate::agent::Agent;
use crate::channel::Channel;

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
        result: &ChatResponse,
    ) -> Result<(), crate::agent::error::AgentError> {
        let elapsed = start.elapsed();
        let latency = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let prompt_estimate = self.runtime.providers.cached_prompt_tokens;
        let completion_heuristic = match result {
            ChatResponse::Text(t) => estimate_tokens(t) as u64,
            ChatResponse::ToolUse {
                text, tool_calls, ..
            } => {
                let text_tokens = estimate_tokens(text.as_deref().unwrap_or(""));
                let calls_tokens: usize = tool_calls
                    .iter()
                    .map(|c| {
                        estimate_tokens(c.name.as_str()) + estimate_tokens(&c.input.to_string())
                    })
                    .sum();
                (text_tokens + calls_tokens) as u64
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
        // Track per-turn LLM call count for the notification gate.
        self.runtime.lifecycle.turn_llm_requests =
            self.runtime.lifecycle.turn_llm_requests.saturating_add(1);
        self.record_cost_and_cache(final_prompt, final_completion);
        self.record_successful_task();

        if let Some(ref recorder) = self.runtime.metrics.histogram_recorder {
            recorder.observe_llm_latency(elapsed);
        }

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
            let sanitized = self
                .services
                .security
                .sanitizer
                .sanitize(&raw_summary, source);
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
}
