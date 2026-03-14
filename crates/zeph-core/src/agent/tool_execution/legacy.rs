// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, MessagePart, Role};
use zeph_tools::executor::{ToolError, ToolOutput};

use super::super::{Agent, DOOM_LOOP_WINDOW, format_tool_output};
use super::{AnomalyOutcome, doom_loop_hash, first_tool_name};
use crate::channel::{Channel, ToolOutputEvent, ToolStartEvent};
use crate::sanitizer::{ContentSource, ContentSourceKind}; // already imported for tool output sanitization
use tokio_stream::StreamExt;
use tracing::Instrument;
use zeph_skills::evolution::FailureKind;

impl<C: Channel> Agent<C> {
    pub(crate) async fn process_response(&mut self) -> Result<(), super::super::error::AgentError> {
        // S3: clear flagged_urls at the start of each turn.
        self.security.flagged_urls.clear();

        if self.provider.supports_tool_use() {
            tracing::debug!(
                provider = self.provider.name(),
                "using native tool_use path"
            );
            return self.process_response_native_tools().await;
        }

        tracing::debug!(
            provider = self.provider.name(),
            "using legacy text extraction path"
        );
        self.tool_orchestrator.clear_doom_history();
        self.tool_orchestrator.clear_recent_tool_calls();

        for iteration in 0..self.tool_orchestrator.max_iterations {
            if self.lifecycle.cancel_token.is_cancelled() {
                tracing::info!("tool loop cancelled by user");
                break;
            }
            // None = continue loop, Some(()) = return Ok, Err = propagate
            if self.process_legacy_turn(iteration).await?.is_some() {
                return Ok(());
            }
        }

        Ok(())
    }

