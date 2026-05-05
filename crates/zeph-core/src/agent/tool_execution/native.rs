// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures::FutureExt as _;
use zeph_common::text::estimate_tokens;
use zeph_llm::provider::{
    ChatResponse, LlmProvider, Message, MessageMetadata, MessagePart, Role, ThinkingBlock,
    ToolDefinition,
};

use super::{
    AnomalyOutcome, retry_backoff_ms, strip_tafc_fields, tool_args_hash,
    tool_def_to_definition_with_tafc,
};
use crate::agent::Agent;
use crate::channel::{Channel, StopHint, ToolOutputEvent, ToolStartEvent};
use crate::overflow_tools::OverflowToolExecutor;
use tracing::Instrument;
use zeph_llm::provider::MAX_TOKENS_TRUNCATION_MARKER;
use zeph_sanitizer::{ContentSource, ContentSourceKind};
use zeph_skills::evolution::FailureKind;
use zeph_tools::ExecutionContext;
use zeph_tools::executor::ToolCall;

type ToolExecFut = futures::future::BoxFuture<
    'static,
    Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
>;

/// Output produced by the tier execution loop (Phase 1).
///
/// `None` signals that the loop was cancelled by the user; `Some` carries the
/// collected tool results and deferred data that the results phase needs.
type TierLoopOutput = Option<TierLoopData>;

struct TierLoopData {
    tool_results: Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>>,
    pending_focus_checkpoint: Option<zeph_llm::provider::Message>,
    pending_system_hints: Vec<String>,
}

/// Per-call computed context produced by `Agent::prepare_tool_dispatch`.
///
/// Bundles the gating results (pre-exec block, utility action, repeat/quota/cache checks)
/// alongside the canonical call list so the tier execution loop has a single, coherent view.
struct ToolDispatchContext {
    calls: Vec<ToolCall>,
    tool_call_ids: Vec<String>,
    tool_started_ats: Vec<std::time::Instant>,
    pre_exec_blocked: Vec<bool>,
    utility_actions: Vec<zeph_tools::UtilityAction>,
    quota_blocked: bool,
    args_hashes: Vec<u64>,
    repeat_blocked: Vec<bool>,
    cache_hits: Vec<Option<zeph_tools::ToolOutput>>,
}

/// Output of `Agent::classify_tool_result`: all fields derived from the raw `ToolResult`.
///
/// Returned by value so `process_one_tool_result` can unpack it without borrow issues.
struct ToolResultClassification {
    output: String,
    is_error: bool,
    diff: Option<zeph_tools::DiffData>,
    inline_stats: Option<String>,
    kept_lines: Option<Vec<usize>>,
    locations: Option<Vec<String>>,
    anomaly_outcome: AnomalyOutcome,
    is_quality_failure: bool,
    tool_err_category: Option<zeph_tools::error_taxonomy::ToolErrorCategory>,
}

