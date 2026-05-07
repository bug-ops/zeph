// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_common::text::estimate_tokens;
#[cfg(test)]
use zeph_llm::provider::LlmProvider;
use zeph_llm::provider::MAX_TOKENS_TRUNCATION_MARKER;
use zeph_llm::provider::{ChatResponse, Message, MessagePart, Role, ToolDefinition};

use super::tool_def_to_definition_with_tafc;
use crate::agent::Agent;
use crate::channel::{Channel, StopHint};
#[cfg(test)]
use tracing::Instrument;

impl<C: Channel> Agent<C> {
    #[tracing::instrument(name = "core.tool.process_response", skip_all, level = "debug", err)]
    pub(crate) async fn process_response(&mut self) -> Result<(), crate::agent::error::AgentError> {
        self.services.security.flagged_urls.clear();
        self.process_response_native_tools().await
    }
    #[tracing::instrument(name = "core.tool.native_loop", skip_all, level = "debug", err)]
    pub(super) async fn process_response_native_tools(
        &mut self,
    ) -> Result<(), crate::agent::error::AgentError> {
        self.tool_orchestrator.clear_doom_history();
        self.tool_orchestrator.clear_recent_tool_calls();
        self.tool_orchestrator.clear_utility_state();

        // `mut` required when context-compression is enabled to inject focus tool definitions.
        let tafc = &self.tool_orchestrator.tafc;
        let mut tool_defs: Vec<ToolDefinition> = self
            .tool_executor
            .tool_definitions_erased()
            .iter()
            .map(|def| tool_def_to_definition_with_tafc(def, tafc))
            .collect();

        // Inject focus tool definitions when the feature is enabled and configured (#1850).
        if self.services.focus.config.enabled {
            tool_defs.extend(crate::agent::focus::focus_tool_definitions());
        }

        // Inject compress_context tool — always available when context-compression is enabled (#2218).
        tool_defs.push(crate::agent::focus::compress_context_tool_definition());

        // Pre-compute the full tool set for iterations 1+ before filtering.
        let all_tool_defs = tool_defs.clone();

        // Iteration 0: apply dynamic tool schema filter (#2020) if cached IDs are available.
        if let Some(ref filtered_ids) = self.services.tool_state.cached_filtered_tool_ids {
            tool_defs.retain(|d| filtered_ids.contains(d.name.as_str()));
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
            if *self.runtime.lifecycle.shutdown.borrow() {
                tracing::info!("native tool loop interrupted by shutdown");
                break;
            }
            if self.runtime.lifecycle.cancel_token.is_cancelled() {
                tracing::info!("native tool loop cancelled by user");
                break;
            }
            // Iteration 0 uses filtered tool_defs (schema filter + dependency gates).
            // Iterations 1+ expand to the full set but still apply hard dependency gates
            // so tools with unmet `requires` cannot re-enter through the expansion path (#2024).
            let defs_for_iter: Vec<ToolDefinition>;
            let defs_for_turn: &[ToolDefinition] = if iteration == 0 {
                &tool_defs
            } else {
                defs_for_iter = build_gated_defs_for_iteration(
                    iteration,
                    &all_tool_defs,
                    &self.services.tool_state,
                );
                &defs_for_iter
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

    /// Returns `true` if a doom loop was detected and the caller should break.
    async fn check_doom_loop(
        &mut self,
        iteration: usize,
    ) -> Result<bool, crate::agent::error::AgentError> {
        if let Some(last_msg) = self.msg.messages.last() {
            let hash = zeph_agent_tools::doom_loop_hash(&last_msg.content);
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
                    "doom-loop detected: {} consecutive identical outputs",
                    crate::agent::DOOM_LOOP_WINDOW
                );
                self.channel
                    .send("Stopping: detected repeated identical tool outputs.")
                    .await?;
                return Ok(true);
            }
        }
        Ok(false)
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

    #[cfg(test)]
    pub(super) async fn handle_tool_result(
        &mut self,
        response: &str,
        result: Result<Option<zeph_tools::executor::ToolOutput>, zeph_tools::executor::ToolError>,
    ) -> Result<bool, crate::agent::error::AgentError> {
        use zeph_sanitizer::{ContentSource, ContentSourceKind};
        use zeph_skills::evolution::FailureKind;
        use zeph_tools::executor::ToolError;
        match result {
            Ok(Some(output)) => self.process_successful_tool_output(output).await,
            Ok(None) => {
                self.record_skill_outcomes("success", None, None).await;
                self.record_anomaly_outcome(super::AnomalyOutcome::Success)
                    .await?;
                Ok(false)
            }
            Err(ToolError::Blocked { command }) => {
                tracing::warn!("blocked command: {command}");
                self.channel
                    .send("This command is blocked by security policy.")
                    .await?;
                self.record_anomaly_outcome(super::AnomalyOutcome::Blocked)
                    .await?;
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
                self.record_anomaly_outcome(super::AnomalyOutcome::Error)
                    .await?;
                Ok(false)
            }
            Err(e) => {
                let category = e.category();
                let err_str = format!("{e:#}");
                tracing::error!("tool execution error: {err_str}");
                if let Some(ref d) = self.runtime.debug.debug_dumper {
                    d.dump_tool_error("legacy", &e);
                }
                let kind = FailureKind::from(category);
                let sanitized_err = self
                    .services
                    .security
                    .sanitizer
                    .sanitize(&err_str, ContentSource::new(ContentSourceKind::McpResponse))
                    .body;
                self.record_skill_outcomes("tool_failure", Some(&err_str), Some(kind.as_str()))
                    .await;
                self.record_anomaly_outcome(super::AnomalyOutcome::Error)
                    .await?;

                if !self.services.learning_engine.was_reflection_used()
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

    /// Record skill learning outcomes for a tool output and optionally trigger self-reflection.
    ///
    /// Returns `Ok(true)` when the caller should return early (reflection consumed the turn),
    /// `Ok(false)` to continue, or `Err` on a hard error.
    #[cfg(test)]
    async fn record_tool_output_outcome(
        &mut self,
        output: &zeph_tools::executor::ToolOutput,
    ) -> Result<bool, crate::agent::error::AgentError> {
        use zeph_skills::evolution::FailureKind;

        if let Some(ref fs) = output.filter_stats {
            self.record_filter_metrics(fs);
        }
        if output.summary.trim().is_empty() {
            tracing::warn!("tool execution returned empty output");
            self.record_skill_outcomes("success", None, None).await;
            return Ok(true);
        }
        if output.summary.contains("[error]") || output.summary.contains("[exit code") {
            let kind = FailureKind::from_error(&output.summary);
            self.record_skill_outcomes("tool_failure", Some(&output.summary), Some(kind.as_str()))
                .await;
            if !self.services.learning_engine.was_reflection_used()
                && self
                    .attempt_self_reflection(&output.summary, &output.summary)
                    .await?
            {
                return Ok(true);
            }
        } else {
            self.record_skill_outcomes("success", None, None).await;
        }
        Ok(false)
    }

    #[cfg(test)]
    async fn process_successful_tool_output(
        &mut self,
        output: zeph_tools::executor::ToolOutput,
    ) -> Result<bool, crate::agent::error::AgentError> {
        use crate::agent::format_tool_output;
        use crate::channel::{ToolOutputEvent, ToolStartEvent};
        use zeph_llm::provider::{Message, MessagePart, Role};

        if self.record_tool_output_outcome(&output).await? {
            return Ok(false);
        }

        let tool_call_id = uuid::Uuid::new_v4().to_string();
        let tool_started_at = std::time::Instant::now();
        self.channel
            .send_tool_start(ToolStartEvent {
                tool_name: output.tool_name.clone(),
                tool_call_id: tool_call_id.clone(),
                params: None,
                parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                started_at: std::time::Instant::now(),
                speculative: false,
                sandbox_profile: None,
            })
            .await?;
        if let Some(ref d) = self.runtime.debug.debug_dumper {
            let dump_content = if self.services.security.pii_filter.is_enabled() {
                self.services
                    .security
                    .pii_filter
                    .scrub(&output.summary)
                    .into_owned()
            } else {
                output.summary.clone()
            };
            d.dump_tool_output(output.tool_name.as_str(), &dump_content);
        }
        let processed = self.maybe_summarize_tool_output(&output.summary).await;
        let body = if let Some(ref fs) = output.filter_stats
            && fs.filtered_chars < fs.raw_chars
        {
            format!(
                "{}\n{processed}",
                fs.format_inline(output.tool_name.as_str())
            )
        } else {
            processed.clone()
        };
        let filter_stats_inline = output.filter_stats.as_ref().and_then(|fs| {
            (fs.filtered_chars < fs.raw_chars).then(|| fs.format_inline(output.tool_name.as_str()))
        });
        let formatted_output = format_tool_output(output.tool_name.as_str(), &body);
        self.channel
            .send_tool_output(ToolOutputEvent {
                tool_name: output.tool_name.clone(),
                display: self.maybe_redact(&body).to_string(),
                diff: None,
                filter_stats: filter_stats_inline,
                kept_lines: None,
                locations: output.locations,
                tool_call_id: tool_call_id.clone(),
                terminal_id: None,
                is_error: false,
                parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                raw_response: None,
                started_at: Some(tool_started_at),
            })
            .await?;

        let (llm_body, has_injection_flags) = self
            .sanitize_tool_output(&processed, output.tool_name.as_str())
            .await;
        let user_msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: output.tool_name.clone(),
                body: llm_body,
                compacted_at: None,
            }],
        );
        self.persist_message(
            Role::User,
            &formatted_output,
            &user_msg.parts,
            has_injection_flags || !self.services.security.flagged_urls.is_empty(),
        )
        .await;
        self.push_message(user_msg);
        let outcome = if output.summary.contains("[error]") || output.summary.contains("[stderr]") {
            super::AnomalyOutcome::Error
        } else {
            super::AnomalyOutcome::Success
        };
        self.record_anomaly_outcome(outcome).await?;
        Ok(true)
    }

    #[cfg(test)]
    async fn handle_confirmation_required(
        &mut self,
        response: &str,
        command: &str,
    ) -> Result<bool, crate::agent::error::AgentError> {
        use crate::agent::format_tool_output;
        use crate::channel::{ToolOutputEvent, ToolStartEvent};
        use zeph_llm::provider::{Message, MessagePart, Role};
        let prompt = format!("Allow command: {command}?");
        if self.channel.confirm(&prompt).await? {
            if let Ok(Some(out)) = self.tool_executor.execute_confirmed_erased(response).await {
                let confirmed_tool_call_id = uuid::Uuid::new_v4().to_string();
                let confirmed_started_at = std::time::Instant::now();
                self.channel
                    .send_tool_start(ToolStartEvent {
                        tool_name: out.tool_name.clone(),
                        tool_call_id: confirmed_tool_call_id.clone(),
                        params: None,
                        parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                        started_at: std::time::Instant::now(),
                        speculative: false,
                        sandbox_profile: None,
                    })
                    .await?;
                if let Some(ref d) = self.runtime.debug.debug_dumper {
                    let dump_content = if self.services.security.pii_filter.is_enabled() {
                        self.services
                            .security
                            .pii_filter
                            .scrub(&out.summary)
                            .into_owned()
                    } else {
                        out.summary.clone()
                    };
                    d.dump_tool_output(out.tool_name.as_str(), &dump_content);
                }
                let processed = self.maybe_summarize_tool_output(&out.summary).await;
                let formatted = format_tool_output(out.tool_name.as_str(), &processed);
                self.channel
                    .send_tool_output(ToolOutputEvent {
                        tool_name: out.tool_name.clone(),
                        display: self.maybe_redact(&processed).to_string(),
                        diff: None,
                        filter_stats: None,
                        kept_lines: None,
                        locations: out.locations,
                        tool_call_id: confirmed_tool_call_id.clone(),
                        terminal_id: None,
                        is_error: false,
                        parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                        raw_response: None,
                        started_at: Some(confirmed_started_at),
                    })
                    .await?;
                let (llm_body, has_injection_flags) = self
                    .sanitize_tool_output(&processed, out.tool_name.as_str())
                    .await;
                let confirmed_msg = Message::from_parts(
                    Role::User,
                    vec![MessagePart::ToolOutput {
                        tool_name: out.tool_name.clone(),
                        body: llm_body,
                        compacted_at: None,
                    }],
                );
                self.persist_message(
                    Role::User,
                    &formatted,
                    &confirmed_msg.parts,
                    has_injection_flags || !self.services.security.flagged_urls.is_empty(),
                )
                .await;
                self.push_message(confirmed_msg);
            }
        } else {
            self.channel.send("Command cancelled.").await?;
        }
        Ok(false)
    }

    /// Execute one turn of the native tool loop. Returns `Ok(Some(()))` when the LLM produced
    /// a terminal text response (caller should return `Ok(())`), `Ok(None)` to continue the
    /// loop, or `Err` on a hard error.
    #[tracing::instrument(
        name = "core.tool.single_turn",
        skip_all,
        level = "debug",
        fields(iteration),
        err
    )]
    async fn process_single_native_turn(
        &mut self,
        tool_defs: &[ToolDefinition],
        iteration: usize,
        query_embedding: Option<Vec<f32>>,
    ) -> Result<Option<()>, crate::agent::error::AgentError> {
        // Track iteration for BudgetHint injection (#2267).
        self.services.tool_state.current_tool_iteration = iteration;
        self.channel.send_typing().await?;

        if let Some(ref budget) = self.context_manager.budget {
            let used =
                usize::try_from(self.runtime.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
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
            self.maybe_log_compression_failure(&cleaned).await;
            if cleaned.contains(MAX_TOKENS_TRUNCATION_MARKER) {
                let _ = self.channel.send_stop_hint(StopHint::MaxTokens).await;
            }
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
        let keep_recent = 2 * self.services.memory.persistence.tool_call_cutoff + 2;
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
    fn rebuild_knowledge_block(&mut self) {
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

/// Build the tool definition slice for iterations 1+ of the native tool loop.
///
/// Applies hard dependency-gate filtering when a dependency graph is configured, ensuring tools
/// with unmet `requires` cannot re-enter through the expansion path after iteration 0 (#2024).
///
/// Returns the allowed set as an owned `Vec`; the caller holds a reference into it.
/// When no dependency graph is present the full `all_tool_defs` slice is returned as-is (cloned).
fn build_gated_defs_for_iteration(
    iteration: usize,
    all_tool_defs: &[ToolDefinition],
    tool_state: &crate::agent::state::ToolState,
) -> Vec<ToolDefinition> {
    let Some(ref dep_graph) = tool_state.dependency_graph else {
        return all_tool_defs.to_vec();
    };
    if dep_graph.is_empty() {
        return all_tool_defs.to_vec();
    }

    let names: Vec<&str> = all_tool_defs.iter().map(|d| d.name.as_str()).collect();
    let allowed = dep_graph.filter_tool_names(
        &names,
        &tool_state.completed_tool_ids,
        &tool_state.dependency_always_on,
    );
    let allowed_set: std::collections::HashSet<&str> = allowed.into_iter().collect();

    // Deadlock fallback: if all non-always-on tools would be blocked, use the full set.
    let non_ao_allowed = allowed_set
        .iter()
        .filter(|n| !tool_state.dependency_always_on.contains(**n))
        .count();
    let non_ao_total = all_tool_defs
        .iter()
        .filter(|d| !tool_state.dependency_always_on.contains(d.name.as_str()))
        .count();
    if non_ao_allowed == 0 && non_ao_total > 0 {
        tracing::warn!(
            iteration,
            "tool dependency graph: all non-always-on tools gated on iter 1+; \
             disabling hard gates for this iteration"
        );
        return all_tool_defs.to_vec();
    }

    all_tool_defs
        .iter()
        .filter(|d| allowed_set.contains(d.name.as_str()))
        .cloned()
        .collect()
}

/// Receive the next elicitation event from an optional channel without blocking.
///
/// Returns `None` when the receiver is absent (no MCP elicitation configured) or the channel
/// is closed, causing the `select!` branch to be disabled rather than polling indefinitely.

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use zeph_llm::provider::{ChatResponse, Message, MessagePart, Role};

    use crate::agent::Agent;
    use crate::agent::tests::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use crate::metrics::HistogramRecorder;

    fn make_agent() -> Agent<MockChannel> {
        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.services.focus.config.enabled = true;
        // System prompt at index 0 (required by complete_focus insert logic)
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::System, "system"));
        agent
    }

    /// Helper: call `handle_focus_tool` and flush the pending checkpoint into agent history,
    /// simulating the deferred insertion that `execute_tool_calls_batch` performs (#3262).
    fn call_focus_tool(
        agent: &mut Agent<MockChannel>,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> String {
        let (result, maybe_checkpoint) = agent.handle_focus_tool(tool_name, input);
        if let Some(cp) = maybe_checkpoint {
            agent.push_message(cp);
        }
        result
    }

    #[test]
    fn start_focus_happy_path_inserts_pinned_checkpoint() {
        let mut agent = make_agent();
        let input = serde_json::json!({"scope": "reading auth files"});
        let result = call_focus_tool(&mut agent, "start_focus", &input);

        assert!(
            !result.starts_with("[error]"),
            "start_focus must not return error: {result}"
        );
        assert!(
            agent.services.focus.is_active(),
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
    fn start_focus_checkpoint_inserted_after_tool_result() {
        // Verify that when the deferred pattern is used, the checkpoint lands AFTER
        // the tool-result User message, maintaining valid OpenAI ordering (#3262).
        let mut agent = make_agent();

        // Simulate assistant message with tool call already in history
        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![MessagePart::ToolUse {
                id: "call_test_1".to_string(),
                name: "start_focus".to_string(),
                input: serde_json::json!({"scope": "test"}),
            }],
            metadata: zeph_llm::provider::MessageMetadata::default(),
        });

        // Capture pending checkpoint WITHOUT flushing it yet
        let (result, maybe_checkpoint) =
            agent.handle_focus_tool("start_focus", &serde_json::json!({"scope": "test"}));
        assert!(!result.starts_with("[error]"));
        assert!(
            maybe_checkpoint.is_some(),
            "start_focus must return a pending checkpoint"
        );

        // Simulate push_message(user_msg) for tool result — happens before checkpoint
        let tool_result_msg = Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "call_test_1".to_string(),
                content: result.clone(),
                is_error: false,
            }],
            metadata: zeph_llm::provider::MessageMetadata::default(),
        };
        agent.msg.messages.push(tool_result_msg);

        // Now flush checkpoint — must land after tool result
        if let Some(cp) = maybe_checkpoint {
            agent.push_message(cp);
        }

        let tool_result_pos = agent.msg.messages.iter().position(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
        });
        let checkpoint_pos = agent
            .msg
            .messages
            .iter()
            .position(|m| m.metadata.focus_marker_id.is_some());
        assert!(tool_result_pos.is_some(), "tool result must be in history");
        assert!(checkpoint_pos.is_some(), "checkpoint must be in history");
        assert!(
            tool_result_pos.unwrap() < checkpoint_pos.unwrap(),
            "tool result (pos={}) must precede checkpoint (pos={})",
            tool_result_pos.unwrap(),
            checkpoint_pos.unwrap()
        );
    }

    #[test]
    fn start_focus_errors_when_already_active() {
        let mut agent = make_agent();
        call_focus_tool(
            &mut agent,
            "start_focus",
            &serde_json::json!({"scope": "first"}),
        );
        let result = call_focus_tool(
            &mut agent,
            "start_focus",
            &serde_json::json!({"scope": "second"}),
        );
        assert!(
            result.starts_with("[error]"),
            "second start_focus must return error: {result}"
        );
    }

    #[test]
    fn complete_focus_errors_when_no_active_session() {
        let mut agent = make_agent();
        let result = call_focus_tool(
            &mut agent,
            "complete_focus",
            &serde_json::json!({"summary": "done"}),
        );
        assert!(
            result.starts_with("[error]"),
            "complete_focus without active session must error: {result}"
        );
    }

    #[test]
    fn complete_focus_happy_path_clears_session_and_appends_knowledge() {
        let mut agent = make_agent();
        call_focus_tool(
            &mut agent,
            "start_focus",
            &serde_json::json!({"scope": "test"}),
        );
        // Add some messages in the focus window
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::User, "some work"));
        let result = call_focus_tool(
            &mut agent,
            "complete_focus",
            &serde_json::json!({"summary": "learned stuff"}),
        );
        assert!(
            !result.starts_with("[error]"),
            "complete_focus must not error: {result}"
        );
        assert!(
            !agent.services.focus.is_active(),
            "focus session must be cleared after complete_focus"
        );
        assert!(
            !agent.services.focus.knowledge_blocks.is_empty(),
            "knowledge must be appended"
        );
    }

    #[test]
    fn complete_focus_marker_not_found_returns_error() {
        let mut agent = make_agent();
        call_focus_tool(
            &mut agent,
            "start_focus",
            &serde_json::json!({"scope": "test"}),
        );
        // Remove checkpoint by hand to simulate marker eviction
        agent
            .msg
            .messages
            .retain(|m| m.metadata.focus_marker_id.is_none());
        let result = call_focus_tool(
            &mut agent,
            "complete_focus",
            &serde_json::json!({"summary": "done"}),
        );
        assert!(
            result.starts_with("[error]"),
            "must return error when checkpoint not found (S4): {result}"
        );
    }

    #[test]
    fn complete_focus_truncates_bracketed_messages() {
        let mut agent = make_agent();
        call_focus_tool(
            &mut agent,
            "start_focus",
            &serde_json::json!({"scope": "test"}),
        );
        let before_len = agent.msg.messages.len();
        // Add 3 messages in the focus window
        for i in 0..3 {
            agent
                .msg
                .messages
                .push(Message::from_legacy(Role::User, format!("msg {i}")));
        }
        call_focus_tool(
            &mut agent,
            "complete_focus",
            &serde_json::json!({"summary": "done"}),
        );
        // Messages after complete_focus: [system prompt, knowledge block] at minimum
        // Checkpoint + bracketed messages must be gone
        assert!(
            agent.msg.messages.len() < before_len + 3,
            "bracketed messages must be truncated after complete_focus"
        );
    }

    /// Regression test for #3476: when `complete_focus` is called in a batch with other
    /// tools, the current turn's assistant `tool_calls` message must be preserved after
    /// truncation so the subsequent tool results have a valid parent.
    #[test]
    fn complete_focus_in_batch_preserves_current_turn_assistant_message() {
        let mut agent = make_agent();
        call_focus_tool(
            &mut agent,
            "start_focus",
            &serde_json::json!({"scope": "test"}),
        );
        // Simulate a mixed batch: push a bracketed message inside the focus window...
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::User, "some work"));
        // ...then simulate the agent pushing the current-turn assistant message
        // (containing ToolUse parts for [read, complete_focus]) before preprocess runs.
        let batch_assistant = Message::from_parts(
            Role::Assistant,
            vec![
                MessagePart::ToolUse {
                    id: "call-1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": "/tmp/x"}),
                },
                MessagePart::ToolUse {
                    id: "call-2".to_string(),
                    name: "complete_focus".to_string(),
                    input: serde_json::json!({"summary": "done"}),
                },
            ],
        );
        agent.push_message(batch_assistant);

        // Now call complete_focus (as preprocess_focus_compress_calls would).
        let result = call_focus_tool(
            &mut agent,
            "complete_focus",
            &serde_json::json!({"summary": "learned stuff"}),
        );
        assert!(
            !result.starts_with("[error]"),
            "complete_focus must not error: {result}"
        );

        // The current-turn assistant message must still be the last assistant message
        // so that the upcoming tool results have a valid parent.
        let last_assistant = agent
            .msg
            .messages
            .iter()
            .rfind(|m| m.role == Role::Assistant);
        assert!(
            last_assistant.is_some(),
            "current-turn assistant message must be preserved after truncation (#3476)"
        );
        let last_assistant = last_assistant.unwrap();
        assert!(
            last_assistant
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. })),
            "preserved assistant message must have ToolUse parts"
        );
    }

    #[test]
    fn min_messages_per_focus_guard_not_enforced_in_tool() {
        // The guard for min_messages_per_focus is advisory (reminder injection path).
        // handle_focus_tool itself does not enforce it — the LLM decides when to call.
        let mut agent = make_agent();
        agent.services.focus.config.min_messages_per_focus = 100; // very high, but tool doesn't check
        let result = call_focus_tool(
            &mut agent,
            "start_focus",
            &serde_json::json!({"scope": "x"}),
        );
        assert!(
            !result.starts_with("[error]"),
            "tool must not enforce min_messages_per_focus: {result}"
        );
    }

    // --- utility gate integration ---

    #[test]
    fn utility_gate_disabled_by_default_scorer_is_not_enabled() {
        // The default ToolOrchestrator has scoring disabled — no calls are gated.
        let agent = make_agent();
        assert!(
            !agent.tool_orchestrator.utility_scorer.is_enabled(),
            "utility scorer must be disabled by default"
        );
    }

    #[test]
    fn set_utility_config_enables_scorer_on_agent() {
        // set_utility_config wires the scorer into the tool orchestrator (integration path).
        let mut agent = make_agent();
        agent
            .tool_orchestrator
            .set_utility_config(zeph_tools::UtilityScoringConfig {
                enabled: true,
                threshold: 0.5,
                ..zeph_tools::UtilityScoringConfig::default()
            });
        assert!(
            agent.tool_orchestrator.utility_scorer.is_enabled(),
            "scorer must be enabled after set_utility_config"
        );
        assert!(
            (agent.tool_orchestrator.utility_scorer.threshold() - 0.5).abs() < f32::EPSILON,
            "threshold must match config"
        );
    }

    #[test]
    fn clear_utility_state_resets_per_turn_redundancy_tracking() {
        // Verify that clear_utility_state() clears the redundancy state so the
        // next turn treats all calls as fresh (no stale redundancy carry-over).
        use zeph_tools::{ToolCall, UtilityContext};

        let mut agent = make_agent();
        agent
            .tool_orchestrator
            .set_utility_config(zeph_tools::UtilityScoringConfig {
                enabled: true,
                threshold: 0.0,
                ..zeph_tools::UtilityScoringConfig::default()
            });

        let call = ToolCall {
            tool_id: zeph_common::ToolName::new("bash"),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
        };
        let ctx = UtilityContext {
            tool_calls_this_turn: 0,
            tokens_consumed: 0,
            token_budget: 1000,
            user_requested: false,
        };

        // Record the call to create redundancy state.
        agent.tool_orchestrator.utility_scorer.record_call(&call);

        // Before clear: redundancy is 1.0.
        let score_before = agent
            .tool_orchestrator
            .utility_scorer
            .score(&call, &ctx)
            .unwrap();
        assert!(
            (score_before.redundancy - 1.0).abs() < f32::EPSILON,
            "redundancy must be 1.0 before clear"
        );

        // clear_utility_state simulates turn start.
        agent.tool_orchestrator.clear_utility_state();

        // After clear: redundancy is 0.0.
        let score_after = agent
            .tool_orchestrator
            .utility_scorer
            .score(&call, &ctx)
            .unwrap();
        assert!(
            score_after.redundancy.abs() < f32::EPSILON,
            "redundancy must be 0.0 after clear_utility_state"
        );
    }

    // --- explicit_request detection: parts vs content (#2641) ---

    #[test]
    fn explicit_request_detected_from_content_when_parts_empty() {
        // Text-only user messages are created via Message::from_legacy which sets
        // parts: vec![] and stores text only in content.  The fix ensures we read
        // content when parts is empty so the bypass fires correctly.
        use zeph_llm::provider::Message;
        let msg = Message::from_legacy(Role::User, "please call the list_directory tool");
        assert!(msg.parts.is_empty(), "from_legacy must produce empty parts");
        let text = if msg.parts.is_empty() {
            msg.content.clone()
        } else {
            msg.parts
                .iter()
                .filter_map(|p| {
                    if let zeph_llm::provider::MessagePart::Text { text } = p {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(
            zeph_tools::has_explicit_tool_request(&text),
            "explicit_request must be true when content contains tool request"
        );
    }

    #[test]
    fn explicit_request_not_detected_from_empty_parts_without_tool_keyword() {
        use zeph_llm::provider::Message;
        let msg = Message::from_legacy(Role::User, "what is the weather today?");
        let text = if msg.parts.is_empty() {
            msg.content.clone()
        } else {
            msg.parts
                .iter()
                .filter_map(|p| {
                    if let zeph_llm::provider::MessagePart::Text { text } = p {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(
            !zeph_tools::has_explicit_tool_request(&text),
            "explicit_request must be false when content has no tool request"
        );
    }

    // T-HR-3: `record_chat_metrics_and_compact` calls `observe_llm_latency` on the recorder.
    #[tokio::test]
    async fn record_chat_metrics_calls_observe_llm_latency() {
        struct CountingRecorder {
            llm_count: AtomicU64,
        }

        impl HistogramRecorder for CountingRecorder {
            fn observe_llm_latency(&self, _: Duration) {
                self.llm_count.fetch_add(1, Ordering::Relaxed);
            }

            fn observe_turn_duration(&self, _: Duration) {}

            fn observe_tool_execution(&self, _: Duration) {}

            fn observe_bg_task(&self, _: &str, _: Duration) {}
        }

        let recorder = Arc::new(CountingRecorder {
            llm_count: AtomicU64::new(0),
        });

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_histogram_recorder(Some(Arc::clone(&recorder) as Arc<dyn HistogramRecorder>));

        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::System, "system"));

        let start = Instant::now();
        let response = ChatResponse::Text("hello".to_owned());
        agent
            .record_chat_metrics_and_compact(start, &response)
            .await
            .unwrap();

        assert_eq!(
            recorder.llm_count.load(Ordering::Relaxed),
            1,
            "record_chat_metrics_and_compact must call observe_llm_latency once"
        );
    }

    // --- LSP hover injection path (#3595) ---

    fn make_agent_with_lsp_note(note: &'static str) -> Agent<MockChannel> {
        use std::sync::Arc;
        let mut agent = Agent::new(
            mock_provider(vec![String::new()]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        let enforcer = zeph_mcp::PolicyEnforcer::new(vec![]);
        let manager = Arc::new(zeph_mcp::McpManager::new(vec![], vec![], enforcer));
        let mut lsp_runner = crate::lsp_hooks::LspHookRunner::new(
            manager,
            crate::lsp_hooks::LspConfig {
                enabled: true,
                token_budget: 500,
                ..crate::lsp_hooks::LspConfig::default()
            },
        );
        lsp_runner.push_note("hover", note, 5);
        agent.services.session.lsp_hooks = Some(lsp_runner);
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::System, "system"));
        agent
    }

    /// Regression test for #3595: LSP notes queued in `lsp_hooks.pending_notes` must be
    /// injected as a `Role::System` message into `self.msg.messages` inside
    /// `call_chat_with_tools`, before the LLM provider is called.
    ///
    /// The old guard (`last_msg_has_tool_results`) was evaluated at the top of
    /// `process_single_native_turn` on the *next* iteration, when tool results had
    /// already been committed to history, so it always fired and prevented injection.
    /// The fix moves injection unconditionally into `call_chat_with_tools`.
    #[tokio::test]
    async fn lsp_notes_injected_before_llm_call_in_call_chat_with_tools() {
        let mut agent = make_agent_with_lsp_note("fn foo() -> u32");

        let _ = agent.call_chat_with_tools(&[]).await;

        let lsp_msg = agent
            .msg
            .messages
            .iter()
            .find(|m| m.role == Role::System && m.content.starts_with("[lsp "));
        assert!(
            lsp_msg.is_some(),
            "call_chat_with_tools must inject a [lsp hover] System message before the LLM call"
        );
        assert!(
            lsp_msg.unwrap().content.contains("fn foo() -> u32"),
            "injected LSP message must contain the queued note content"
        );
    }

    /// On a retry attempt the note queue is already empty (drained on the first call),
    /// so `call_chat_with_tools` must remove the stale LSP message and not re-inject.
    /// This verifies that notes never accumulate across retry iterations.
    #[tokio::test]
    async fn lsp_notes_not_duplicated_on_retry() {
        use zeph_llm::LlmError;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;

        // First call → ContextLengthExceeded, second call → success.
        let provider = AnyProvider::Mock(
            MockProvider::with_responses(vec![String::new()])
                .with_errors(vec![LlmError::ContextLengthExceeded]),
        );
        let enforcer = zeph_mcp::PolicyEnforcer::new(vec![]);
        let manager = Arc::new(zeph_mcp::McpManager::new(vec![], vec![], enforcer));
        let mut lsp_runner = crate::lsp_hooks::LspHookRunner::new(
            manager,
            crate::lsp_hooks::LspConfig {
                enabled: true,
                token_budget: 500,
                ..crate::lsp_hooks::LspConfig::default()
            },
        );
        lsp_runner.push_note("hover", "fn bar() -> bool", 5);

        let mut agent = Agent::new(
            provider,
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.services.session.lsp_hooks = Some(lsp_runner);
        agent
            .msg
            .messages
            .push(Message::from_legacy(Role::System, "system"));
        agent.context_manager.budget = Some(crate::context::ContextBudget::new(200_000, 0.20));

        let _ = agent.call_chat_with_tools_retry(&[], 2).await;

        let lsp_count = agent
            .msg
            .messages
            .iter()
            .filter(|m| m.role == Role::System && m.content.starts_with("[lsp "))
            .count();
        assert_eq!(
            lsp_count, 0,
            "after retry the stale LSP message must be removed and not re-injected \
            (queue was drained on first attempt)"
        );
    }

    // ── commit_speculative_tier unit tests (issues #3652, #3653) ─────────────────────────────

    use zeph_common::ToolName;
    use zeph_config::tools::{SpeculationMode, SpeculativeConfig};
    use zeph_llm::provider::ToolUseRequest;
    use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};

    use crate::agent::speculative::SpeculationEngine;
    use crate::agent::speculative::prediction::{Prediction, PredictionSource};

    struct AlwaysOkSpecExec;
    impl ToolExecutor for AlwaysOkSpecExec {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "speculative-ok".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }

        fn is_tool_speculatable(&self, _: &str) -> bool {
            true
        }
    }

    struct AlwaysErrSpecExec;
    impl ToolExecutor for AlwaysErrSpecExec {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(&self, _: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
            Err(ToolError::Execution(std::io::Error::other(
                "simulated error",
            )))
        }

        fn is_tool_speculatable(&self, _: &str) -> bool {
            true
        }
    }

    fn decoding_engine<E: ToolExecutor + 'static>(exec: E) -> Arc<SpeculationEngine> {
        Arc::new(SpeculationEngine::new(
            Arc::new(exec),
            SpeculativeConfig {
                mode: SpeculationMode::Decoding,
                ..Default::default()
            },
        ))
    }

    fn test_tool_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: ToolName::new(tool_id),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
        }
    }

    fn test_tool_use_request(name: &str) -> ToolUseRequest {
        ToolUseRequest {
            id: format!("id-{name}"),
            name: ToolName::new(name),
            input: serde_json::Value::Object(serde_json::Map::new()),
        }
    }

    fn test_prediction(tool_id: &str) -> Prediction {
        Prediction {
            tool_id: ToolName::new(tool_id),
            args: serde_json::Map::new(),
            confidence: 0.9,
            source: PredictionSource::StreamPartial,
        }
    }

    /// `engine = None` → returns empty map immediately (zero-cost fast path).
    #[tokio::test]
    async fn commit_speculative_tier_no_engine_returns_empty() {
        let mut agent = make_agent();
        let calls = [test_tool_call("echo")];
        let tool_calls = [test_tool_use_request("echo")];
        let tool_call_ids = ["id-0".to_string()];
        let mut tool_started_ats = [Instant::now()];
        let before = tool_started_ats[0];

        let commits = agent
            .commit_speculative_tier(
                &[0],
                &calls,
                &tool_calls,
                &tool_call_ids,
                &mut tool_started_ats,
                None,
            )
            .await
            .expect("commit_speculative_tier must not fail with no engine");

        assert!(commits.is_empty(), "no engine → empty commit map");
        assert_eq!(
            tool_started_ats[0], before,
            "tool_started_ats must not be modified when engine is None"
        );
    }

    /// `try_commit` returns `None` for all calls (cache miss) → empty commit map.
    #[tokio::test]
    async fn commit_speculative_tier_cache_miss_returns_empty() {
        let engine = decoding_engine(AlwaysOkSpecExec);
        let mut agent = make_agent();
        let calls = [test_tool_call("echo")];
        let tool_calls = [test_tool_use_request("echo")];
        let tool_call_ids = ["id-0".to_string()];
        let mut tool_started_ats = [Instant::now()];

        // Nothing dispatched into the engine — every try_commit will be a miss.
        let commits = agent
            .commit_speculative_tier(
                &[0],
                &calls,
                &tool_calls,
                &tool_call_ids,
                &mut tool_started_ats,
                Some(&engine),
            )
            .await
            .expect("commit_speculative_tier must not fail on cache miss");

        assert!(commits.is_empty(), "cache miss → empty commit map");
    }

    /// `try_commit` returns `Ok(result)` → index in map, `tool_started_ats` stamped,
    /// `ToolStartEvent { speculative: true }` emitted.
    #[tokio::test]
    async fn commit_speculative_tier_ok_result_stamps_and_emits_event() {
        let engine = decoding_engine(AlwaysOkSpecExec);
        let pred = test_prediction("echo");
        engine.try_dispatch(&pred, zeph_common::SkillTrustLevel::Trusted);

        // Let the speculative task complete.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut agent = make_agent();
        let calls = [test_tool_call("echo")];
        let tool_calls = [test_tool_use_request("echo")];
        let tool_call_ids = ["id-0".to_string()];
        let before = Instant::now();
        let mut tool_started_ats = [before];

        let commits = agent
            .commit_speculative_tier(
                &[0],
                &calls,
                &tool_calls,
                &tool_call_ids,
                &mut tool_started_ats,
                Some(&engine),
            )
            .await
            .expect("commit_speculative_tier must not fail on cache hit");

        assert!(
            commits.contains_key(&0),
            "committed index must be in the map"
        );
        assert!(
            commits[&0].is_ok(),
            "AlwaysOkSpecExec must produce Ok result"
        );
        assert!(
            tool_started_ats[0] >= before,
            "tool_started_ats[idx] must be stamped at or after before"
        );

        let starts = agent.channel.tool_starts.lock().unwrap();
        assert_eq!(
            starts.len(),
            1,
            "exactly one ToolStartEvent must be emitted"
        );
        assert!(
            starts[0].speculative,
            "ToolStartEvent.speculative must be true for committed speculative call"
        );
        assert_eq!(
            starts[0].tool_name.as_str(),
            "echo",
            "ToolStartEvent.tool_name must match the tool"
        );
    }

    /// `try_commit` returns `Err(_)` → index still in map with `Err`, `tracing::warn` fires.
    #[tokio::test]
    async fn commit_speculative_tier_err_result_still_in_map() {
        let engine = decoding_engine(AlwaysErrSpecExec);
        let pred = test_prediction("echo");
        engine.try_dispatch(&pred, zeph_common::SkillTrustLevel::Trusted);

        // Let the speculative task complete.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut agent = make_agent();
        let calls = [test_tool_call("echo")];
        let tool_calls = [test_tool_use_request("echo")];
        let tool_call_ids = ["id-0".to_string()];
        let mut tool_started_ats = [Instant::now()];

        let commits = agent
            .commit_speculative_tier(
                &[0],
                &calls,
                &tool_calls,
                &tool_call_ids,
                &mut tool_started_ats,
                Some(&engine),
            )
            .await
            .expect("commit_speculative_tier must not fail when committed result is Err");

        assert!(
            commits.contains_key(&0),
            "even an Err result must be in the commit map"
        );
        assert!(
            commits[&0].is_err(),
            "AlwaysErrSpecExec must produce Err result"
        );
    }
}