    /// Execute one iteration of the legacy tool loop.
    /// Returns `Ok(Some(()))` to exit the loop (return Ok), `Ok(None)` to continue, or `Err`.
    async fn process_legacy_turn(
        &mut self,
        iteration: usize,
    ) -> Result<Option<()>, super::super::error::AgentError> {
        self.channel.send_typing().await?;

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

        let _ = self.channel.send_status("thinking...").await;
        let Some(response) = self.call_llm_with_retry(2).await? else {
            let _ = self.channel.send_status("").await;
            return Ok(Some(()));
        };
        let _ = self.channel.send_status("").await;

        if response.trim().is_empty() {
            tracing::warn!("received empty response from LLM, skipping");
            self.record_skill_outcomes("empty_response", None, None)
                .await;
            if !self.learning_engine.was_reflection_used()
                && self
                    .attempt_self_reflection("LLM returned empty response", "")
                    .await?
            {
                return Ok(Some(()));
            }
            self.channel
                .send("Received an empty response. Please try again.")
                .await?;
            return Ok(Some(()));
        }

        self.persist_message(Role::Assistant, &response, &[], false)
            .await;
        self.push_message(Message {
            role: Role::Assistant,
            content: response.clone(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        self.inject_active_skill_env();
        let tool_name = first_tool_name(&response);

        // Repeat-detection (IMP-6): check BEFORE execution.
        if let Some(result) = self.check_repeat_detection(&response, tool_name).await? {
            return Ok(result);
        }

        let status_msg = format!("running {tool_name}...");
        let _ = self.channel.send_status(&status_msg).await;
        let result = self
            .tool_executor
            .execute_erased(&response)
            .instrument(tracing::info_span!("tool_exec"))
            .await;
        let _ = self.channel.send_status("").await;
        self.tool_executor.set_skill_env(None);
        if !self.handle_tool_result(&response, result).await? {
            return Ok(Some(()));
        }

        self.maybe_summarize_tool_pair().await;
        let keep_recent = 2 * self.memory_state.tool_call_cutoff + 2;
        self.prune_stale_tool_outputs(keep_recent);
        self.maybe_apply_deferred_summaries();

        // Doom-loop detection
        if let Some(last_msg) = self.messages.last() {
            let hash = doom_loop_hash(&last_msg.content);
            tracing::debug!(
                iteration,
                hash,
                content_len = last_msg.content.len(),
                content_preview = &last_msg.content[..last_msg.content.len().min(120)],
                "doom-loop hash recorded"
            );
            self.tool_orchestrator.push_doom_hash(hash);
            if self.tool_orchestrator.is_doom_loop() {
                tracing::warn!(
                    iteration,
                    hash,
                    content_len = last_msg.content.len(),
                    content_preview = &last_msg.content[..last_msg.content.len().min(200)],
                    "doom-loop detected: {DOOM_LOOP_WINDOW} consecutive identical outputs"
                );
                self.channel
                    .send("Stopping: detected repeated identical tool outputs.")
                    .await?;
                return Ok(Some(()));
            }
        }

        Ok(None)
    }

    /// Check for a repeated identical tool call (IMP-6).
    /// Returns `Ok(Some(Some(())))` to exit the loop, `Ok(Some(None))` to continue after injecting
    /// an error message, or `Ok(None)` when no repeat is detected.
    async fn check_repeat_detection(
        &mut self,
        response: &str,
        tool_name: &str,
    ) -> Result<Option<Option<()>>, super::super::error::AgentError> {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h = DefaultHasher::new();
        response.hash(&mut h);
        let args_hash = h.finish();
        if self.tool_orchestrator.is_repeat(tool_name, args_hash) {
            tracing::warn!(
                tool = tool_name,
                "[repeat-detect] identical tool call detected in legacy path"
            );
            self.tool_executor.set_skill_env(None);
            let msg = format!(
                "[error] Repeated identical call to {tool_name} detected. \
                 Use different arguments or a different approach."
            );
            if !self
                .handle_tool_result(
                    response,
                    Ok(Some(zeph_tools::ToolOutput {
                        tool_name: tool_name.to_owned(),
                        summary: msg,
                        blocks_executed: 0,
                        filter_stats: None,
                        diff: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                    })),
                )
                .await?
            {
                return Ok(Some(Some(())));
            }
            return Ok(Some(None));
        }
        self.tool_orchestrator.push_tool_call(tool_name, args_hash);
        Ok(None)
    }

    pub(super) async fn call_llm_with_timeout(
        &mut self,
    ) -> Result<Option<String>, super::super::error::AgentError> {
        if self.lifecycle.cancel_token.is_cancelled() {
            return Ok(None);
        }

        if let Some(ref tracker) = self.metrics.cost_tracker
            && let Err(e) = tracker.check_budget()
        {
            self.channel
                .send(&format!("Budget limit reached: {e}"))
                .await?;
            return Ok(None);
        }

        if let Some(resp) = self.check_response_cache().await? {
            return Ok(Some(resp));
        }

        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);
        let start = std::time::Instant::now();
        let prompt_estimate = self.providers.cached_prompt_tokens;

        let dump_id =
            self.debug_state
                .debug_dumper
                .as_ref()
                .map(|d: &crate::debug_dump::DebugDumper| {
                    d.dump_request(&crate::debug_dump::RequestDebugDump {
                        model_name: &self.runtime.model_name,
                        messages: &self.messages,
                        tools: &[],
                        provider_request: self.provider.debug_request_json(
                            &self.messages,
                            &[],
                            self.provider.supports_streaming(),
                        ),
                    })
                });

        let llm_span = tracing::info_span!("llm_call", model = %self.runtime.model_name);
        if self.provider.supports_streaming() {
            self.call_llm_streaming(llm_timeout, start, prompt_estimate, dump_id, llm_span)
                .await
        } else {
            self.call_llm_non_streaming(llm_timeout, start, prompt_estimate, dump_id, llm_span)
                .await
        }
    }

    async fn call_llm_streaming(
        &mut self,
        llm_timeout: std::time::Duration,
        start: std::time::Instant,
        prompt_estimate: u64,
        dump_id: Option<u32>,
        llm_span: tracing::Span,
    ) -> Result<Option<String>, super::super::error::AgentError> {
        let cancel = self.lifecycle.cancel_token.clone();
        let streaming_fut = self.process_response_streaming().instrument(llm_span);
        let result = tokio::select! {
            r = tokio::time::timeout(llm_timeout, streaming_fut) => r,
            () = cancel.cancelled() => {
                tracing::info!("LLM call cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                return Ok(None);
            }
        };
        let Ok(r) = result else {
            self.channel
                .send("LLM request timed out. Please try again.")
                .await?;
            return Ok(None);
        };
        let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        // completion_tokens was set by process_response_streaming via heuristic;
        // override with API-reported counts when available (non-streaming providers).
        let (final_prompt, final_completion_opt) = self
            .provider
            .last_usage()
            .map_or((prompt_estimate, None), |(p, c)| (p, Some(c)));
        self.update_metrics(|m| {
            m.api_calls += 1;
            m.last_llm_latency_ms = latency;
            m.context_tokens = final_prompt;
            m.prompt_tokens += final_prompt;
            if let Some(final_completion) = final_completion_opt {
                m.completion_tokens = m
                    .completion_tokens
                    .saturating_sub(
                        r.as_ref()
                            .map_or(0, |s| u64::try_from(s.len()).unwrap_or(0) / 4),
                    )
                    .saturating_add(final_completion);
            }
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
        });
        self.record_cache_usage();
        let cost_completion = final_completion_opt.unwrap_or_else(|| {
            r.as_ref()
                .map_or(0, |s| u64::try_from(s.len()).unwrap_or(0) / 4)
        });
        self.record_cost(final_prompt, cost_completion);
        let raw = r?;
        if let (Some(d), Some(id)) = (self.debug_state.debug_dumper.as_ref(), dump_id) {
            d.dump_response(id, &raw);
        }
        // Redact secrets from the full accumulated response before it is persisted to
        // history. Per-chunk redaction is applied during streaming (see send_chunk above).
        let redacted = self.maybe_redact(&raw).into_owned();
        // S2: scan accumulated streaming response. Per-chunk scanning not feasible
        // (markdown may split across chunk boundaries); persistence is guarded here.
        let cleaned = self.scan_output_and_warn(&redacted);
        self.store_response_in_cache(&cleaned).await;
        Ok(Some(cleaned))
    }

    async fn call_llm_non_streaming(
        &mut self,
        llm_timeout: std::time::Duration,
        start: std::time::Instant,
        prompt_estimate: u64,
        dump_id: Option<u32>,
        llm_span: tracing::Span,
    ) -> Result<Option<String>, super::super::error::AgentError> {
        let cancel = self.lifecycle.cancel_token.clone();
        let chat_fut = self.provider.chat(&self.messages).instrument(llm_span);
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
                let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let completion_heuristic = u64::try_from(resp.len()).unwrap_or(0) / 4;
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
                self.record_cache_usage();
                self.record_cost(final_prompt, final_completion);
                // S2: scan for markdown image exfiltration in non-streaming path.
                let cleaned = self.scan_output_and_warn(&resp);
                if let (Some(d), Some(id)) = (self.debug_state.debug_dumper.as_ref(), dump_id) {
                    d.dump_response(id, &cleaned);
                }
                let display = self.maybe_redact(&cleaned);
                self.channel.send(&display).await?;
                self.store_response_in_cache(&cleaned).await;
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

    /// Call LLM with retry on context length error.
    /// On `ContextLengthExceeded`, compacts context and retries up to `max_attempts` times.
    pub(in crate::agent) async fn call_llm_with_retry(
        &mut self,
        max_attempts: usize,
    ) -> Result<Option<String>, super::super::error::AgentError> {
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
                    self.compact_context().await?;
                    let _ = self.channel.send_status("").await;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("loop covers all attempts")
    }

    pub(super) async fn handle_tool_result(
        &mut self,
        response: &str,
        result: Result<Option<ToolOutput>, ToolError>,
    ) -> Result<bool, super::super::error::AgentError> {
        match result {
            Ok(Some(output)) => self.process_successful_tool_output(output).await,
            Ok(None) => {
                self.record_skill_outcomes("success", None, None).await;
                self.record_anomaly_outcome(AnomalyOutcome::Success).await?;
                Ok(false)
            }
            Err(ToolError::Blocked { command }) => {
                tracing::warn!("blocked command: {command}");
                self.channel
                    .send("This command is blocked by security policy.")
                    .await?;
                self.record_anomaly_outcome(AnomalyOutcome::Blocked).await?;
                Ok(false)
            }
            Err(ToolError::ConfirmationRequired { command }) => {
                self.handle_confirmation_required(response, &command).await
            }
            Err(ToolError::Cancelled) => {
                tracing::info!("tool execution cancelled");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                Ok(false)
            }
            Err(ToolError::SandboxViolation { path }) => {
                tracing::warn!("sandbox violation: {path}");
                self.channel
                    .send("Command targets a path outside the sandbox.")
                    .await?;
                self.record_anomaly_outcome(AnomalyOutcome::Error).await?;
                Ok(false)
            }
            Err(e) => {
                let err_str = format!("{e:#}");
                tracing::error!("tool execution error: {err_str}");
                if let Some(ref d) = self.debug_state.debug_dumper {
                    d.dump_tool_error("legacy", &e);
                }
                let kind = FailureKind::from_error(&err_str);
                // Sanitize before passing to self_reflection: error messages from MCP servers
                // and web endpoints can contain untrusted content with injection patterns.
                // Use McpResponse (ExternalUntrusted) as conservative default — tool_name is
                // not available in this error branch, and over-spotlighting local errors is
                // harmless while under-spotlighting external errors is a risk.
                let sanitized_err = self
                    .security
                    .sanitizer
                    .sanitize(&err_str, ContentSource::new(ContentSourceKind::McpResponse))
                    .body;
                self.record_skill_outcomes("tool_failure", Some(&err_str), Some(kind.as_str()))
                    .await;
                self.record_anomaly_outcome(AnomalyOutcome::Error).await?;

                if !self.learning_engine.was_reflection_used()
                    && self.attempt_self_reflection(&sanitized_err, "").await?
                {
                    return Ok(false);
                }

                self.channel
                    .send("Tool execution failed. Please try a different approach.")
                    .await?;
                Ok(false)
            }
        }
    }

    fn record_filter_metrics(&mut self, fs: &zeph_tools::FilterStats) {
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

    async fn process_successful_tool_output(
        &mut self,
        output: ToolOutput,
    ) -> Result<bool, super::super::error::AgentError> {
        if let Some(ref fs) = output.filter_stats {
            self.record_filter_metrics(fs);
        }
        if output.summary.trim().is_empty() {
            tracing::warn!("tool execution returned empty output");
            self.record_skill_outcomes("success", None, None).await;
            return Ok(false);
        }

        if output.summary.contains("[error]") || output.summary.contains("[exit code") {
            let kind = FailureKind::from_error(&output.summary);
            self.record_skill_outcomes("tool_failure", Some(&output.summary), Some(kind.as_str()))
                .await;

            if !self.learning_engine.was_reflection_used()
                && self
                    .attempt_self_reflection(&output.summary, &output.summary)
                    .await?
            {
                return Ok(false);
            }
        } else {
            self.record_skill_outcomes("success", None, None).await;
        }

        let tool_call_id = uuid::Uuid::new_v4().to_string();
        let tool_started_at = std::time::Instant::now();
        self.channel
            .send_tool_start(ToolStartEvent {
                tool_name: &output.tool_name,
                tool_call_id: &tool_call_id,
                params: None,
                parent_tool_use_id: self.parent_tool_use_id.clone(),
            })
            .await?;
        if let Some(ref d) = self.debug_state.debug_dumper {
            d.dump_tool_output(&output.tool_name, &output.summary);
        }
        let processed = self.maybe_summarize_tool_output(&output.summary).await;
        let body = if let Some(ref fs) = output.filter_stats
            && fs.filtered_chars < fs.raw_chars
        {
            format!("{}\n{processed}", fs.format_inline(&output.tool_name))
        } else {
            processed.clone()
        };
        let filter_stats_inline = output.filter_stats.as_ref().and_then(|fs| {
            (fs.filtered_chars < fs.raw_chars).then(|| fs.format_inline(&output.tool_name))
        });
        let formatted_output = format_tool_output(&output.tool_name, &body);
        self.channel
            .send_tool_output(ToolOutputEvent {
                tool_name: &output.tool_name,
                body: &self.maybe_redact(&body),
                diff: None,
                filter_stats: filter_stats_inline,
                kept_lines: None,
                locations: output.locations,
                tool_call_id: &tool_call_id,
                is_error: false,
                parent_tool_use_id: self.parent_tool_use_id.clone(),
                raw_response: output.raw_response.map(|r| self.redact_json(r)),
                started_at: Some(tool_started_at),
            })
            .await?;

        let (llm_body, has_injection_flags) = self
            .sanitize_tool_output(&processed, &output.tool_name)
            .await;
        let user_msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: output.tool_name.clone(),
                body: llm_body,
                compacted_at: None,
            }],
        );
        // C1: use injection flag state from sanitize_tool_output to guard Qdrant embedding.
        // has_injection_flags covers pure text injections (no URL); flagged_urls covers
        // URL-based exfiltration patterns. Both are OR-combined for conservative guarding.
        // Persist before push so parts are taken directly from the message being saved.
        self.persist_message(
            Role::User,
            &formatted_output,
            &user_msg.parts,
            has_injection_flags || !self.security.flagged_urls.is_empty(),
        )
        .await;
        self.push_message(user_msg);
        let outcome = if output.summary.contains("[error]") || output.summary.contains("[stderr]") {
            AnomalyOutcome::Error
        } else {
            AnomalyOutcome::Success
        };
        self.record_anomaly_outcome(outcome).await?;
        Ok(true)
    }

    async fn handle_confirmation_required(
        &mut self,
        response: &str,
        command: &str,
    ) -> Result<bool, super::super::error::AgentError> {
        let prompt = format!("Allow command: {command}?");
        if self.channel.confirm(&prompt).await? {
            if let Ok(Some(out)) = self.tool_executor.execute_confirmed_erased(response).await {
                let confirmed_tool_call_id = uuid::Uuid::new_v4().to_string();
                let confirmed_started_at = std::time::Instant::now();
                self.channel
                    .send_tool_start(ToolStartEvent {
                        tool_name: &out.tool_name,
                        tool_call_id: &confirmed_tool_call_id,
                        params: None,
                        parent_tool_use_id: self.parent_tool_use_id.clone(),
                    })
                    .await?;
                if let Some(ref d) = self.debug_state.debug_dumper {
                    d.dump_tool_output(&out.tool_name, &out.summary);
                }
                let processed = self.maybe_summarize_tool_output(&out.summary).await;
                let formatted = format_tool_output(&out.tool_name, &processed);
                self.channel
                    .send_tool_output(ToolOutputEvent {
                        tool_name: &out.tool_name,
                        body: &self.maybe_redact(&processed),
                        diff: None,
                        filter_stats: None,
                        kept_lines: None,
                        locations: out.locations,
                        tool_call_id: &confirmed_tool_call_id,
                        is_error: false,
                        parent_tool_use_id: self.parent_tool_use_id.clone(),
                        raw_response: out.raw_response.map(|r| self.redact_json(r)),
                        started_at: Some(confirmed_started_at),
                    })
                    .await?;
                let (llm_body, has_injection_flags) =
                    self.sanitize_tool_output(&processed, &out.tool_name).await;
                let confirmed_msg = Message::from_parts(
                    Role::User,
                    vec![MessagePart::ToolOutput {
                        tool_name: out.tool_name.clone(),
                        body: llm_body,
                        compacted_at: None,
                    }],
                );
                // C1: same as above — OR injection flags with flagged_urls for guarding.
                // Persist before push so parts are taken directly from the message being saved.
                self.persist_message(
                    Role::User,
                    &formatted,
                    &confirmed_msg.parts,
                    has_injection_flags || !self.security.flagged_urls.is_empty(),
                )
                .await;
                self.push_message(confirmed_msg);
            }
        } else {
            self.channel.send("Command cancelled.").await?;
        }
        Ok(false)
    }

    pub(super) async fn process_response_streaming(
        &mut self,
    ) -> Result<String, super::super::error::AgentError> {
        let mut stream = self.provider.chat_stream(&self.messages).await?;
        let mut response = String::with_capacity(2048);

        loop {
            let chunk_result = tokio::select! {
                item = stream.next() => match item {
                    Some(r) => r,
                    None => break,
                },
                () = super::super::shutdown_signal(&mut self.lifecycle.shutdown) => {
                    tracing::info!("streaming interrupted by shutdown");
                    break;
                }
                () = self.lifecycle.cancel_token.cancelled() => {
                    tracing::info!("streaming interrupted by cancellation");
                    break;
                }
            };
            match chunk_result? {
                zeph_llm::StreamChunk::Content(chunk) => {
                    response.push_str(&chunk);
                    let display_chunk = self.maybe_redact(&chunk);
                    self.channel.send_chunk(&display_chunk).await?;
                }
                zeph_llm::StreamChunk::Thinking(thinking) => {
                    self.channel.send_thinking_chunk(&thinking).await?;
                }
                zeph_llm::StreamChunk::ToolUse(calls) => {
                    tracing::warn!(
                        count = calls.len(),
                        names = ?calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                        "tool calls received in streaming path (not handled; use chat_with_tools for tool execution)"
                    );
                }
                zeph_llm::StreamChunk::Compaction(raw_summary) => {
                    let _ = self
                        .channel
                        .send_status("Compacting context (server-side)...")
                        .await;
                    // SEC-COMPACT-01: sanitize compaction summary before inserting into context.
                    // Use McpResponse (ExternalUntrusted) as the conservative trust level.
                    let source = ContentSource::new(ContentSourceKind::McpResponse);
                    let sanitized = self.security.sanitizer.sanitize(&raw_summary, source);
                    let summary = sanitized.body;
                    tracing::info!(
                        summary_len = summary.len(),
                        messages_before = self.messages.len(),
                        "server-side compaction received via stream; pruning old messages"
                    );
                    let last_user = self
                        .messages
                        .iter()
                        .rposition(|m| m.role == Role::User)
                        .unwrap_or(0);
                    let tail: Vec<Message> = self.messages.drain(last_user..).collect();
                    self.messages.clear();
                    self.messages.push(Message {
                        role: Role::Assistant,
                        content: summary.clone(),
                        parts: vec![MessagePart::Compaction {
                            summary: summary.clone(),
                        }],
                        metadata: MessageMetadata::default(),
                    });
                    self.messages.extend(tail);
                    self.update_metrics(|m| m.server_compaction_events += 1);
                    let _ = self.channel.send_status("").await;
                }
            }
        }

        self.channel.flush_chunks().await?;

        // For streaming paths, last_usage() is None (providers don't return usage in streams),
        // so the heuristic is the fallback. For non-streaming via this path, use real counts.
        let completion_heuristic = u64::try_from(response.len()).unwrap_or(0) / 4;
        let completion_tokens = self
            .provider
            .last_usage()
            .map_or(completion_heuristic, |(_, out)| out);
        self.update_metrics(|m| {
            m.completion_tokens += completion_tokens;
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
        });

        Ok(response)
    }

    /// Returns `true` if a doom loop was detected and the caller should break.
    pub(super) async fn check_doom_loop(
        &mut self,
        iteration: usize,
    ) -> Result<bool, super::super::error::AgentError> {
        if let Some(last_msg) = self.messages.last() {
            let hash = doom_loop_hash(&last_msg.content);
            tracing::debug!(
                iteration,
                hash,
                content_len = last_msg.content.len(),
                content_preview = &last_msg.content[..last_msg.content.len().min(120)],
                "doom-loop hash recorded"
            );
            self.tool_orchestrator.push_doom_hash(hash);
            if self.tool_orchestrator.is_doom_loop() {
                tracing::warn!(
                    iteration,
                    hash,
                    content_len = last_msg.content.len(),
                    content_preview = &last_msg.content[..last_msg.content.len().min(200)],
                    "doom-loop detected: {DOOM_LOOP_WINDOW} consecutive identical outputs"
                );
                self.channel
                    .send("Stopping: detected repeated identical tool outputs.")
                    .await?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}