impl<C: Channel> Agent<C> {
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

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "agent.process_response", skip_all)
    )]
    pub(crate) async fn process_response(&mut self) -> Result<(), crate::agent::error::AgentError> {
        self.services.security.flagged_urls.clear();
        self.process_response_native_tools().await
    }

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

        let llm_span = tracing::info_span!("llm_call", model = %self.runtime.config.model_name);
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

    async fn call_chat_with_tools(
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

        let llm_span = tracing::info_span!("llm_call", model = %self.runtime.config.model_name);
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
        let result = if let Ok(result) = timeout_result {
            result?
        } else {
            self.channel
                .send("LLM request timed out. Please try again.")
                .await?;
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

    /// Prepare and dump the request debug payload, returning the dump ID for the response dump.
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

    /// Run `RuntimeLayer::before_chat` hooks. Returns `Ok(Some(sc))` when a layer short-circuits
    /// the LLM call, `Ok(None)` when all hooks pass through.
    async fn run_before_chat_layers(
        &self,
        tool_defs: &[ToolDefinition],
    ) -> Result<Option<ChatResponse>, crate::agent::error::AgentError> {
        if self.runtime.config.layers.is_empty() {
            return Ok(None);
        }
        let conv_id_str = self
            .services
            .memory
            .persistence
            .conversation_id
            .map(|id| id.0.to_string());
        let ctx = crate::runtime_layer::LayerContext {
            conversation_id: conv_id_str.as_deref(),
            turn_number: u32::try_from(self.services.sidequest.turn_counter).unwrap_or(u32::MAX),
        };
        for layer in &self.runtime.config.layers {
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
        Ok(None)
    }

    /// Run `RuntimeLayer::after_chat` hooks. Panics in hooks are logged and swallowed.
    async fn run_after_chat_layers(&self, result: &ChatResponse) {
        if self.runtime.config.layers.is_empty() {
            return;
        }
        let conv_id_str = self
            .services
            .memory
            .persistence
            .conversation_id
            .map(|id| id.0.to_string());
        let ctx = crate::runtime_layer::LayerContext {
            conversation_id: conv_id_str.as_deref(),
            turn_number: u32::try_from(self.services.sidequest.turn_counter).unwrap_or(u32::MAX),
        };
        for layer in &self.runtime.config.layers {
            let hook_result = std::panic::AssertUnwindSafe(layer.after_chat(&ctx, result))
                .catch_unwind()
                .await;
            if hook_result.is_err() {
                tracing::warn!("RuntimeLayer::after_chat panicked, continuing");
            }
        }
    }

    /// Close the CR-01 LLM trace span after the chat call completes.
    fn record_llm_trace_span_close(
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

    async fn record_chat_metrics_and_compact(
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

    /// Handle phases 2a / 2 / 3 of the tool dispatch cycle after the tier join completes.
    ///
    /// - **Phase 2a**: Interactive confirmation prompts for `ConfirmationRequired` results.
    /// - **Phase 2**: Sequential retry with exponential backoff for transient errors on retryable
    ///   executors. Shell/non-idempotent tools keep their error result unchanged.
    /// - **Phase 3**: Parameter-reformat placeholder path (future LLM-based reformat).
    ///
    /// Each phase checks the cancellation token and returns `Ok(())` on early cancel, persisting
    /// tombstone `ToolResult` entries to satisfy the `TOOL_RESULT_GUARANTEE` invariant.
    /// Phases 2a / 2 / 3 of the tool dispatch cycle (confirmation, retry, reformat).
    async fn run_post_dispatch_phases(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        max_retries: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::agent::error::AgentError> {
        self.handle_confirmation_phase(tool_calls, calls, tool_results, cancel)
            .await?;
        self.handle_retry_phase(tool_calls, calls, tool_results, max_retries, cancel)
            .await?;
        self.handle_reformat_phase(tool_calls, tool_results, cancel)
            .await?;
        Ok(())
    }

    /// Phase 2a: resolve `ConfirmationRequired` results via interactive channel prompt.
    async fn handle_confirmation_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::agent::error::AgentError> {
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
                        // ConfirmationRequired here indicates a misconfigured executor stack.
                        self.tool_executor
                            .execute_tool_call_confirmed_erased(&calls[idx])
                            .await
                    } else {
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
                    && let Some(ref d) = self.runtime.debug.debug_dumper
                {
                    d.dump_tool_error(tool_calls[idx].name.as_str(), e);
                }
                tool_results[idx] = result;
            }
        }
        Ok(())
    }

    /// Phase 2: sequential retry with exponential backoff for transient errors on retryable tools.
    async fn handle_retry_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        max_retries: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::agent::error::AgentError> {
        if max_retries == 0 {
            return Ok(());
        }
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
            if !self
                .tool_executor
                .is_tool_retryable_erased(tc.name.as_str())
            {
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
                        if max_retry_duration_secs > 0 && elapsed_secs >= max_retry_duration_secs {
                            tracing::warn!(
                                tool = %tc.name, elapsed_secs, max_retry_duration_secs,
                                "tool retry budget exceeded, aborting retries"
                            );
                            break exec_result;
                        }
                        attempt += 1;
                        let delay_ms = retry_backoff_ms(attempt - 1, retry_base_ms, retry_max_ms);
                        tracing::warn!(
                            tool = %tc.name, attempt, delay_ms, error = %e,
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
        Ok(())
    }

    /// Phase 3: parameter-reformat placeholder path (future LLM-based reformat).
    ///
    /// Currently logs and skips; original error is propagated unchanged to the LLM.
    async fn handle_reformat_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::agent::error::AgentError> {
        if self
            .tool_orchestrator
            .parameter_reformat_provider
            .is_empty()
        {
            return Ok(());
        }
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
            tracing::warn!(
                tool = %tc.name,
                "parameter error detected; parameter reformat path is reserved for future \
                 LLM-based reformat implementation"
            );
            // Budget check: a newly created instant always has ~0 elapsed, so this guard
            // is effectively a no-op today. Kept for structural parity with the planned
            // LLM-based reformat implementation that will run actual work here.
            let reformat_start = std::time::Instant::now();
            if budget_secs > 0 && reformat_start.elapsed().as_secs() >= budget_secs {
                tracing::warn!(tool = %tc.name, "parameter reformat budget exhausted, skipping");
                continue;
            }
            let _ = self
                .channel
                .send_status(&format!(
                    "Reformat for {} pending provider integration…",
                    tc.name
                ))
                .await;
            let _ = self.channel.send_status("").await;
        }
        Ok(())
    }

    /// Run all registered pre-execution verifiers against each tool call.
    ///
    /// Returns a `Vec<bool>` where `true` at index `i` means call `i` was blocked.
    /// Blocking fires an audit log entry via the supervisor (fire-and-forget).
    fn run_pre_execution_verifiers(&mut self, calls: &[ToolCall]) -> Vec<bool> {
        let mut pre_exec_blocked = vec![false; calls.len()];
        if self.tool_orchestrator.pre_execution_verifiers.is_empty() {
            return pre_exec_blocked;
        }
        for (idx, call) in calls.iter().enumerate() {
            let args_value = serde_json::Value::Object(call.params.clone());
            for verifier in &self.tool_orchestrator.pre_execution_verifiers {
                match verifier.verify(call.tool_id.as_str(), &args_value) {
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
                            zeph_common::SecurityEventCategory::PreExecutionBlock,
                            call.tool_id.as_str(),
                            format!("{}: {}", verifier.name(), reason),
                        );
                        if let Some(ref logger) = self.tool_orchestrator.audit_logger {
                            let args_json = serde_json::to_string(&args_value).unwrap_or_default();
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
                                adversarial_policy_decision: None,
                                exit_code: None,
                                truncated: false,
                                caller_id: call.caller_id.clone(),
                                policy_match: None,
                                correlation_id: None,
                                vigil_risk: None,
                                execution_env: None,
                                resolved_cwd: None,
                                scope_at_definition: None,
                                scope_at_dispatch: None,
                            };
                            let logger = std::sync::Arc::clone(logger);
                            self.runtime.lifecycle.supervisor.spawn(
                                crate::agent::agent_supervisor::TaskClass::Telemetry,
                                "audit-log",
                                async move { logger.log(&entry).await },
                            );
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
                            zeph_common::SecurityEventCategory::PreExecutionWarn,
                            call.tool_id.as_str(),
                            format!("{}: {}", verifier.name(), message),
                        );
                    }
                }
            }
        }
        pre_exec_blocked
    }

    /// Score each tool call with the utility gate and return a recommended action per call.
    ///
    /// Pre-exec-blocked calls are returned as `ToolCall` to avoid double-counting. Exempt tools
    /// bypass scoring entirely. For all others, the scorer recommends an action based on context
    /// (token budget, call history, explicit user request).
    fn compute_utility_actions(
        &mut self,
        calls: &[ToolCall],
        pre_exec_blocked: &[bool],
    ) -> Vec<zeph_tools::UtilityAction> {
        #[allow(clippy::cast_possible_truncation)]
        let tokens_consumed =
            usize::try_from(self.runtime.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
        // token_budget = 0 signals "unknown" to UtilityContext — cost component is zeroed.
        let token_budget: usize = 0;
        let tool_calls_this_turn = self.tool_orchestrator.recent_tool_calls.len();
        // Detect explicit tool request from the last user message text only.
        // We only read MessagePart::Text parts so tool outputs/thinking blocks are excluded.
        let explicit_request = self
            .msg
            .messages
            .iter()
            .rfind(|m| m.role == zeph_llm::provider::Role::User)
            .is_some_and(|m| {
                let text = if m.parts.is_empty() {
                    m.content.clone()
                } else {
                    m.parts
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
                zeph_tools::has_explicit_tool_request(&text)
            });
        calls
            .iter()
            .enumerate()
            .map(|(idx, call)| {
                if pre_exec_blocked[idx] {
                    return zeph_tools::UtilityAction::ToolCall;
                }
                if self
                    .tool_orchestrator
                    .utility_scorer
                    .is_exempt(call.tool_id.as_str())
                {
                    return zeph_tools::UtilityAction::ToolCall;
                }
                let ctx = zeph_tools::UtilityContext {
                    tool_calls_this_turn: tool_calls_this_turn + idx,
                    tokens_consumed,
                    token_budget,
                    user_requested: explicit_request,
                };
                let score = self.tool_orchestrator.utility_scorer.score(call, &ctx);
                let action = self
                    .tool_orchestrator
                    .utility_scorer
                    .recommend_action(score.as_ref(), &ctx);
                tracing::debug!(
                    tool = %call.tool_id,
                    score = ?score.as_ref().map(|s| s.total),
                    threshold = self.tool_orchestrator.utility_scorer.threshold(),
                    action = ?action,
                    "utility gate: action recommended"
                );
                if action != zeph_tools::UtilityAction::ToolCall {
                    tracing::info!(
                        tool = %call.tool_id,
                        action = ?action,
                        "utility gate: non-execute action"
                    );
                }
                // Record call regardless so subsequent calls in this batch see it as prior.
                self.tool_orchestrator.utility_scorer.record_call(call);
                action
            })
            .collect()
    }

    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "agent.tool_loop", skip_all)
    )]
    pub(super) async fn handle_native_tool_calls(
        &mut self,
        text: Option<&str>,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<(), crate::agent::error::AgentError> {
        let t_tool_exec = std::time::Instant::now();
        tracing::debug!("turn timing: tool_exec start");
        // Scan for image-exfiltration in accompanying text, send to channel, persist
        // the assistant ToolUse message.
        self.push_assistant_tool_use_message(text, tool_calls)
            .await?;

        // Build calls, assign IDs, run exfiltration guard, gate checks (pre-exec/utility/
        // quota/repeat/cache), and inject skill env. Extracted to keep this function under
        // the clippy line limit.
        let ToolDispatchContext {
            calls,
            tool_call_ids,
            mut tool_started_ats,
            pre_exec_blocked,
            utility_actions,
            quota_blocked,
            args_hashes,
            repeat_blocked,
            cache_hits,
        } = self.prepare_tool_dispatch(tool_calls);

        let max_retries = self.tool_orchestrator.max_tool_retries;
        // Clamp to 1 to prevent Semaphore(0) deadlock when config is set to 0.
        let max_parallel = self.runtime.config.timeouts.max_parallel_tools.max(1);
        let cancel = self.runtime.lifecycle.cancel_token.clone();

        // Causal IPI pre-probe: record behavioral baseline before tool batch dispatch.
        let causal_pre_response = self.run_causal_pre_probe().await;

        // Phase 1: Tiered parallel execution bounded by a shared semaphore.
        // Extracted to run_tier_execution_loop to satisfy the line-count limit.
        // Returns None when the user cancelled (caller must return Ok(())).
        let tier_data = self
            .run_tier_execution_loop(
                tool_calls,
                &calls,
                &pre_exec_blocked,
                &utility_actions,
                quota_blocked,
                &args_hashes,
                &repeat_blocked,
                &cache_hits,
                max_parallel,
                &cancel,
                &tool_call_ids,
                &mut tool_started_ats,
            )
            .await?;

        // Unpack tier execution output. None means the user cancelled — return early.
        let Some(TierLoopData {
            mut tool_results,
            pending_focus_checkpoint,
            pending_system_hints,
        }) = tier_data
        else {
            return Ok(());
        };

        // Phases 2a / 2 / 3: confirmation, transient retry, parameter reformat.
        // Each phase may return early on cancellation (Ok(())) or propagate channel errors.
        self.run_post_dispatch_phases(tool_calls, &calls, &mut tool_results, max_retries, &cancel)
            .await?;

        // Process results, persist messages, run LSP hooks, fire deferred reflection.
        // Also clears skill env and syncs cache counters after execution.
        // Extracted to process_tool_result_batch to satisfy the line-count limit.
        self.process_tool_result_batch(
            tool_calls,
            &tool_call_ids,
            &tool_started_ats,
            tool_results,
            causal_pre_response,
            pending_focus_checkpoint,
            pending_system_hints,
        )
        .await?;

        let tool_exec_ms = u64::try_from(t_tool_exec.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::debug!(ms = tool_exec_ms, "turn timing: tool_exec done");
        self.runtime.metrics.pending_timings.tool_exec_ms = self
            .runtime
            .metrics
            .pending_timings
            .tool_exec_ms
            .saturating_add(tool_exec_ms);

        Ok(())
    }

    /// Scan the optional text accompanying a `ToolUse` LLM response for injection patterns,
    /// send it to the channel, then persist the assistant `ToolUse` message to history.
    ///
    /// Separated from `handle_native_tool_calls` to keep that function under the line limit.
    async fn push_assistant_tool_use_message(
        &mut self,
        text: Option<&str>,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<(), crate::agent::error::AgentError> {
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
                name: tc.name.to_string(),
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
        if let (Some(id), Some(last)) = (
            self.msg.last_persisted_message_id,
            self.msg.messages.last_mut(),
        ) {
            last.metadata.db_id = Some(id);
        }
        Ok(())
    }

    /// Build the canonical call list from `tool_calls`, assign stable IDs, run the exfiltration
    /// guard, and compute all per-call gate results (pre-exec block, utility action, quota,
    /// repeat detection, cache lookup). Injects active skill secrets before returning.
    ///
    /// Separated from `handle_native_tool_calls` to keep that function under the line limit.
    fn prepare_tool_dispatch(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> ToolDispatchContext {
        let tafc_enabled = self.tool_orchestrator.tafc.enabled;
        // When the orchestration scheduler has set a named execution environment for the
        // current task, inject it into every ToolCall so ShellExecutor::resolve_context
        // uses the right env/cwd without the LLM having to supply it.
        let task_ctx: Option<ExecutionContext> = self
            .services
            .orchestration
            .task_execution_env
            .as_deref()
            .map(|name| ExecutionContext::default().with_name(name));
        let calls: Vec<ToolCall> = tool_calls
            .iter()
            .filter_map(|tc| {
                let mut params: serde_json::Map<String, serde_json::Value> =
                    if let serde_json::Value::Object(map) = &tc.input {
                        map.clone()
                    } else {
                        serde_json::Map::new()
                    };
                if tafc_enabled && strip_tafc_fields(&mut params, tc.name.as_str()).is_err() {
                    // Model produced only think fields — skip this tool call.
                    return None;
                }
                Some(ToolCall {
                    tool_id: tc.name.clone(),
                    params,
                    caller_id: None,
                    context: task_ctx.clone(),
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
        let tool_started_ats: Vec<std::time::Instant> =
            vec![std::time::Instant::now(); tool_calls.len()];

        self.check_exfiltration_urls(tool_calls);

        // Pre-execution verification (TrustBench pattern, issue #1630).
        // Runs after exfiltration guard (flag-only) and before repeat-detection.
        // Block: return synthetic error result for this call without executing.
        // Warn: log + emit security event + continue execution.
        let pre_exec_blocked = self.run_pre_execution_verifiers(&calls);

        // Utility gate: score each call and recommend an action (#2477).
        // user_requested is detected from the last user message only — never from LLM content or
        // tool outputs to prevent prompt-injection bypass (C2 fix).
        let utility_actions = self.compute_utility_actions(&calls, &pre_exec_blocked);

        // Per-session quota check. Counted once per logical dispatch batch (M3: retries do not
        // consume additional quota slots). When exceeded, all calls in this batch are quota-blocked.
        let quota_blocked = if let Some(max) = self.tool_orchestrator.check_quota() {
            tracing::warn!(
                max,
                count = self.tool_orchestrator.session_tool_call_count,
                "tool call quota exceeded for session"
            );
            true
        } else {
            // Increment before the retry loop — each call in this batch counts as one logical call.
            self.tool_orchestrator.session_tool_call_count = self
                .tool_orchestrator
                .session_tool_call_count
                .saturating_add(u32::try_from(calls.len()).unwrap_or(u32::MAX));
            false
        };

        // Build args hashes and check for repeats. Blocked calls get a pre-built error result.
        let args_hashes: Vec<u64> = calls.iter().map(|c| tool_args_hash(&c.params)).collect();
        let repeat_blocked: Vec<bool> = calls
            .iter()
            .zip(args_hashes.iter())
            .map(|(call, &hash)| {
                let blocked = self
                    .tool_orchestrator
                    .is_repeat(call.tool_id.as_str(), hash);
                if blocked {
                    tracing::warn!(
                        tool = %call.tool_id,
                        "[repeat-detect] identical tool call detected, skipping execution"
                    );
                }
                blocked
            })
            .collect();
        // Repeat-detection (CRIT-3): push LLM-initiated calls BEFORE execution.
        // Cache hits are also pushed here (P1 invariant): a cached tool called N times must
        // still trigger repeat-detection to prevent infinite loops if the LLM keeps requesting it.
        for (call, &hash) in calls.iter().zip(args_hashes.iter()) {
            self.tool_orchestrator
                .push_tool_call(call.tool_id.as_str(), hash);
        }

        // Cache lookup: for each non-repeat, cacheable call, check result cache before dispatch.
        // Hits are stored as pre-built results; cache store happens after join_all completes.
        let cache_hits: Vec<Option<zeph_tools::ToolOutput>> = calls
            .iter()
            .zip(args_hashes.iter())
            .zip(repeat_blocked.iter())
            .map(|((call, &hash), &blocked)| {
                if blocked || !zeph_tools::is_cacheable(call.tool_id.as_str()) {
                    return None;
                }
                let key = zeph_tools::CacheKey::new(call.tool_id.as_str(), hash);
                self.tool_orchestrator.result_cache.get(&key)
            })
            .collect();

        // Inject active skill secrets before tool execution.
        self.inject_active_skill_env();

        ToolDispatchContext {
            calls,
            tool_call_ids,
            tool_started_ats,
            pre_exec_blocked,
            utility_actions,
            quota_blocked,
            args_hashes,
            repeat_blocked,
            cache_hits,
        }
    }

    /// Validate tool call arguments against URLs seen in flagged untrusted content.
    ///
    /// This is a flag-only check — suspicious URLs are logged and metered but do not block
    /// execution. Extracted from `prepare_tool_dispatch` to keep that function under the
    /// clippy line limit.
    fn check_exfiltration_urls(&mut self, tool_calls: &[zeph_llm::provider::ToolUseRequest]) {
        for tc in tool_calls {
            let args_json = tc.input.to_string();
            let url_events = self
                .services
                .security
                .exfiltration_guard
                .validate_tool_call(
                    tc.name.as_str(),
                    &args_json,
                    &self.services.security.flagged_urls,
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
                    zeph_common::SecurityEventCategory::ExfiltrationBlock,
                    tc.name.as_str(),
                    format!(
                        "{} suspicious URL(s) flagged in tool args",
                        url_events.len()
                    ),
                );
            }
        }
    }

    /// Run the causal IPI pre-probe to record the behavioral baseline before tool dispatch.
    ///
    /// Returns the probe response paired with the context summary so the post-probe can
    /// compare the two. Returns `None` when the analyzer is not configured or the probe fails.
    async fn run_causal_pre_probe(&mut self) -> Option<(String, String)> {
        let analyzer = self.services.security.causal_analyzer.as_ref()?;
        let context_summary = self.build_causal_context_summary();
        match analyzer.probe(&context_summary).await {
            Ok(resp) => Some((resp, context_summary)),
            Err(e) => {
                tracing::warn!(error = %e, "causal IPI pre-probe failed, skipping analysis");
                None
            }
        }
    }

    /// Execute tool calls in topological tiers (Phase 1).
    ///
    /// Builds a dependency DAG, partitions into tiers, and runs each tier with bounded
    /// concurrency via a shared semaphore. MCP elicitation events are drained during the
    /// join to prevent deadlocks.
    ///
    /// Returns `None` when the user cancelled (the caller must return `Ok(())`), or
    /// `Some(TierLoopData)` with the collected results on success.
    #[allow(clippy::too_many_arguments)]
    async fn run_tier_execution_loop(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        pre_exec_blocked: &[bool],
        utility_actions: &[zeph_tools::UtilityAction],
        quota_blocked: bool,
        args_hashes: &[u64],
        repeat_blocked: &[bool],
        cache_hits: &[Option<zeph_tools::ToolOutput>],
        max_parallel: usize,
        cancel: &tokio_util::sync::CancellationToken,
        tool_call_ids: &[String],
        tool_started_ats: &mut [std::time::Instant],
    ) -> Result<TierLoopOutput, crate::agent::error::AgentError> {
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
        let pending_focus_checkpoint = self
            .preprocess_focus_compress_calls(tool_calls, &mut tool_results)
            .await;

        // Track which indices have a failed/ConfirmationRequired prerequisite so that
        // dependent calls in later tiers receive a synthetic error instead of executing.
        // IMP-02: ConfirmationRequired is treated as a failure for dependency propagation —
        // a dependent tool must not proceed when its prerequisite is awaiting user approval.
        let mut failed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Utility gate hints (Retrieve/Verify) are deferred so they are pushed after
        // User(tool_results), maintaining valid OpenAI message ordering (#2615).
        let mut pending_system_hints: Vec<String> = Vec::new();

        for (tier_idx, tier) in tiers.into_iter().enumerate() {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("tool execution cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls).await;
                return Ok(None);
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

            // Stamp execution start time and send ToolStartEvent per-tier (§3.7).
            self.stamp_and_send_tier_start(
                &tier.indices,
                tool_calls,
                tool_call_ids,
                tool_started_ats,
            )
            .await?;

            // Build futures for this tier, applying all gate checks per call.
            let tier_futs = self
                .build_tier_call_futures(
                    tool_calls,
                    calls,
                    &tier.indices,
                    &dag,
                    &failed_ids,
                    quota_blocked,
                    pre_exec_blocked,
                    utility_actions,
                    repeat_blocked,
                    cache_hits,
                    &semaphore,
                    &mut pending_system_hints,
                )
                .await?;

            // Execute futures concurrently with cancellation and MCP elicitation drain.
            let (indices, futs): (Vec<usize>, Vec<ToolExecFut>) = tier_futs.into_iter().unzip();
            let Some(tier_results) = self.execute_tier_join(futs, cancel, tool_calls).await? else {
                return Ok(None);
            };

            // Store results, update dependency graph, and run after_tool hooks.
            self.apply_tier_results(
                indices,
                tier_results,
                tool_calls,
                calls,
                cache_hits,
                args_hashes,
                &mut failed_ids,
                &mut tool_results,
            )
            .await;

            if tier_count > 1 {
                let _ = self.channel.send_status("").await;
            }
        }

        // Pad with empty results if needed (defensive; should not happen).
        while tool_results.len() < tool_calls.len() {
            tool_results.push(Ok(None));
        }

        Ok(Some(TierLoopData {
            tool_results,
            pending_focus_checkpoint,
            pending_system_hints,
        }))
    }

    /// Pre-process focus and `compress_context` tool calls before the tier loop.
    ///
    /// These tools require `&mut self` and cannot run inside spawned tier futures.
    /// Results are pre-populated in `tool_results` so the tier loop skips them.
    /// Returns the pending focus checkpoint message, if one was produced.
    async fn preprocess_focus_compress_calls(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
    ) -> Option<zeph_llm::provider::Message> {
        let mut pending_focus_checkpoint: Option<zeph_llm::provider::Message> = None;
        for (idx, tc) in tool_calls.iter().enumerate() {
            let is_focus_tool = self.services.focus.config.enabled
                && (tc.name == "start_focus" || tc.name == "complete_focus");
            let is_compress = tc.name == "compress_context";
            if is_focus_tool || is_compress {
                let result = if is_compress {
                    self.handle_compress_context().await
                } else {
                    let (text, maybe_checkpoint) =
                        self.handle_focus_tool(tc.name.as_str(), &tc.input);
                    if let Some(cp) = maybe_checkpoint {
                        pending_focus_checkpoint = Some(cp);
                    }
                    text
                };
                tool_results[idx] = Ok(Some(skipped_output(tc.name.clone(), result)));
            }
        }
        pending_focus_checkpoint
    }

    /// Mark the execution start time for each call in the tier and send `ToolStartEvent`s.
    ///
    /// Called once per tier before futures are dispatched so that TUI timing is accurate.
    async fn stamp_and_send_tier_start(
        &mut self,
        tier_indices: &[usize],
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_call_ids: &[String],
        tool_started_ats: &mut [std::time::Instant],
    ) -> Result<(), crate::agent::error::AgentError> {
        let tier_start = std::time::Instant::now();
        for &idx in tier_indices {
            tool_started_ats[idx] = tier_start;
        }
        for &idx in tier_indices {
            let tc = &tool_calls[idx];
            self.channel
                .send_tool_start(ToolStartEvent {
                    tool_name: tc.name.clone(),
                    tool_call_id: tool_call_ids[idx].clone(),
                    params: Some(tc.input.clone()),
                    parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                    started_at: std::time::Instant::now(),
                    speculative: false,
                    sandbox_profile: None,
                })
                .await?;
        }
        Ok(())
    }

    /// Apply the utility gate decision for a single tool call.
    ///
    /// Returns `Some((idx, fut))` when the gate fires and the call should be short-circuited
    /// (the caller must push the future and `continue`), or `None` to proceed normally.
    /// Mutates `pending_system_hints` for Retrieve/Verify actions.
    async fn handle_utility_gate(
        &mut self,
        idx: usize,
        tc: &zeph_llm::provider::ToolUseRequest,
        utility_actions: &[zeph_tools::UtilityAction],
        pending_system_hints: &mut Vec<String>,
    ) -> Result<Option<(usize, ToolExecFut)>, crate::agent::error::AgentError> {
        match utility_actions[idx] {
            zeph_tools::UtilityAction::ToolCall => Ok(None),
            zeph_tools::UtilityAction::Respond => {
                let _ = self
                    .channel
                    .send_status(&format!("Utility action: Respond ({})", tc.name))
                    .await;
                Ok(Some(ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[skipped] Tool call to {} skipped — utility policy recommends a \
                             direct response without further tool use.",
                            tc.name
                        ),
                    ),
                )))
            }
            zeph_tools::UtilityAction::Retrieve => {
                let _ = self
                    .channel
                    .send_status(&format!("Utility action: Retrieve ({})", tc.name))
                    .await;
                // Inject a system message directing the LLM to retrieve context first (#2620).
                pending_system_hints.push(format!(
                    "[utility:retrieve] Before executing the '{}' tool, retrieve \
                     relevant context via memory_search or a related lookup to ensure \
                     the call is well-targeted. After retrieving context, you MUST call \
                     the '{}' tool again with the same arguments.",
                    tc.name, tc.name
                ));
                Ok(Some(ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[skipped] Tool call to {} skipped — utility policy recommends \
                             retrieving additional context first.",
                            tc.name
                        ),
                    ),
                )))
            }
            zeph_tools::UtilityAction::Verify => {
                let _ = self
                    .channel
                    .send_status(&format!("Utility action: Verify ({})", tc.name))
                    .await;
                pending_system_hints.push(format!(
                    "[utility:verify] Before executing the '{}' tool again, verify \
                     the result of the previous tool call to confirm it is correct \
                     and that further tool use is necessary.",
                    tc.name
                ));
                Ok(Some(ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[skipped] Tool call to {} skipped — utility policy recommends \
                             verifying the previous result first.",
                            tc.name
                        ),
                    ),
                )))
            }
            zeph_tools::UtilityAction::Stop => {
                let _ = self
                    .channel
                    .send_status(&format!("Utility action: Stop ({})", tc.name))
                    .await;
                let threshold = self.tool_orchestrator.utility_scorer.threshold();
                Ok(Some(ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[stopped] Tool call to {} halted by the utility gate — \
                             budget exhausted or score below threshold {threshold:.2}.",
                            tc.name
                        ),
                    ),
                )))
            }
        }
    }

    /// Run `RuntimeLayer::before_tool` hooks for a single call.
    ///
    /// Returns `Some((idx, fut))` when a hook short-circuits execution (the caller must push and
    /// `continue`), or `None` when all hooks pass and execution should proceed normally.
    /// `PermissionDenied` hooks are fired asynchronously when a layer blocks the call.
    async fn run_before_tool_hooks(
        &mut self,
        idx: usize,
        tc: &zeph_llm::provider::ToolUseRequest,
        call: &ToolCall,
    ) -> Option<(usize, ToolExecFut)> {
        if self.runtime.config.layers.is_empty() {
            return None;
        }
        let conv_id_str = self
            .services
            .memory
            .persistence
            .conversation_id
            .map(|id| id.0.to_string());
        let ctx = crate::runtime_layer::LayerContext {
            conversation_id: conv_id_str.as_deref(),
            turn_number: u32::try_from(self.services.sidequest.turn_counter).unwrap_or(u32::MAX),
        };
        let mut sc_result: crate::runtime_layer::BeforeToolResult = None;
        for layer in &self.runtime.config.layers {
            let hook_result = std::panic::AssertUnwindSafe(layer.before_tool(&ctx, call))
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
        let r = sc_result?;
        // Fire PermissionDenied hooks (fail_open: hook errors are logged, not fatal).
        let pd_hooks = self.services.session.hooks_config.permission_denied.clone();
        if !pd_hooks.is_empty() {
            let _span =
                tracing::info_span!("core.hooks.permission_denied", tool = %tc.name).entered();
            let mut env = std::collections::HashMap::new();
            env.insert("ZEPH_DENIED_TOOL".to_owned(), tc.name.to_string());
            env.insert("ZEPH_DENY_REASON".to_owned(), r.reason.clone());
            let dispatch = self.mcp_dispatch();
            let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                .as_ref()
                .map(|d| d as &dyn zeph_subagent::McpDispatch);
            // TODO: implement retry-on-{"retry":true} stdout signal (#3292)
            if let Err(e) = zeph_subagent::hooks::fire_hooks(&pd_hooks, &env, mcp).await {
                tracing::warn!(error = %e, tool = %tc.name, "PermissionDenied hook failed");
            }
        }
        Some((idx, Box::pin(std::future::ready(r.result))))
    }

    /// Check the static per-call gates in order: dependency failure, quota, pre-exec block,
    /// utility gate action, and repeat detection.
    ///
    /// Returns `Some((idx, fut))` when any gate fires (the caller must push and `continue`),
    /// or `None` when all gates pass and execution should proceed.
    #[allow(clippy::too_many_arguments)]
    async fn check_call_gates(
        &mut self,
        idx: usize,
        tc: &zeph_llm::provider::ToolUseRequest,
        has_failed_dep: bool,
        quota_blocked: bool,
        pre_exec_blocked: &[bool],
        utility_actions: &[zeph_tools::UtilityAction],
        repeat_blocked: &[bool],
        pending_system_hints: &mut Vec<String>,
    ) -> Result<Option<(usize, ToolExecFut)>, crate::agent::error::AgentError> {
        if has_failed_dep {
            return Ok(Some(ready_fut(
                idx,
                skipped_output(
                    tc.name.clone(),
                    "[error] Skipped: a prerequisite tool failed or requires confirmation",
                ),
            )));
        }
        if quota_blocked {
            let max = self
                .tool_orchestrator
                .max_tool_calls_per_session
                .unwrap_or(0);
            return Ok(Some(ready_fut(
                idx,
                skipped_output(
                    tc.name.clone(),
                    format!(
                        "[error] Tool call quota exceeded (session limit: {max} calls). \
                         No further tool calls are allowed this session."
                    ),
                ),
            )));
        }
        if pre_exec_blocked[idx] {
            return Ok(Some(ready_fut(
                idx,
                skipped_output(
                    tc.name.clone(),
                    format!(
                        "[error] Tool call to {} was blocked by pre-execution verifier. \
                         The requested operation is not permitted.",
                        tc.name
                    ),
                ),
            )));
        }
        if let Some(fut) = self
            .handle_utility_gate(idx, tc, utility_actions, pending_system_hints)
            .await?
        {
            return Ok(Some(fut));
        }
        if repeat_blocked[idx] {
            return Ok(Some(ready_fut(
                idx,
                skipped_output(
                    tc.name.clone(),
                    format!(
                        "[error] Repeated identical call to {} detected. \
                         Use different arguments or a different approach.",
                        tc.name
                    ),
                ),
            )));
        }
        Ok(None)
    }

    /// Build the execution futures for one DAG tier, applying all per-call gate checks.
    ///
    /// For each call in `tier_indices`, the function checks (in order): DAG dependency
    /// failure, session quota, pre-exec block, utility gate, repeat detection, result cache,
    /// rate limiter, and `RuntimeLayer::before_tool` hooks. Any gate that fires returns a
    /// pre-resolved synthetic future. Calls that pass all gates get a semaphore-guarded
    /// executor future. Returns `Err` only for channel errors from utility-gate status sends.
    #[allow(clippy::too_many_arguments, clippy::ptr_arg)]
    async fn build_tier_call_futures(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tier_indices: &[usize],
        dag: &super::tool_call_dag::ToolCallDag,
        failed_ids: &std::collections::HashSet<String>,
        quota_blocked: bool,
        pre_exec_blocked: &[bool],
        utility_actions: &[zeph_tools::UtilityAction],
        repeat_blocked: &[bool],
        cache_hits: &[Option<zeph_tools::ToolOutput>],
        semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
        pending_system_hints: &mut Vec<String>,
    ) -> Result<Vec<(usize, ToolExecFut)>, crate::agent::error::AgentError> {
        let tier_tool_names: Vec<&str> = tier_indices
            .iter()
            .map(|&i| tool_calls[i].name.as_str())
            .collect();
        let rate_results = self
            .runtime
            .config
            .rate_limiter
            .check_batch(&tier_tool_names);

        let mut tier_futs: Vec<(usize, ToolExecFut)> = Vec::with_capacity(tier_indices.len());
        for (tier_local_idx, &idx) in tier_indices.iter().enumerate() {
            let tc = &tool_calls[idx];
            let call = &calls[idx];

            // Skip focus tools and compress_context — pre-handled before the tier loop.
            if tc.name == "compress_context"
                || (self.services.focus.config.enabled
                    && (tc.name == "start_focus" || tc.name == "complete_focus"))
            {
                continue;
            }

            // Check static gates: dep failure, quota, pre-exec block, utility gate, repeat.
            let has_failed_dep = dag
                .string_values_for(idx)
                .iter()
                .any(|v| failed_ids.contains(v));
            if let Some(fut) = self
                .check_call_gates(
                    idx,
                    tc,
                    has_failed_dep,
                    quota_blocked,
                    pre_exec_blocked,
                    utility_actions,
                    repeat_blocked,
                    pending_system_hints,
                )
                .await?
            {
                tier_futs.push(fut);
                continue;
            }

            // Cache hit: return pre-computed result without executing the tool.
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
                    zeph_common::SecurityEventCategory::RateLimit,
                    tc.name.as_str(),
                    format!(
                        "{} calls exceeded {}/min",
                        exceeded.category.as_str(),
                        exceeded.limit
                    ),
                );
                tier_futs.push(ready_fut(
                    idx,
                    skipped_output(tc.name.clone(), exceeded.to_error_message()),
                ));
                continue;
            }

            if let Some(fut) = self.run_before_tool_hooks(idx, tc, call).await {
                tier_futs.push(fut);
                continue;
            }

            let sem = std::sync::Arc::clone(semaphore);
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
        Ok(tier_futs)
    }

    /// Poll tier futures concurrently with cancellation and MCP elicitation drain.
    ///
    /// Returns `Ok(None)` when the user cancelled (the tier loop must `return Ok(None)`),
    /// or `Ok(Some(results))` on completion.
    async fn execute_tier_join(
        &mut self,
        futs: Vec<ToolExecFut>,
        cancel: &tokio_util::sync::CancellationToken,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<
        Option<Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>>>,
        crate::agent::error::AgentError,
    > {
        let mut join_fut = std::pin::pin!(futures::future::join_all(futs));
        // Take elicitation_rx out of self so we can hold &mut self for handling.
        let mut elicitation_rx = self.services.mcp.elicitation_rx.take();
        let result = loop {
            tokio::select! {
                results = &mut join_fut => break results,
                () = cancel.cancelled() => {
                    self.services.mcp.elicitation_rx = elicitation_rx;
                    self.tool_executor.set_skill_env(None);
                    tracing::info!("tool execution cancelled by user");
                    self.update_metrics(|m| m.cancellations += 1);
                    self.channel.send("[Cancelled]").await?;
                    self.persist_cancelled_tool_results(tool_calls).await;
                    return Ok(None);
                }
                event = recv_elicitation(&mut elicitation_rx) => {
                    if let Some(ev) = event {
                        self.handle_elicitation_event(ev).await;
                    } else {
                        tracing::debug!("elicitation channel closed during tier exec");
                        elicitation_rx = None;
                    }
                }
            }
        };
        self.services.mcp.elicitation_rx = elicitation_rx;
        Ok(Some(result))
    }

    /// Store tier execution results, update the dependency-failure set, prime the result cache,
    /// update the dependency graph, and run `RuntimeLayer::after_tool` hooks.
    #[allow(clippy::too_many_arguments)]
    async fn apply_tier_results(
        &mut self,
        indices: Vec<usize>,
        tier_results: Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>>,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        cache_hits: &[Option<zeph_tools::ToolOutput>],
        args_hashes: &[u64],
        failed_ids: &mut std::collections::HashSet<String>,
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
    ) {
        for (idx, result) in indices.into_iter().zip(tier_results) {
            // IMP-02: Err(_) covers all error variants including ConfirmationRequired.
            // Ok(Some(out)) with "[error]" prefix covers synthetic/blocked results.
            let is_failed = match &result {
                Err(_) => true,
                Ok(Some(out)) => out.summary.starts_with("[error]"),
                Ok(None) => false,
            };
            if is_failed {
                failed_ids.insert(tool_calls[idx].id.clone());
            }

            // Store successful, non-cached results in the tool result cache.
            if !is_failed
                && cache_hits[idx].is_none()
                && zeph_tools::is_cacheable(tool_calls[idx].name.as_str())
                && let Ok(Some(ref out)) = result
            {
                let key =
                    zeph_tools::CacheKey::new(tool_calls[idx].name.to_string(), args_hashes[idx]);
                self.tool_orchestrator.result_cache.put(key, out.clone());
            }

            // Record successful tool completions for the dependency graph (#2024).
            if !is_failed && self.services.tool_state.dependency_graph.is_some() {
                self.services
                    .tool_state
                    .completed_tool_ids
                    .insert(tool_calls[idx].name.to_string());
            }

            // RuntimeLayer after_tool hooks.
            if !self.runtime.config.layers.is_empty() {
                let conv_id_str = self
                    .services
                    .memory
                    .persistence
                    .conversation_id
                    .map(|id| id.0.to_string());
                let ctx = crate::runtime_layer::LayerContext {
                    conversation_id: conv_id_str.as_deref(),
                    turn_number: u32::try_from(self.services.sidequest.turn_counter)
                        .unwrap_or(u32::MAX),
                };
                for layer in &self.runtime.config.layers {
                    let hook_result =
                        std::panic::AssertUnwindSafe(layer.after_tool(&ctx, &calls[idx], &result))
                            .catch_unwind()
                            .await;
                    if hook_result.is_err() {
                        tracing::warn!("RuntimeLayer::after_tool panicked, continuing");
                    }
                }
            }

            tool_results[idx] = result;
        }
    }

    /// Run the causal IPI post-probe after tool batch execution.
    ///
    /// Collects sanitized snippets from `result_parts`, runs the behavioral post-probe,
    /// and logs a security event if the deviation exceeds the analyzer threshold.
    /// No-op when `causal_pre_response` is `None` or no causal analyzer is configured.
    async fn run_causal_ipi_post_probe(
        &mut self,
        causal_pre_response: Option<(String, String)>,
        result_parts: &[MessagePart],
    ) {
        let Some((pre_response, context_summary)) = causal_pre_response else {
            return;
        };
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
        let Some(ref analyzer) = self.services.security.causal_analyzer else {
            return;
        };
        match analyzer.post_probe(&context_summary, &tool_snippets).await {
            Ok(post_response) => {
                let analysis = analyzer.analyze(&pre_response, &post_response);
                if analysis.is_flagged {
                    let pre_excerpt = &pre_response[..pre_response.floor_char_boundary(100)];
                    let post_excerpt = &post_response[..post_response.floor_char_boundary(100)];
                    tracing::warn!(
                        deviation_score = analysis.deviation_score,
                        threshold = analyzer.threshold(),
                        pre = %pre_excerpt,
                        post = %post_excerpt,
                        "causal IPI: behavioral deviation detected at tool-return boundary"
                    );
                    self.update_metrics(|m| m.causal_ipi_flags += 1);
                    self.push_security_event(
                        zeph_common::SecurityEventCategory::CausalIpiFlag,
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

    /// Process tool results after all tiers have completed.
    ///
    /// Clears skill env, syncs cache counters, processes each result (metrics, channel
    /// display, VIGIL gate, sanitization, skill outcomes), flushes outcomes, runs causal
    /// post-probe, persists the `User{ToolResults}` message, fires deferred self-reflection,
    /// runs LSP hooks, and checks for cwd changes.
    ///
    /// Separated from `handle_native_tool_calls` to keep that function under the line limit.
    #[allow(clippy::too_many_arguments)]
    async fn process_tool_result_batch(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_call_ids: &[String],
        tool_started_ats: &[std::time::Instant],
        mut tool_results: Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>>,
        causal_pre_response: Option<(String, String)>,
        pending_focus_checkpoint: Option<zeph_llm::provider::Message>,
        pending_system_hints: Vec<String>,
    ) -> Result<(), crate::agent::error::AgentError> {
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
        // Accumulate skill outcomes during the tool loop; flushed once after the loop via
        // flush_skill_outcomes to avoid N×M×13 sequential SQLite awaits (#2770).
        let mut pending_outcomes: Vec<crate::agent::learning::PendingSkillOutcome> = Vec::new();
        for idx in 0..tool_calls.len() {
            let tc = &tool_calls[idx];
            let tool_call_id = &tool_call_ids[idx];
            let started_at = &tool_started_ats[idx];
            let tool_result = std::mem::replace(&mut tool_results[idx], Ok(None));
            self.process_one_tool_result(
                tc,
                tool_call_id,
                started_at,
                tool_result,
                &mut result_parts,
                &mut lsp_tool_calls,
                &mut has_any_injection_flags,
                &mut pending_reflection,
                &mut pending_outcomes,
            )
            .await?;
        }

        // Flush all accumulated skill outcomes from the tool batch in a single pass.
        // This replaces the per-tool record_skill_outcomes calls that caused N×M sequential
        // SQLite awaits (#2770).
        self.flush_skill_outcomes(pending_outcomes).await;

        self.run_causal_ipi_post_probe(causal_pre_response, &result_parts)
            .await;

        let user_msg = Message::from_parts(Role::User, result_parts);
        // flagged_urls accumulates across ALL tools in this batch (cross-tool trust boundary).
        // A URL from tool N's output can flag tool M's arguments even if tool M returned clean
        // output. has_any_injection_flags covers pure text injections (no URL); flagged_urls
        // covers URL-based exfiltration. Both are OR-combined for conservative guarding.
        // Individual per-tool granularity would require separate persist_message calls per
        // result, which would change message history structure.
        let tool_results_have_flags =
            has_any_injection_flags || !self.services.security.flagged_urls.is_empty();
        tracing::debug!("tool_batch: calling persist_message for tool results");
        self.persist_message(
            Role::User,
            &user_msg.content,
            &user_msg.parts,
            tool_results_have_flags,
        )
        .await;
        tracing::debug!("tool_batch: persist_message done, pushing message");
        self.push_message(user_msg);
        tracing::debug!("tool_batch: message pushed, starting LSP hooks");
        if let (Some(id), Some(last)) = (
            self.msg.last_persisted_message_id,
            self.msg.messages.last_mut(),
        ) {
            last.metadata.db_id = Some(id);
        }

        // Flush deferred start_focus checkpoint AFTER User(tool_results) so the ordering
        // Assistant→User→System is valid for OpenAI (#3262).
        if let Some(checkpoint) = pending_focus_checkpoint {
            self.push_message(checkpoint);
        }

        // Flush deferred utility gate hints (Retrieve/Verify). Pushed after User(tool_results)
        // so the ordering Assistant→User→System is valid for OpenAI (#2615).
        for hint in pending_system_hints {
            self.push_message(zeph_llm::provider::Message::from_legacy(
                zeph_llm::provider::Role::System,
                &hint,
            ));
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
        // `&mut self.services.session.lsp_hooks` without conflicting borrows.
        //
        // The entire batch is capped at 30s to prevent stalls when many files are
        // modified in one tool batch (#2750). Per the critic review, a single outer
        // timeout is more effective than per-call timeouts because it bounds total
        // blocking time regardless of N.
        if self.services.session.lsp_hooks.is_some() {
            let tc_arc = std::sync::Arc::clone(&self.runtime.metrics.token_counter);
            let sanitizer = self.services.security.sanitizer.clone();
            let _ = self.channel.send_status("Analyzing changes...").await;
            // TODO: cooperative MCP cancellation — dropped futures here may leave
            // in-flight MCP JSON-RPC requests pending until the server-side timeout.
            let lsp_result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                for (name, input, output) in lsp_tool_calls {
                    if let Some(ref mut lsp) = self.services.session.lsp_hooks {
                        lsp.after_tool(&name, &input, &output, &tc_arc, &sanitizer)
                            .await;
                    }
                }
            })
            .await;
            let _ = self.channel.send_status("").await;
            if lsp_result.is_err() {
                tracing::warn!("LSP after_tool batch timed out (30s)");
            }
            tracing::debug!("tool_batch: LSP hooks done");
        }

        // Defense-in-depth: check if process cwd changed during this tool batch.
        // Normally only changes via set_working_directory; this also catches any
        // future code path that calls set_current_dir.
        self.check_cwd_changed().await;

        Ok(())
    }

    /// Spawn an audit log entry for a non-clean VIGIL verdict on a tool output.
    ///
    /// No-op when the verdict is `Clean` or no audit logger is configured.
    fn fire_vigil_audit_entry(
        &mut self,
        tool_name: &str,
        vigil_outcome: Option<&super::VigilOutcome>,
    ) {
        let (Some(vo), Some(logger)) = (
            vigil_outcome.filter(|v| !matches!(v, super::VigilOutcome::Clean)),
            self.tool_orchestrator.audit_logger.as_ref(),
        ) else {
            return;
        };
        let (vigil_risk, audit_result, err_cat) = if vo.is_blocked() {
            (
                Some(zeph_tools::VigilRiskLevel::High),
                zeph_tools::AuditResult::Blocked {
                    reason: "vigil_blocked".into(),
                },
                "vigil_blocked",
            )
        } else {
            (
                Some(zeph_tools::VigilRiskLevel::Medium),
                zeph_tools::AuditResult::Success,
                "vigil_sanitized",
            )
        };
        let entry = zeph_tools::AuditEntry {
            timestamp: zeph_tools::chrono_now(),
            tool: tool_name.to_owned().into(),
            command: String::new(),
            result: audit_result,
            duration_ms: 0,
            error_category: Some(err_cat.to_owned()),
            error_domain: Some("security".to_owned()),
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            caller_id: None,
            policy_match: None,
            correlation_id: None,
            vigil_risk,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let logger = std::sync::Arc::clone(logger);
        self.runtime.lifecycle.supervisor.spawn(
            crate::agent::agent_supervisor::TaskClass::Telemetry,
            "vigil-audit-log",
            async move { logger.log(&entry).await },
        );
    }

    /// Spawn a fire-and-forget task to record this tool call's outcome in the experience store.
    ///
    /// No-op when experience memory is not configured or no active conversation ID exists.
    fn record_tool_experience(
        &mut self,
        tool_name: &str,
        vigil_blocked: bool,
        is_error: bool,
        tool_succeeded: bool,
        tool_err_category: Option<&zeph_tools::error_taxonomy::ToolErrorCategory>,
        llm_content: &str,
    ) {
        let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
            return;
        };
        let Some(experience) = memory.experience.as_ref() else {
            return;
        };
        let Some(conversation_id) = self.services.memory.persistence.conversation_id else {
            return;
        };
        let (outcome, detail, error_ctx): (&'static str, Option<String>, Option<String>) =
            if vigil_blocked {
                (
                    "blocked",
                    Some("vigil".to_owned()),
                    Some(truncate_utf8(llm_content, 256)),
                )
            } else if is_error {
                (
                    "error",
                    tool_err_category.map(|c| format!("{c:?}")),
                    Some(truncate_utf8(llm_content, 256)),
                )
            } else if tool_succeeded {
                ("success", None, None)
            } else {
                ("unknown", None, None)
            };
        let exp = std::sync::Arc::clone(experience);
        let session_id = conversation_id.0.to_string();
        let turn = i64::try_from(self.services.sidequest.turn_counter).unwrap_or(i64::MAX);
        let tool_name_owned = tool_name.to_owned();
        let accepted = self.runtime.lifecycle.supervisor.spawn(
            crate::agent::agent_supervisor::TaskClass::Telemetry,
            "experience-record",
            async move {
                if let Err(e) = exp
                    .record_tool_outcome(
                        &session_id,
                        turn,
                        &tool_name_owned,
                        outcome,
                        detail.as_deref(),
                        error_ctx.as_deref(),
                    )
                    .await
                {
                    tracing::warn!(
                        tool = %tool_name_owned, outcome = %outcome, error = %e,
                        "experience: record_tool_outcome failed",
                    );
                }
            },
        );
        if !accepted {
            tracing::warn!(
                tool = %tool_name, outcome = %outcome,
                "experience-record dropped (telemetry class at capacity)",
            );
        }
    }

    /// Record histogram and trace-collector telemetry for a completed tool call.
    ///
    /// No-op when no histogram recorder or trace collector is configured.
    fn record_tool_execution_telemetry(
        &mut self,
        tool_name: &str,
        started_at: &std::time::Instant,
        is_error: bool,
        output: &str,
    ) {
        if let Some(ref recorder) = self.runtime.metrics.histogram_recorder {
            recorder.observe_tool_execution(started_at.elapsed());
        }
        if let Some(ref mut trace_coll) = self.runtime.debug.trace_collector
            && let Some(iter_span_id) = self.runtime.debug.current_iteration_span_id
        {
            let latency = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            let guard = trace_coll.begin_tool_call_at(tool_name, iter_span_id, started_at);
            let error_kind = is_error.then(|| output.chars().take(200).collect::<String>());
            trace_coll.end_tool_call(
                guard,
                tool_name,
                crate::debug_dump::trace::ToolAttributes {
                    latency_ms: latency,
                    is_error,
                    error_kind,
                },
            );
        }
    }

    /// Record failure-path skill outcomes and set up deferred self-reflection.
    ///
    /// Returns `true` if the tool succeeded (no `[error]` / `[exit code]` in output),
    /// `false` if the failure path was taken. `tool_err_category` is consumed via `take()`
    /// so subsequent callers see `None` after this call.
    fn handle_tool_failure_outcomes(
        &mut self,
        output: &str,
        tool_err_category: &mut Option<zeph_tools::error_taxonomy::ToolErrorCategory>,
        is_quality_failure: bool,
        pending_outcomes: &mut Vec<crate::agent::learning::PendingSkillOutcome>,
        pending_reflection: &mut Option<String>,
    ) -> bool {
        if output.contains("[error]") || output.contains("[exit code") {
            let kind = tool_err_category
                .take()
                .map_or_else(|| FailureKind::from_error(output), FailureKind::from);
            pending_outcomes.push(crate::agent::learning::PendingSkillOutcome {
                outcome: "tool_failure".into(),
                error_context: Some(output.to_owned()),
                outcome_detail: Some(kind.as_str().into()),
            });
            if is_quality_failure {
                self.provider
                    .record_quality_outcome(self.provider.name(), false);
            }
            if pending_reflection.is_none()
                && !self.services.learning_engine.was_reflection_used()
                && is_quality_failure
            {
                let sanitized_out = self
                    .services
                    .security
                    .sanitizer
                    .sanitize(output, ContentSource::new(ContentSourceKind::ToolResult))
                    .body;
                *pending_reflection = Some(sanitized_out);
            }
            false
        } else {
            true
        }
    }

    /// Classify a raw tool execution result into structured outcome fields.
    ///
    /// Extracts the output string, error flag, display metadata (diff, filter stats, locations),
    /// and outcome classification (anomaly, quality failure, error category) from the `Result`.
    /// Side effects: records filter metrics for successful outputs and fires security events for
    /// invalid memory writes.
    fn classify_tool_result(
        &mut self,
        tc: &zeph_llm::provider::ToolUseRequest,
        tool_result: Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
    ) -> ToolResultClassification {
        match tool_result {
            Ok(Some(out)) => {
                let anomaly_outcome =
                    if out.summary.contains("[error]") || out.summary.contains("[stderr]") {
                        AnomalyOutcome::Error
                    } else {
                        AnomalyOutcome::Success
                    };
                if let Some(ref fs) = out.filter_stats {
                    self.record_filter_metrics(fs);
                }
                let inline_stats = out.filter_stats.as_ref().and_then(|fs| {
                    (fs.filtered_chars < fs.raw_chars).then(|| fs.format_inline(tc.name.as_str()))
                });
                let kept_lines = out
                    .filter_stats
                    .as_ref()
                    .and_then(|fs| (!fs.kept_lines.is_empty()).then(|| fs.kept_lines.clone()));
                ToolResultClassification {
                    output: out.summary,
                    is_error: false,
                    diff: out.diff,
                    inline_stats,
                    kept_lines,
                    locations: out.locations,
                    anomaly_outcome,
                    is_quality_failure: false,
                    tool_err_category: None,
                }
            }
            Ok(None) => ToolResultClassification {
                output: "(no output)".to_owned(),
                is_error: false,
                diff: None,
                inline_stats: None,
                kept_lines: None,
                locations: None,
                anomaly_outcome: AnomalyOutcome::Success,
                is_quality_failure: false,
                tool_err_category: None,
            },
            Err(ref e) => {
                let category = e.category();
                let is_quality_failure = category.is_quality_failure();
                let anomaly_outcome = if matches!(e, zeph_tools::ToolError::Blocked { .. }) {
                    AnomalyOutcome::Blocked
                } else if is_quality_failure && zeph_tools::is_reasoning_model(self.provider.name())
                {
                    AnomalyOutcome::ReasoningQualityFailure {
                        model: self.provider.name().to_owned(),
                        tool: tc.name.to_string(),
                    }
                } else {
                    AnomalyOutcome::Error
                };
                if let Some(ref d) = self.runtime.debug.debug_dumper {
                    d.dump_tool_error(tc.name.as_str(), e);
                }
                if tc.name == "memory_save"
                    && matches!(e, zeph_tools::ToolError::InvalidParams { .. })
                    && e.to_string().contains("memory write rejected")
                {
                    self.update_metrics(|m| m.memory_validation_failures += 1);
                    self.push_security_event(
                        zeph_common::SecurityEventCategory::MemoryValidation,
                        "memory_save",
                        e.to_string(),
                    );
                }
                let feedback = zeph_tools::ToolErrorFeedback {
                    category,
                    message: e.to_string(),
                    retryable: category.is_retryable(),
                };
                ToolResultClassification {
                    output: feedback.format_for_llm(),
                    is_error: true,
                    diff: None,
                    inline_stats: None,
                    kept_lines: None,
                    locations: None,
                    anomaly_outcome,
                    is_quality_failure,
                    tool_err_category: Some(category),
                }
            }
        }
    }

    /// Process the result of a single tool execution within the batch result loop.
    ///
    /// Handles metrics, trace spans, skill learning, channel display, VIGIL gate, sanitization,
    /// experience memory recording, and appends a `MessagePart::ToolResult` to `result_parts`.
    /// Returns `Err` only on hard channel errors (e.g., broken pipe on `send_tool_output`).
    #[allow(clippy::too_many_arguments)]
    async fn process_one_tool_result(
        &mut self,
        tc: &zeph_llm::provider::ToolUseRequest,
        tool_call_id: &str,
        started_at: &std::time::Instant,
        tool_result: Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
        result_parts: &mut Vec<MessagePart>,
        lsp_tool_calls: &mut Vec<(String, serde_json::Value, String)>,
        has_any_injection_flags: &mut bool,
        pending_reflection: &mut Option<String>,
        pending_outcomes: &mut Vec<crate::agent::learning::PendingSkillOutcome>,
    ) -> Result<(), crate::agent::error::AgentError> {
        let ToolResultClassification {
            output,
            mut is_error,
            diff,
            inline_stats,
            kept_lines,
            locations,
            anomaly_outcome,
            is_quality_failure,
            mut tool_err_category,
        } = self.classify_tool_result(tc, tool_result);

        self.record_tool_execution_telemetry(tc.name.as_str(), started_at, is_error, &output);

        let tool_succeeded = self.handle_tool_failure_outcomes(
            &output,
            &mut tool_err_category,
            is_quality_failure,
            pending_outcomes,
            pending_reflection,
        );
        let _ = self.record_anomaly_outcome(anomaly_outcome).await;

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
                tool_name: tc.name.clone(),
                display: body_display.into_owned(),
                diff,
                filter_stats: inline_stats,
                kept_lines,
                locations,
                tool_call_id: tool_call_id.to_owned(),
                is_error,
                terminal_id: None,
                parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                raw_response: None,
                started_at: Some(*started_at),
            })
            .await?;

        let (processed, vigil_outcome) = self.run_vigil_gate(tc.name.as_str(), processed);
        self.fire_vigil_audit_entry(tc.name.as_str(), vigil_outcome.as_ref());

        let (llm_content, tool_had_injection_flags) = match &vigil_outcome {
            Some(super::VigilOutcome::Blocked { sentinel, .. }) => {
                is_error = true;
                (sentinel.clone(), false)
            }
            _ => {
                self.sanitize_tool_output(&processed, tc.name.as_str())
                    .await
            }
        };
        *has_any_injection_flags |= tool_had_injection_flags;

        let vigil_blocked = vigil_outcome
            .as_ref()
            .is_some_and(super::VigilOutcome::is_blocked);
        if !is_error && !vigil_blocked {
            lsp_tool_calls.push((tc.name.to_string(), tc.input.clone(), llm_content.clone()));
        }

        if vigil_blocked {
            pending_outcomes.push(crate::agent::learning::PendingSkillOutcome {
                outcome: FailureKind::SecurityBlocked.as_str().into(),
                error_context: Some("VIGIL blocked tool output".into()),
                outcome_detail: None,
            });
        } else if tool_succeeded {
            pending_outcomes.push(crate::agent::learning::PendingSkillOutcome {
                outcome: "success".into(),
                error_context: None,
                outcome_detail: None,
            });
            self.provider
                .record_quality_outcome(self.provider.name(), true);
        }

        self.record_tool_experience(
            tc.name.as_str(),
            vigil_blocked,
            is_error,
            tool_succeeded,
            tool_err_category.as_ref(),
            &llm_content,
        );

        result_parts.push(MessagePart::ToolResult {
            tool_use_id: tc.id.clone(),
            content: llm_content,
            is_error,
        });
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
async fn recv_elicitation(
    rx: &mut Option<tokio::sync::mpsc::Receiver<zeph_mcp::ElicitationEvent>>,
) -> Option<zeph_mcp::ElicitationEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Build a skipped `ToolOutput` for utility gate non-`ToolCall` actions.
///
/// All four non-execute arms (Respond/Retrieve/Verify/Stop) produce an identical struct shape
/// with zero blocks executed and no diff/filter metadata.
fn skipped_output(
    tool_name: impl Into<zeph_common::ToolName>,
    summary: impl Into<String>,
) -> zeph_tools::ToolOutput {
    zeph_tools::ToolOutput {
        tool_name: tool_name.into(),
        summary: summary.into(),
        blocks_executed: 0,
        filter_stats: None,
        diff: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    }
}

/// Wrap a pre-built `ToolOutput` as an immediately-ready boxed future suitable for
/// insertion into the tier futures list.
fn ready_fut(idx: usize, out: zeph_tools::ToolOutput) -> (usize, ToolExecFut) {
    (idx, Box::pin(std::future::ready(Ok(Some(out)))))
}

/// Truncate `s` to at most `max_bytes` bytes on a valid UTF-8 char boundary.
///
/// Never panics: if `s` is shorter than `max_bytes`, it is returned as-is.
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

// T-CRIT-02: handle_focus_tool tests — happy path, error paths, checkpoint pinning (S5 fix).
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
}
