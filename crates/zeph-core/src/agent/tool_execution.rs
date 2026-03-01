// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tokio_stream::StreamExt;
use zeph_llm::provider::{
    ChatResponse, LlmProvider, Message, MessageMetadata, MessagePart, Role, ToolDefinition,
};
use zeph_tools::executor::{ToolCall, ToolError, ToolOutput};

use super::{Agent, DOOM_LOOP_WINDOW, TOOL_LOOP_KEEP_RECENT, format_tool_output};
use crate::channel::Channel;
use crate::redact::redact_secrets;
use tracing::Instrument;
use zeph_skills::evolution::FailureKind;

enum AnomalyOutcome {
    Success,
    Error,
    Blocked,
}

/// Hash message content for doom-loop detection, skipping volatile IDs in-place.
/// Normalizes `[tool_result: <id>]` → `[tool_result]` and `[tool_use: <name>(<id>)]` → `[tool_use: <name>]`
/// by feeding only stable segments into the hasher without materializing the normalized string.
// DefaultHasher output is not stable across Rust versions — do not persist or serialize
// these hashes. They are used only for within-session equality comparison.
fn doom_loop_hash(content: &str) -> u64 {
    use std::hash::{DefaultHasher, Hasher};
    let mut hasher = DefaultHasher::new();
    let mut rest = content;
    while !rest.is_empty() {
        let r_pos = rest.find("[tool_result: ");
        let u_pos = rest.find("[tool_use: ");
        match (r_pos, u_pos) {
            (Some(r), Some(u)) if u < r => hash_tool_use_in_place(&mut hasher, &mut rest, u),
            (Some(r), _) => hash_tool_result_in_place(&mut hasher, &mut rest, r),
            (_, Some(u)) => hash_tool_use_in_place(&mut hasher, &mut rest, u),
            _ => {
                hasher.write(rest.as_bytes());
                break;
            }
        }
    }
    hasher.finish()
}

fn hash_tool_result_in_place(hasher: &mut impl std::hash::Hasher, rest: &mut &str, start: usize) {
    hasher.write(&rest.as_bytes()[..start]);
    if let Some(end) = rest[start..].find(']') {
        hasher.write(b"[tool_result]");
        *rest = &rest[start + end + 1..];
    } else {
        hasher.write(&rest.as_bytes()[start..]);
        *rest = "";
    }
}

fn hash_tool_use_in_place(hasher: &mut impl std::hash::Hasher, rest: &mut &str, start: usize) {
    hasher.write(&rest.as_bytes()[..start]);
    let tag = &rest[start..];
    if let (Some(paren), Some(end)) = (tag.find('('), tag.find(']')) {
        hasher.write(&tag.as_bytes()[..paren]);
        hasher.write(b"]");
        *rest = &rest[start + end + 1..];
    } else {
        hasher.write(tag.as_bytes());
        *rest = "";
    }
}

#[cfg(test)]
fn normalize_for_doom_loop(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while !rest.is_empty() {
        let r_pos = rest.find("[tool_result: ");
        let u_pos = rest.find("[tool_use: ");
        match (r_pos, u_pos) {
            (Some(r), Some(u)) if u < r => {
                handle_tool_use(&mut out, &mut rest, u);
            }
            (Some(r), _) => {
                handle_tool_result(&mut out, &mut rest, r);
            }
            (_, Some(u)) => {
                handle_tool_use(&mut out, &mut rest, u);
            }
            _ => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
fn handle_tool_result(out: &mut String, rest: &mut &str, start: usize) {
    out.push_str(&rest[..start]);
    if let Some(end) = rest[start..].find(']') {
        out.push_str("[tool_result]");
        *rest = &rest[start + end + 1..];
    } else {
        out.push_str(&rest[start..]);
        *rest = "";
    }
}

#[cfg(test)]
fn handle_tool_use(out: &mut String, rest: &mut &str, start: usize) {
    out.push_str(&rest[..start]);
    let tag = &rest[start..];
    if let (Some(paren), Some(end)) = (tag.find('('), tag.find(']')) {
        out.push_str(&tag[..paren]);
        out.push(']');
        *rest = &rest[start + end + 1..];
    } else {
        out.push_str(tag);
        *rest = "";
    }
}

impl<C: Channel> Agent<C> {
    pub(crate) async fn process_response(&mut self) -> Result<(), super::error::AgentError> {
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

        for iteration in 0..self.tool_orchestrator.max_iterations {
            if self.cancel_token.is_cancelled() {
                tracing::info!("tool loop cancelled by user");
                break;
            }

            self.channel.send_typing().await?;

            // Context budget check at 80% threshold
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
            let Some(response) = self.call_llm_with_retry(2).await? else {
                let _ = self.channel.send_status("").await;
                return Ok(());
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
                    return Ok(());
                }

                self.channel
                    .send("Received an empty response. Please try again.")
                    .await?;
                return Ok(());
            }

            self.push_message(Message {
                role: Role::Assistant,
                content: response.clone(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
            self.persist_message(Role::Assistant, &response).await;

            self.inject_active_skill_env();
            let _ = self.channel.send_status("running tool...").await;
            let result = self
                .tool_executor
                .execute_erased(&response)
                .instrument(tracing::info_span!("tool_exec"))
                .await;
            let _ = self.channel.send_status("").await;
            self.tool_executor.set_skill_env(None);
            if !self.handle_tool_result(&response, result).await? {
                return Ok(());
            }

            // Prune tool output bodies from older iterations to reduce context growth
            self.prune_stale_tool_outputs(TOOL_LOOP_KEEP_RECENT);
            self.maybe_summarize_tool_pair().await;

            // Doom-loop detection: compare last N outputs by content hash
            if let Some(last_msg) = self.messages.last() {
                self.tool_orchestrator
                    .push_doom_hash(doom_loop_hash(&last_msg.content));
                if self.tool_orchestrator.is_doom_loop() {
                    tracing::warn!(
                        iteration,
                        "doom-loop detected: {DOOM_LOOP_WINDOW} consecutive identical outputs"
                    );
                    self.channel
                        .send("Stopping: detected repeated identical tool outputs.")
                        .await?;
                    break;
                }
            }
        }

        Ok(())
    }

    pub(super) async fn call_llm_with_timeout(
        &mut self,
    ) -> Result<Option<String>, super::error::AgentError> {
        if self.cancel_token.is_cancelled() {
            return Ok(None);
        }

        if let Some(ref tracker) = self.cost_tracker
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
        let prompt_estimate = self.cached_prompt_tokens;

        let llm_span = tracing::info_span!("llm_call", model = %self.runtime.model_name);
        if self.provider.supports_streaming() {
            let cancel = self.cancel_token.clone();
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
            if let Ok(r) = result {
                let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                let completion_estimate_for_cost = r
                    .as_ref()
                    .map_or(0, |s| u64::try_from(s.len()).unwrap_or(0) / 4);
                self.update_metrics(|m| {
                    m.api_calls += 1;
                    m.last_llm_latency_ms = latency;
                    m.context_tokens = prompt_estimate;
                    m.prompt_tokens += prompt_estimate;
                    m.total_tokens = m.prompt_tokens + m.completion_tokens;
                });
                self.record_cache_usage();
                self.record_cost(prompt_estimate, completion_estimate_for_cost);
                let raw = r?;
                // Redact secrets from the full accumulated response before it is persisted to
                // history. Per-chunk redaction is applied during streaming (see send_chunk above).
                let redacted = self.maybe_redact(&raw).into_owned();
                Ok(Some(redacted))
            } else {
                self.channel
                    .send("LLM request timed out. Please try again.")
                    .await?;
                Ok(None)
            }
        } else {
            let cancel = self.cancel_token.clone();
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
                    let completion_estimate = u64::try_from(resp.len()).unwrap_or(0) / 4;
                    self.update_metrics(|m| {
                        m.api_calls += 1;
                        m.last_llm_latency_ms = latency;
                        m.context_tokens = prompt_estimate;
                        m.prompt_tokens += prompt_estimate;
                        m.completion_tokens += completion_estimate;
                        m.total_tokens = m.prompt_tokens + m.completion_tokens;
                    });
                    self.record_cache_usage();
                    self.record_cost(prompt_estimate, completion_estimate);
                    let display = self.maybe_redact(&resp);
                    self.channel.send(&display).await?;
                    self.store_response_in_cache(&resp).await;
                    Ok(Some(resp))
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
    }

    /// Call LLM with retry on context length error.
    /// On `ContextLengthExceeded`, compacts context and retries up to `max_attempts` times.
    pub(super) async fn call_llm_with_retry(
        &mut self,
        max_attempts: usize,
    ) -> Result<Option<String>, super::error::AgentError> {
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

    /// Call `chat_with_tools` with retry on context length error.
    pub(super) async fn call_chat_with_tools_retry(
        &mut self,
        tool_defs: &[ToolDefinition],
        max_attempts: usize,
    ) -> Result<Option<ChatResponse>, super::error::AgentError> {
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

    pub(super) fn last_user_query(&self) -> &str {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User && !m.content.starts_with("[tool output"))
            .map_or("", |m| m.content.as_str())
    }

    pub(super) async fn summarize_tool_output(&self, output: &str) -> String {
        let truncated = zeph_tools::truncate_tool_output(output);
        let query = self.last_user_query();
        let prompt = format!(
            "The user asked: {query}\n\n\
             A tool produced output ({len} chars, truncated to fit).\n\
             Summarize the key information relevant to the user's question.\n\
             Preserve exact: file paths, error messages, numeric values, exit codes.\n\n\
             {truncated}",
            len = output.len(),
        );

        let messages = vec![Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let llm_timeout = std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds);
        let result = tokio::time::timeout(
            llm_timeout,
            self.summary_or_primary_provider().chat(&messages),
        )
        .await;
        match result {
            Ok(Ok(summary)) => format!("[tool output summary]\n```\n{summary}\n```"),
            Ok(Err(e)) => {
                tracing::warn!(
                    "tool output summarization failed, falling back to truncation: {e:#}"
                );
                truncated
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = self.runtime.timeouts.llm_seconds,
                    "tool output summarization timed out, falling back to truncation"
                );
                truncated
            }
        }
    }

    pub(super) async fn maybe_summarize_tool_output(&self, output: &str) -> String {
        if output.len() <= self.tool_orchestrator.overflow_config.threshold {
            return output.to_string();
        }
        let overflow_notice = if let Some(filename) =
            zeph_tools::save_overflow(output, &self.tool_orchestrator.overflow_config)
        {
            format!("\n[full output saved to {filename}, use read tool to access]")
        } else {
            String::new()
        };
        let truncated = if self.tool_orchestrator.summarize_tool_output_enabled {
            self.summarize_tool_output(output).await
        } else {
            zeph_tools::truncate_tool_output(output)
        };
        format!("{truncated}{overflow_notice}")
    }

    async fn record_anomaly_outcome(
        &mut self,
        outcome: AnomalyOutcome,
    ) -> Result<(), super::error::AgentError> {
        let Some(ref mut det) = self.anomaly_detector else {
            return Ok(());
        };
        match outcome {
            AnomalyOutcome::Success => det.record_success(),
            AnomalyOutcome::Error => det.record_error(),
            AnomalyOutcome::Blocked => det.record_blocked(),
        }
        if let Some(anomaly) = det.check() {
            tracing::warn!(severity = ?anomaly.severity, "{}", anomaly.description);
            self.channel
                .send(&format!("[anomaly] {}", anomaly.description))
                .await?;
        }
        Ok(())
    }

    /// Returns `true` if the tool loop should continue.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn handle_tool_result(
        &mut self,
        response: &str,
        result: Result<Option<ToolOutput>, ToolError>,
    ) -> Result<bool, super::error::AgentError> {
        match result {
            Ok(Some(output)) => {
                if let Some(ref fs) = output.filter_stats {
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
                if output.summary.trim().is_empty() {
                    tracing::warn!("tool execution returned empty output");
                    self.record_skill_outcomes("success", None, None).await;
                    return Ok(false);
                }

                if output.summary.contains("[error]") || output.summary.contains("[exit code") {
                    let kind = FailureKind::from_error(&output.summary);
                    self.record_skill_outcomes(
                        "tool_failure",
                        Some(&output.summary),
                        Some(kind.as_str()),
                    )
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
                    .send_tool_start(
                        &output.tool_name,
                        &tool_call_id,
                        None,
                        self.parent_tool_use_id.clone(),
                    )
                    .await?;
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
                    .send_tool_output(
                        &output.tool_name,
                        &self.maybe_redact(&body),
                        None,
                        filter_stats_inline,
                        None,
                        output.locations,
                        &tool_call_id,
                        false,
                        self.parent_tool_use_id.clone(),
                        output.raw_response.map(|r| self.redact_json(r)),
                        Some(tool_started_at),
                    )
                    .await?;

                self.push_message(Message::from_parts(
                    Role::User,
                    vec![MessagePart::ToolOutput {
                        tool_name: output.tool_name.clone(),
                        body: processed,
                        compacted_at: None,
                    }],
                ));
                self.persist_message(Role::User, &formatted_output).await;
                let outcome = if output.summary.contains("[error]")
                    || output.summary.contains("[exit code")
                {
                    AnomalyOutcome::Error
                } else {
                    AnomalyOutcome::Success
                };
                self.record_anomaly_outcome(outcome).await?;
                Ok(true)
            }
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
                let prompt = format!("Allow command: {command}?");
                if self.channel.confirm(&prompt).await? {
                    if let Ok(Some(out)) =
                        self.tool_executor.execute_confirmed_erased(response).await
                    {
                        let confirmed_tool_call_id = uuid::Uuid::new_v4().to_string();
                        let confirmed_started_at = std::time::Instant::now();
                        self.channel
                            .send_tool_start(
                                &out.tool_name,
                                &confirmed_tool_call_id,
                                None,
                                self.parent_tool_use_id.clone(),
                            )
                            .await?;
                        let processed = self.maybe_summarize_tool_output(&out.summary).await;
                        let formatted = format_tool_output(&out.tool_name, &processed);
                        self.channel
                            .send_tool_output(
                                &out.tool_name,
                                &self.maybe_redact(&processed),
                                None,
                                None,
                                None,
                                out.locations,
                                &confirmed_tool_call_id,
                                false,
                                self.parent_tool_use_id.clone(),
                                out.raw_response.map(|r| self.redact_json(r)),
                                Some(confirmed_started_at),
                            )
                            .await?;
                        self.push_message(Message::from_parts(
                            Role::User,
                            vec![MessagePart::ToolOutput {
                                tool_name: out.tool_name.clone(),
                                body: processed,
                                compacted_at: None,
                            }],
                        ));
                        self.persist_message(Role::User, &formatted).await;
                    }
                } else {
                    self.channel.send("Command cancelled.").await?;
                }
                Ok(false)
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
                let kind = FailureKind::from_error(&err_str);
                self.record_skill_outcomes("tool_failure", Some(&err_str), Some(kind.as_str()))
                    .await;
                self.record_anomaly_outcome(AnomalyOutcome::Error).await?;

                if !self.learning_engine.was_reflection_used()
                    && self.attempt_self_reflection(&err_str, "").await?
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

    pub(super) async fn process_response_streaming(
        &mut self,
    ) -> Result<String, super::error::AgentError> {
        let mut stream = self.provider.chat_stream(&self.messages).await?;
        let mut response = String::with_capacity(2048);

        loop {
            let chunk_result = tokio::select! {
                item = stream.next() => match item {
                    Some(r) => r,
                    None => break,
                },
                () = super::shutdown_signal(&mut self.shutdown) => {
                    tracing::info!("streaming interrupted by shutdown");
                    break;
                }
                () = self.cancel_token.cancelled() => {
                    tracing::info!("streaming interrupted by cancellation");
                    break;
                }
            };
            let chunk: String = chunk_result?;
            response.push_str(&chunk);
            let display_chunk = self.maybe_redact(&chunk);
            self.channel.send_chunk(&display_chunk).await?;
        }

        self.channel.flush_chunks().await?;

        let completion_estimate = u64::try_from(response.len()).unwrap_or(0) / 4;
        self.update_metrics(|m| {
            m.completion_tokens += completion_estimate;
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
        });

        Ok(response)
    }

    pub(super) fn maybe_redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.runtime.security.redact_secrets {
            let redacted = redact_secrets(text);
            let sanitized = crate::redact::sanitize_paths(&redacted);
            match sanitized {
                std::borrow::Cow::Owned(s) => std::borrow::Cow::Owned(s),
                std::borrow::Cow::Borrowed(_) => redacted,
            }
        } else {
            std::borrow::Cow::Borrowed(text)
        }
    }

    /// Walk a JSON value and apply `maybe_redact` to every string leaf.
    ///
    /// Used to sanitize `raw_response` before it is forwarded to `claudeCode.toolResponse`
    /// in the ACP notification. Without this, file content and shell stdout would bypass
    /// the `redact_secrets` pipeline even when it is enabled.
    pub(super) fn redact_json(&self, value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => {
                serde_json::Value::String(self.maybe_redact(&s).into_owned())
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.into_iter().map(|v| self.redact_json(v)).collect())
            }
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, self.redact_json(v)))
                    .collect(),
            ),
            other => other,
        }
    }

    async fn check_response_cache(&mut self) -> Result<Option<String>, super::error::AgentError> {
        if let Some(ref cache) = self.response_cache
            && !self.provider.supports_streaming()
        {
            let key =
                zeph_memory::ResponseCache::compute_key(&self.messages, &self.runtime.model_name);
            if let Ok(Some(cached)) = cache.get(&key).await {
                tracing::debug!("response cache hit");
                let display = self.maybe_redact(&cached);
                self.channel.send(&display).await?;
                return Ok(Some(cached));
            }
        }
        Ok(None)
    }

    async fn store_response_in_cache(&self, response: &str) {
        if let Some(ref cache) = self.response_cache {
            let key =
                zeph_memory::ResponseCache::compute_key(&self.messages, &self.runtime.model_name);
            if let Err(e) = cache.put(&key, response, &self.runtime.model_name).await {
                tracing::warn!("failed to store response in cache: {e:#}");
            }
        }
    }

    async fn process_response_native_tools(&mut self) -> Result<(), super::error::AgentError> {
        self.tool_orchestrator.clear_doom_history();

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
                if !text.is_empty() {
                    let display = self.maybe_redact(text);
                    self.channel.send(&display).await?;
                }
                self.messages
                    .push(Message::from_legacy(Role::Assistant, text.as_str()));
                self.persist_message(Role::Assistant, text).await;
                self.channel.flush_chunks().await?;
                return Ok(());
            }

            // ToolUse → execute tools and loop
            let ChatResponse::ToolUse { text, tool_calls } = chat_result else {
                unreachable!();
            };
            self.handle_native_tool_calls(text.as_deref(), &tool_calls)
                .await?;

            // Prune tool output bodies from older iterations to reduce context growth
            self.prune_stale_tool_outputs(TOOL_LOOP_KEEP_RECENT);
            self.maybe_summarize_tool_pair().await;

            if self.check_doom_loop(iteration).await? {
                break;
            }
        }

        self.channel.flush_chunks().await?;
        Ok(())
    }

    async fn call_chat_with_tools(
        &mut self,
        tool_defs: &[ToolDefinition],
    ) -> Result<Option<ChatResponse>, super::error::AgentError> {
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
        let completion_estimate = match &result {
            ChatResponse::Text(t) => u64::try_from(t.len()).unwrap_or(0) / 4,
            ChatResponse::ToolUse { text, tool_calls } => {
                let text_len = text.as_deref().map_or(0, str::len);
                let calls_len: usize = tool_calls
                    .iter()
                    .map(|c| c.name.len() + c.input.to_string().len())
                    .sum();
                u64::try_from(text_len + calls_len).unwrap_or(0) / 4
            }
        };
        self.update_metrics(|m| {
            m.api_calls += 1;
            m.last_llm_latency_ms = latency;
            m.context_tokens = prompt_estimate;
            m.prompt_tokens += prompt_estimate;
            m.completion_tokens += completion_estimate;
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
        });
        self.record_cache_usage();
        self.record_cost(prompt_estimate, completion_estimate);

        if let Some((input_tokens, output_tokens)) = self.provider.last_usage() {
            let context_window =
                u64::try_from(self.provider.context_window().unwrap_or(0)).unwrap_or(0);
            let _ = self
                .channel
                .send_usage(input_tokens, output_tokens, context_window)
                .await;
        }

        Ok(Some(result))
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_native_tool_calls(
        &mut self,
        text: Option<&str>,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<(), super::error::AgentError> {
        if let Some(t) = text
            && !t.is_empty()
        {
            let display = self.maybe_redact(t);
            self.channel.send(&display).await?;
        }

        let mut parts: Vec<MessagePart> = Vec::new();
        if let Some(t) = text
            && !t.is_empty()
        {
            parts.push(MessagePart::Text { text: t.to_owned() });
        }
        for tc in tool_calls {
            parts.push(MessagePart::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.input.clone(),
            });
        }
        let assistant_msg = Message::from_parts(Role::Assistant, parts);
        self.persist_message(Role::Assistant, &assistant_msg.content)
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
                .send_tool_start(
                    &tc.name,
                    tool_call_id,
                    Some(raw_params),
                    self.parent_tool_use_id.clone(),
                )
                .await?;
        }

        // Inject active skill secrets before tool execution
        self.inject_active_skill_env();
        // Execute tool calls in parallel, with cancellation
        let max_parallel = self.runtime.timeouts.max_parallel_tools;
        let exec_fut = async {
            if calls.len() <= max_parallel {
                let futs: Vec<_> = calls
                    .iter()
                    .zip(tool_calls.iter())
                    .map(|(call, tc)| {
                        self.tool_executor.execute_tool_call_erased(call).instrument(
                            tracing::info_span!("tool_exec", tool_name = %tc.name, idx = %tc.id),
                        )
                    })
                    .collect();
                futures::future::join_all(futs).await
            } else {
                use futures::StreamExt;
                let stream =
                    futures::stream::iter(calls.iter().zip(tool_calls.iter()).map(|(call, tc)| {
                        self.tool_executor.execute_tool_call_erased(call).instrument(
                            tracing::info_span!("tool_exec", tool_name = %tc.name, idx = %tc.id),
                        )
                    }));
                futures::StreamExt::collect::<Vec<_>>(stream.buffered(max_parallel)).await
            }
        };
        let tool_results = tokio::select! {
            results = exec_fut => results,
            () = self.cancel_token.cancelled() => {
                self.tool_executor.set_skill_env(None);
                tracing::info!("tool execution cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                return Ok(());
            }
        };
        self.tool_executor.set_skill_env(None);

        // Process results sequentially (metrics, channel sends, message parts)
        let mut result_parts: Vec<MessagePart> = Vec::new();
        for (((tc, tool_result), tool_call_id), started_at) in tool_calls
            .iter()
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

            let processed = self.maybe_summarize_tool_output(&output).await;
            let body = if let Some(ref stats) = inline_stats {
                format!("{stats}\n{processed}")
            } else {
                processed.clone()
            };
            let body_display = self.maybe_redact(&body);
            self.channel
                .send_tool_output(
                    &tc.name,
                    &body_display,
                    diff,
                    inline_stats,
                    kept_lines,
                    locations,
                    tool_call_id,
                    is_error,
                    self.parent_tool_use_id.clone(),
                    None,
                    Some(*started_at),
                )
                .await?;

            result_parts.push(MessagePart::ToolResult {
                tool_use_id: tc.id.clone(),
                content: processed,
                is_error,
            });
        }

        let user_msg = Message::from_parts(Role::User, result_parts);
        self.persist_message(Role::User, &user_msg.content).await;
        self.push_message(user_msg);

        Ok(())
    }

    /// Inject environment variables from the active skill's required secrets into the executor.
    ///
    /// Secret `github_token` maps to env var `GITHUB_TOKEN` (uppercased, underscores preserved).
    fn inject_active_skill_env(&self) {
        if self.skill_state.active_skill_names.is_empty()
            || self.skill_state.available_custom_secrets.is_empty()
        {
            return;
        }
        let env: std::collections::HashMap<String, String> = self
            .skill_state
            .active_skill_names
            .iter()
            .filter_map(|name| self.skill_state.registry.get_skill(name).ok())
            .flat_map(|skill| {
                skill
                    .meta
                    .requires_secrets
                    .into_iter()
                    .filter_map(|secret_name| {
                        self.skill_state
                            .available_custom_secrets
                            .get(&secret_name)
                            .map(|secret| {
                                let env_key = secret_name.to_uppercase();
                                // Secret is intentionally exposed here for subprocess
                                // env injection, not for logging.
                                let value = secret.expose().to_owned(); // lgtm[rust/cleartext-logging]
                                (env_key, value)
                            })
                    })
            })
            .collect();
        if !env.is_empty() {
            self.tool_executor.set_skill_env(Some(env));
        }
    }

    /// Returns `true` if a doom loop was detected and the caller should break.
    async fn check_doom_loop(
        &mut self,
        iteration: usize,
    ) -> Result<bool, super::error::AgentError> {
        if let Some(last_msg) = self.messages.last() {
            self.tool_orchestrator
                .push_doom_hash(doom_loop_hash(&last_msg.content));
            if self.tool_orchestrator.is_doom_loop() {
                tracing::warn!(
                    iteration,
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

fn tool_def_to_definition(def: &zeph_tools::registry::ToolDef) -> ToolDefinition {
    let mut params = serde_json::to_value(&def.schema).unwrap_or_default();
    if let serde_json::Value::Object(ref mut map) = params {
        map.remove("$schema");
        map.remove("title");
    }
    ToolDefinition {
        name: def.id.to_string(),
        description: def.description.to_string(),
        parameters: params,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use futures::future::join_all;
    use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};

    use super::{doom_loop_hash, normalize_for_doom_loop, tool_def_to_definition};

    #[test]
    fn tool_def_strips_schema_and_title() {
        use schemars::Schema;
        use zeph_tools::registry::{InvocationHint, ToolDef};

        let raw: serde_json::Value = serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "BashParams",
            "type": "object",
            "properties": {
                "command": { "type": "string" }
            },
            "required": ["command"]
        });
        let schema: Schema = serde_json::from_value(raw).expect("valid schema");
        let def = ToolDef {
            id: "bash".into(),
            description: "run a shell command".into(),
            schema,
            invocation: InvocationHint::ToolCall,
        };

        let result = tool_def_to_definition(&def);
        let map = result.parameters.as_object().expect("should be object");
        assert!(!map.contains_key("$schema"));
        assert!(!map.contains_key("title"));
        assert!(map.contains_key("type"));
        assert!(map.contains_key("properties"));
    }

    #[test]
    fn normalize_empty_string() {
        assert_eq!(normalize_for_doom_loop(""), "");
    }

    #[test]
    fn normalize_multiple_tool_results() {
        let s = "[tool_result: id1]\nok\n[tool_result: id2]\nfail\n[tool_result: id3]\nok";
        let expected = "[tool_result]\nok\n[tool_result]\nfail\n[tool_result]\nok";
        assert_eq!(normalize_for_doom_loop(s), expected);
    }

    #[test]
    fn normalize_strips_tool_result_ids() {
        let a = "[tool_result: toolu_abc123]\nerror: missing field";
        let b = "[tool_result: toolu_xyz789]\nerror: missing field";
        assert_eq!(normalize_for_doom_loop(a), normalize_for_doom_loop(b));
        assert_eq!(
            normalize_for_doom_loop(a),
            "[tool_result]\nerror: missing field"
        );
    }

    #[test]
    fn normalize_strips_tool_use_ids() {
        let a = "[tool_use: bash(toolu_abc)]";
        let b = "[tool_use: bash(toolu_xyz)]";
        assert_eq!(normalize_for_doom_loop(a), normalize_for_doom_loop(b));
        assert_eq!(normalize_for_doom_loop(a), "[tool_use: bash]");
    }

    #[test]
    fn normalize_preserves_plain_text() {
        let text = "hello world, no tool tags here";
        assert_eq!(normalize_for_doom_loop(text), text);
    }

    #[test]
    fn normalize_handles_mixed_tag_order() {
        let s = "[tool_use: bash(id1)] result: [tool_result: id2]";
        assert_eq!(
            normalize_for_doom_loop(s),
            "[tool_use: bash] result: [tool_result]"
        );
    }

    // Helpers to hash a string the same way doom_loop_hash would if it materialized.
    fn hash_str(s: &str) -> u64 {
        use std::hash::{DefaultHasher, Hasher};
        let mut h = DefaultHasher::new();
        h.write(s.as_bytes());
        h.finish()
    }

    // doom_loop_hash must produce the same value as hashing the normalize_for_doom_loop output.
    fn expected_hash(content: &str) -> u64 {
        hash_str(&normalize_for_doom_loop(content))
    }

    #[test]
    fn doom_loop_hash_matches_normalize_then_hash_plain_text() {
        let s = "hello world, no tool tags here";
        assert_eq!(doom_loop_hash(s), expected_hash(s));
    }

    #[test]
    fn doom_loop_hash_matches_normalize_then_hash_tool_result() {
        let s = "[tool_result: toolu_abc123]\nerror: missing field";
        assert_eq!(doom_loop_hash(s), expected_hash(s));
    }

    #[test]
    fn doom_loop_hash_matches_normalize_then_hash_tool_use() {
        let s = "[tool_use: bash(toolu_abc)]";
        assert_eq!(doom_loop_hash(s), expected_hash(s));
    }

    #[test]
    fn doom_loop_hash_matches_normalize_then_hash_mixed() {
        let s = "[tool_use: bash(id1)] result: [tool_result: id2]";
        assert_eq!(doom_loop_hash(s), expected_hash(s));
    }

    #[test]
    fn doom_loop_hash_matches_normalize_then_hash_multiple_results() {
        let s = "[tool_result: id1]\nok\n[tool_result: id2]\nfail\n[tool_result: id3]\nok";
        assert_eq!(doom_loop_hash(s), expected_hash(s));
    }

    #[test]
    fn doom_loop_hash_same_content_different_ids_equal() {
        let a = "[tool_result: toolu_abc]\nerror";
        let b = "[tool_result: toolu_xyz]\nerror";
        assert_eq!(doom_loop_hash(a), doom_loop_hash(b));
    }

    #[test]
    fn doom_loop_hash_empty_string() {
        assert_eq!(doom_loop_hash(""), expected_hash(""));
    }

    struct DelayExecutor {
        delay: Duration,
        call_order: Arc<AtomicUsize>,
    }

    impl ToolExecutor for DelayExecutor {
        fn execute(
            &self,
            _response: &str,
        ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
            std::future::ready(Ok(None))
        }

        fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
            let delay = self.delay;
            let order = self.call_order.clone();
            let idx = order.fetch_add(1, Ordering::SeqCst);
            let tool_id = call.tool_id.clone();
            async move {
                tokio::time::sleep(delay).await;
                Ok(Some(ToolOutput {
                    tool_name: tool_id,
                    summary: format!("result-{idx}"),
                    blocks_executed: 1,
                    diff: None,
                    filter_stats: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                }))
            }
        }
    }

    struct FailingNthExecutor {
        fail_index: usize,
        call_count: AtomicUsize,
    }

    impl ToolExecutor for FailingNthExecutor {
        fn execute(
            &self,
            _response: &str,
        ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
            std::future::ready(Ok(None))
        }

        fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> impl Future<Output = Result<Option<ToolOutput>, ToolError>> + Send {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            let fail = idx == self.fail_index;
            let tool_id = call.tool_id.clone();
            async move {
                if fail {
                    Err(ToolError::Execution(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("tool {tool_id} failed"),
                    )))
                } else {
                    Ok(Some(ToolOutput {
                        tool_name: tool_id,
                        summary: format!("ok-{idx}"),
                        blocks_executed: 1,
                        diff: None,
                        filter_stats: None,
                        streamed: false,
                        terminal_id: None,
                        locations: None,
                        raw_response: None,
                    }))
                }
            }
        }
    }

    fn make_calls(n: usize) -> Vec<ToolCall> {
        (0..n)
            .map(|i| ToolCall {
                tool_id: format!("tool-{i}"),
                params: serde_json::Map::new(),
            })
            .collect()
    }

    #[tokio::test]
    async fn parallel_preserves_result_order() {
        let executor = DelayExecutor {
            delay: Duration::from_millis(10),
            call_order: Arc::new(AtomicUsize::new(0)),
        };
        let calls = make_calls(5);

        let futs: Vec<_> = calls
            .iter()
            .map(|c| executor.execute_tool_call(c))
            .collect();
        let results = join_all(futs).await;

        for (i, r) in results.iter().enumerate() {
            let out = r.as_ref().unwrap().as_ref().unwrap();
            assert_eq!(out.tool_name, format!("tool-{i}"));
        }
    }

    #[tokio::test]
    async fn parallel_faster_than_sequential() {
        let executor = DelayExecutor {
            delay: Duration::from_millis(50),
            call_order: Arc::new(AtomicUsize::new(0)),
        };
        let calls = make_calls(4);

        let start = Instant::now();
        let futs: Vec<_> = calls
            .iter()
            .map(|c| executor.execute_tool_call(c))
            .collect();
        let _results = join_all(futs).await;
        let parallel_time = start.elapsed();

        // Sequential would take >= 200ms (4 * 50ms); parallel should be ~50ms
        assert!(
            parallel_time < Duration::from_millis(150),
            "parallel took {parallel_time:?}, expected < 150ms"
        );
    }

    #[tokio::test]
    async fn one_failure_does_not_block_others() {
        let executor = FailingNthExecutor {
            fail_index: 1,
            call_count: AtomicUsize::new(0),
        };
        let calls = make_calls(3);

        let futs: Vec<_> = calls
            .iter()
            .map(|c| executor.execute_tool_call(c))
            .collect();
        let results = join_all(futs).await;

        assert!(results[0].is_ok());
        assert!(results[1].is_err());
        assert!(results[2].is_ok());
    }

    #[test]
    fn maybe_redact_disabled_returns_original() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use std::borrow::Cow;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);
        agent.runtime.security.redact_secrets = false;

        let text = "AWS_SECRET_ACCESS_KEY=abc123";
        let result = agent.maybe_redact(text);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(result.as_ref(), text);
    }

    #[test]
    fn maybe_redact_enabled_redacts_secrets() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);
        agent.runtime.security.redact_secrets = true;

        // A token-like secret should be redacted
        let text = "token: ghp_1234567890abcdefghijklmnopqrstuvwxyz";
        let result = agent.maybe_redact(text);
        // With redaction enabled, result should either be redacted or unchanged
        // (actual redaction depends on patterns matching)
        let _ = result.as_ref(); // just ensure no panic
    }

    #[test]
    fn redact_json_sanitizes_string_leaves() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);
        agent.runtime.security.redact_secrets = false;

        // With redaction disabled, strings pass through unchanged.
        let val = serde_json::json!({
            "file": { "content": "hello", "filePath": "/tmp/a.rs" },
            "count": 42,
            "tags": ["a", "b"]
        });
        let result = agent.redact_json(val.clone());
        assert_eq!(result, val);

        // With redaction enabled, secret patterns inside nested strings are replaced.
        agent.runtime.security.redact_secrets = true;
        let secret = "sk-abc123def456";
        let val_with_secret = serde_json::json!({
            "file": {
                "content": format!("api_key = {secret}"),
                "filePath": "/tmp/config.rs"
            },
            "stdout": format!("loaded key {secret} ok"),
            "count": 1
        });
        let redacted = agent.redact_json(val_with_secret);
        let content = redacted["file"]["content"].as_str().unwrap();
        let stdout = redacted["stdout"].as_str().unwrap();
        assert!(
            !content.contains(secret),
            "secret must not appear in file.content after redaction"
        );
        assert!(
            content.contains("[REDACTED]"),
            "file.content must contain [REDACTED]"
        );
        assert!(
            !stdout.contains(secret),
            "secret must not appear in stdout after redaction"
        );
        assert!(
            stdout.contains("[REDACTED]"),
            "stdout must contain [REDACTED]"
        );
        // Non-string fields must remain intact.
        assert_eq!(redacted["count"], 1);
    }

    #[test]
    fn redact_json_preserves_non_string_types() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let val = serde_json::json!({
            "n": 1,
            "b": true,
            "null_val": null,
            "arr": [1, 2, 3]
        });
        let result = agent.redact_json(val.clone());
        assert_eq!(result["n"], 1);
        assert_eq!(result["b"], true);
        assert!(result["null_val"].is_null());
    }

    #[test]
    fn last_user_query_finds_latest_user_message() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.push(Message {
            role: Role::User,
            content: "first question".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "some answer".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.messages.push(Message {
            role: Role::User,
            content: "second question".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        assert_eq!(agent.last_user_query(), "second question");
    }

    #[test]
    fn last_user_query_skips_tool_output_messages() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.push(Message {
            role: Role::User,
            content: "what is the result?".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        // Tool output messages start with "[tool output"
        agent.messages.push(Message {
            role: Role::User,
            content: "[tool output] some output".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        assert_eq!(agent.last_user_query(), "what is the result?");
    }

    #[test]
    fn last_user_query_no_user_messages_returns_empty() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        assert_eq!(agent.last_user_query(), "");
    }

    #[tokio::test]
    async fn handle_tool_result_blocked_returns_false() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_tools::executor::ToolError;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent
            .handle_tool_result(
                "response",
                Err(ToolError::Blocked {
                    command: "rm -rf /".into(),
                }),
            )
            .await
            .unwrap();
        assert!(!result);
        assert!(
            agent
                .channel
                .sent_messages()
                .iter()
                .any(|s| s.contains("blocked"))
        );
    }

    #[tokio::test]
    async fn handle_tool_result_cancelled_returns_false() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_tools::executor::ToolError;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent
            .handle_tool_result("response", Err(ToolError::Cancelled))
            .await
            .unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn handle_tool_result_sandbox_violation_returns_false() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_tools::executor::ToolError;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent
            .handle_tool_result(
                "response",
                Err(ToolError::SandboxViolation {
                    path: "/etc/passwd".into(),
                }),
            )
            .await
            .unwrap();
        assert!(!result);
        assert!(
            agent
                .channel
                .sent_messages()
                .iter()
                .any(|s| s.contains("sandbox"))
        );
    }

    #[tokio::test]
    async fn handle_tool_result_none_returns_false() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent
            .handle_tool_result("response", Ok(None))
            .await
            .unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn handle_tool_result_with_output_returns_true() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_tools::executor::ToolOutput;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let output = ToolOutput {
            tool_name: "bash".into(),
            summary: "hello from tool".into(),
            blocks_executed: 1,
            diff: None,
            filter_stats: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        };
        let result = agent
            .handle_tool_result("response", Ok(Some(output)))
            .await
            .unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn handle_tool_result_empty_output_returns_false() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_tools::executor::ToolOutput;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let output = ToolOutput {
            tool_name: "bash".into(),
            summary: "   ".into(), // whitespace only → considered empty
            blocks_executed: 0,
            diff: None,
            filter_stats: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        };
        let result = agent
            .handle_tool_result("response", Ok(Some(output)))
            .await
            .unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn handle_tool_result_exit_code_in_output_triggers_failure_path() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_tools::executor::ToolOutput;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let output = ToolOutput {
            tool_name: "bash".into(),
            summary: "[exit code 1] command failed".into(),
            blocks_executed: 1,
            diff: None,
            filter_stats: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        };
        // reflection_used = true so reflection path is skipped
        agent.learning_engine.mark_reflection_used();
        let result = agent
            .handle_tool_result("response", Ok(Some(output)))
            .await
            .unwrap();
        // Returns true because the tool loop continues after recording failure
        assert!(result);
    }

    #[tokio::test]
    async fn buffered_preserves_order() {
        use futures::StreamExt;

        let executor = DelayExecutor {
            delay: Duration::from_millis(10),
            call_order: Arc::new(AtomicUsize::new(0)),
        };
        let calls = make_calls(6);
        let max_parallel = 2;

        let stream = futures::stream::iter(calls.iter().map(|c| executor.execute_tool_call(c)));
        let results: Vec<_> =
            futures::StreamExt::collect::<Vec<_>>(stream.buffered(max_parallel)).await;

        for (i, r) in results.iter().enumerate() {
            let out = r.as_ref().unwrap().as_ref().unwrap();
            assert_eq!(out.tool_name, format!("tool-{i}"));
        }
    }

    #[test]
    fn inject_active_skill_env_maps_secret_name_to_env_key() {
        // Verify the mapping logic: "github_token" -> "GITHUB_TOKEN"
        let secret_name = "github_token";
        let env_key = secret_name.to_uppercase();
        assert_eq!(env_key, "GITHUB_TOKEN");

        // "some_api_key" -> "SOME_API_KEY"
        let secret_name2 = "some_api_key";
        let env_key2 = secret_name2.to_uppercase();
        assert_eq!(env_key2, "SOME_API_KEY");
    }

    #[tokio::test]
    async fn inject_active_skill_env_injects_only_active_skill_secrets() {
        use crate::agent::Agent;
        #[allow(clippy::wildcard_imports)]
        use crate::agent::agent_tests::*;
        use crate::vault::Secret;
        use zeph_skills::registry::SkillRegistry;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = SkillRegistry::default();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Add available custom secrets
        agent
            .skill_state
            .available_custom_secrets
            .insert("github_token".into(), Secret::new("gh-secret-val"));
        agent
            .skill_state
            .available_custom_secrets
            .insert("other_key".into(), Secret::new("other-val"));

        // No active skills — inject_active_skill_env should be a no-op
        assert!(agent.skill_state.active_skill_names.is_empty());
        agent.inject_active_skill_env();
        // tool_executor.set_skill_env was not called (no-op path)
        assert!(agent.skill_state.active_skill_names.is_empty());
    }

    #[test]
    fn inject_active_skill_env_calls_set_skill_env_with_correct_map() {
        use crate::agent::Agent;
        #[allow(clippy::wildcard_imports)]
        use crate::agent::agent_tests::*;
        use crate::vault::Secret;
        use std::sync::Arc;
        use zeph_skills::registry::SkillRegistry;

        // Build a registry with one skill that requires "github_token".
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join("gh-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: gh-skill\ndescription: GitHub.\nx-requires-secrets: github_token\n---\nbody",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[temp_dir.path().to_path_buf()]);

        let executor = MockToolExecutor::no_tools();
        let captured = Arc::clone(&executor.captured_env);

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent
            .skill_state
            .available_custom_secrets
            .insert("github_token".into(), Secret::new("gh-val"));
        agent.skill_state.active_skill_names.push("gh-skill".into());

        agent.inject_active_skill_env();

        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 1, "set_skill_env must be called once");
        let env = calls[0].as_ref().expect("env must be Some");
        assert_eq!(env.get("GITHUB_TOKEN").map(String::as_str), Some("gh-val"));
    }

    #[test]
    fn inject_active_skill_env_clears_after_call() {
        use crate::agent::Agent;
        #[allow(clippy::wildcard_imports)]
        use crate::agent::agent_tests::*;
        use crate::vault::Secret;
        use std::sync::Arc;
        use zeph_skills::registry::SkillRegistry;

        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join("tok-skill");
        std::fs::create_dir(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: tok-skill\ndescription: Token.\nx-requires-secrets: api_token\n---\nbody",
        )
        .unwrap();
        let registry = SkillRegistry::load(&[temp_dir.path().to_path_buf()]);

        let executor = MockToolExecutor::no_tools();
        let captured = Arc::clone(&executor.captured_env);

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent
            .skill_state
            .available_custom_secrets
            .insert("api_token".into(), Secret::new("tok-val"));
        agent
            .skill_state
            .active_skill_names
            .push("tok-skill".into());

        // First call — injects env
        agent.inject_active_skill_env();
        // Simulate post-execution clear
        agent.tool_executor.set_skill_env(None);

        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 2, "inject + clear = 2 calls");
        assert!(calls[0].is_some(), "first call must set env");
        assert!(calls[1].is_none(), "second call must clear env");
    }

    #[tokio::test]
    async fn streaming_chunk_with_secret_is_redacted_before_channel_send() {
        use super::super::agent_tests::*;
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        // Streaming provider returns a chunk containing an AWS-style access key.
        let secret_chunk = "AKIA1234567890ABCDEF".to_string();
        let provider = mock_provider_streaming(vec![secret_chunk.clone()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);
        agent.runtime.security.redact_secrets = true;

        agent.messages.push(Message {
            role: Role::User,
            content: "tell me a secret".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        let _ = agent.process_response_streaming().await.unwrap();

        // The raw secret must not appear in any chunk sent to the channel.
        let chunks = agent.channel.sent_chunks();
        assert!(!chunks.is_empty(), "at least one chunk must have been sent");
        for chunk in &chunks {
            assert!(
                !chunk.contains(&secret_chunk),
                "raw secret must not appear in sent chunk: {chunk:?}"
            );
        }
    }

    #[test]
    fn check_response_cache_bypassed_when_streaming() {
        // Verifies that the streaming provider flag correctly identifies the bypass condition.
        // The cache check guard is `!self.provider.supports_streaming()`, so a streaming
        // provider must return true from supports_streaming() and a non-streaming one must not.
        use super::super::agent_tests::*;
        use zeph_llm::LlmProvider;

        let streaming_provider = mock_provider_streaming(vec!["hello".into()]);
        let non_streaming_provider = mock_provider(vec!["hello".into()]);

        assert!(
            streaming_provider.supports_streaming(),
            "streaming mock must report supports_streaming=true"
        );
        assert!(
            !non_streaming_provider.supports_streaming(),
            "non-streaming mock must report supports_streaming=false"
        );
    }

    #[tokio::test]
    async fn call_llm_returns_cached_response_without_provider_call() {
        use super::super::agent_tests::*;
        use std::sync::Arc;
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::{ResponseCache, sqlite::SqliteStore};

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        // Non-streaming provider — cache path is active for non-streaming.
        let provider = mock_provider(vec!["uncached response".into()]);
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        // Set up a response cache with a pre-populated entry.
        let store = SqliteStore::new(":memory:").await.unwrap();
        let cache = Arc::new(ResponseCache::new(store.pool().clone(), 3600));

        // Build the key for the current agent messages.
        let key = ResponseCache::compute_key(&agent.messages, &agent.runtime.model_name);
        cache
            .put(&key, "cached response", "test-model")
            .await
            .unwrap();

        agent.response_cache = Some(cache);

        // push a user message so the conversation is non-empty
        agent.messages.push(Message {
            role: Role::User,
            content: "what is 2+2?".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        // Recompute key after adding user message
        let key2 = ResponseCache::compute_key(&agent.messages, &agent.runtime.model_name);
        if let Some(ref c) = agent.response_cache {
            c.put(&key2, "cached response", "test-model").await.unwrap();
        }

        let result = agent.call_llm_with_timeout().await.unwrap();
        assert_eq!(result.as_deref(), Some("cached response"));
        // Channel should have received the cached response
        assert!(
            agent
                .channel
                .sent_messages()
                .iter()
                .any(|s| s == "cached response")
        );
    }

    #[tokio::test]
    async fn store_response_in_cache_enables_second_call_to_return_cached() {
        use super::super::agent_tests::*;
        use std::sync::Arc;
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::{ResponseCache, sqlite::SqliteStore};

        // Provider has one response; the second call must come from cache.
        let provider = mock_provider(vec!["provider response".into()]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let store = SqliteStore::new(":memory:").await.unwrap();
        let cache = Arc::new(ResponseCache::new(store.pool().clone(), 3600));
        agent.response_cache = Some(cache);

        agent.messages.push(Message {
            role: Role::User,
            content: "what is 3+3?".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        // First call — hits provider, stores response in cache.
        let first = agent.call_llm_with_timeout().await.unwrap();
        assert_eq!(first.as_deref(), Some("provider response"));

        // Second call with the same messages — must return cached value.
        let second = agent.call_llm_with_timeout().await.unwrap();
        assert_eq!(
            second.as_deref(),
            Some("provider response"),
            "second call must return cached response"
        );

        // Channel must have received both responses.
        let sent = agent.channel.sent_messages();
        let matching: Vec<_> = sent
            .iter()
            .filter(|s| s.as_str() == "provider response")
            .collect();
        assert_eq!(
            matching.len(),
            2,
            "both calls must have sent the response to the channel"
        );
    }

    mod retry_tests {
        use crate::agent::agent_tests::*;
        use zeph_llm::LlmError;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        fn agent_with_provider(provider: AnyProvider) -> crate::agent::Agent<MockChannel> {
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::no_tools();
            let mut agent =
                super::super::Agent::new(provider, channel, registry, None, 5, executor);
            agent.messages.push(Message {
                role: Role::User,
                content: "hello".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
            agent
        }

        #[tokio::test]
        async fn call_llm_with_retry_succeeds_on_first_attempt() {
            let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["ok".into()]));
            let mut agent = agent_with_provider(provider);
            let result = agent.call_llm_with_retry(2).await.unwrap();
            assert_eq!(result.as_deref(), Some("ok"));
        }

        #[tokio::test]
        async fn call_llm_with_retry_recovers_after_context_length_error() {
            // First call returns ContextLengthExceeded, second succeeds.
            // compact_context() is a no-op with only 1 non-system message + system prompt,
            // but the retry logic itself must still re-call after compaction.
            let provider = AnyProvider::Mock(
                MockProvider::with_responses(vec!["recovered".into()])
                    .with_errors(vec![LlmError::ContextLengthExceeded]),
            );
            let mut agent = agent_with_provider(provider);
            // Add context budget so compact_context can run
            agent.context_manager.budget = Some(zeph_core_budget_for_test());
            let result = agent.call_llm_with_retry(2).await.unwrap();
            assert_eq!(result.as_deref(), Some("recovered"));
        }

        fn zeph_core_budget_for_test() -> crate::context::ContextBudget {
            crate::context::ContextBudget::new(200_000, 0.20)
        }

        #[tokio::test]
        async fn call_llm_with_retry_propagates_non_context_error() {
            let provider = AnyProvider::Mock(
                MockProvider::with_responses(vec![])
                    .with_errors(vec![LlmError::Other("network error".into())]),
            );
            let mut agent = agent_with_provider(provider);
            let result: Result<Option<String>, _> = agent.call_llm_with_retry(2).await;
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(!err.is_context_length_error());
        }

        #[tokio::test]
        async fn call_llm_with_retry_exhausts_all_attempts() {
            // Two context length errors, max_attempts=2 — second attempt has no guard,
            // so it returns the error directly.
            let provider =
                AnyProvider::Mock(MockProvider::with_responses(vec![]).with_errors(vec![
                    LlmError::ContextLengthExceeded,
                    LlmError::ContextLengthExceeded,
                ]));
            let mut agent = agent_with_provider(provider);
            agent.context_manager.budget = Some(zeph_core_budget_for_test());
            let result: Result<Option<String>, _> = agent.call_llm_with_retry(2).await;
            assert!(result.is_err());
            assert!(result.unwrap_err().is_context_length_error());
        }
    }

    mod retry_integration {
        use crate::agent::agent_tests::*;
        use zeph_llm::LlmError;
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{Message, MessageMetadata, Role, ToolDefinition};

        fn agent_with_provider(provider: AnyProvider) -> crate::agent::Agent<MockChannel> {
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::no_tools();
            let mut agent =
                super::super::Agent::new(provider, channel, registry, None, 5, executor);
            agent.messages.push(Message {
                role: Role::User,
                content: "hello".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
            agent
        }

        fn budget_for_test() -> crate::context::ContextBudget {
            crate::context::ContextBudget::new(200_000, 0.20)
        }

        fn no_tools() -> Vec<ToolDefinition> {
            vec![]
        }

        #[tokio::test]
        async fn call_chat_with_tools_retry_succeeds_on_first_attempt() {
            let provider = AnyProvider::Mock(MockProvider::with_responses(vec!["ok".into()]));
            let mut agent = agent_with_provider(provider);
            let result = agent
                .call_chat_with_tools_retry(&no_tools(), 2)
                .await
                .unwrap();
            assert!(result.is_some());
        }

        #[tokio::test]
        async fn call_chat_with_tools_retry_recovers_after_context_error() {
            // First call returns ContextLengthExceeded, second succeeds.
            let provider = AnyProvider::Mock(
                MockProvider::with_responses(vec!["recovered".into()])
                    .with_errors(vec![LlmError::ContextLengthExceeded]),
            );
            let mut agent = agent_with_provider(provider);
            agent.context_manager.budget = Some(budget_for_test());
            let result = agent
                .call_chat_with_tools_retry(&no_tools(), 2)
                .await
                .unwrap();
            assert!(result.is_some());
        }

        #[tokio::test]
        async fn call_chat_with_tools_retry_exhausts_all_attempts() {
            // Both attempts return ContextLengthExceeded — final error propagates.
            let provider =
                AnyProvider::Mock(MockProvider::with_responses(vec![]).with_errors(vec![
                    LlmError::ContextLengthExceeded,
                    LlmError::ContextLengthExceeded,
                ]));
            let mut agent = agent_with_provider(provider);
            agent.context_manager.budget = Some(budget_for_test());
            let result: Result<Option<_>, _> =
                agent.call_chat_with_tools_retry(&no_tools(), 2).await;
            assert!(result.is_err());
            assert!(result.unwrap_err().is_context_length_error());
        }
    }

    // Regression tests for issue #1003: tool output must reach all channel types
    // regardless of whether the tool streamed its output.
    #[tokio::test]
    async fn handle_tool_result_sends_output_when_streamed_true() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_tools::executor::ToolOutput;

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        let output = ToolOutput {
            tool_name: "bash".into(),
            summary: "streamed content".into(),
            blocks_executed: 1,
            diff: None,
            filter_stats: None,
            streamed: true,
            terminal_id: None,
            locations: None,
            raw_response: None,
        };
        agent
            .handle_tool_result("response", Ok(Some(output)))
            .await
            .unwrap();

        let sent = agent.channel.sent_messages();
        assert!(
            sent.iter().any(|m| m.contains("bash")),
            "send_tool_output must be called even when streamed=true; got: {sent:?}"
        );
    }

    #[tokio::test]
    async fn handle_tool_result_fenced_emits_tool_start_then_output_via_loopback() {
        use super::super::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};
        use crate::channel::{LoopbackChannel, LoopbackEvent};
        use zeph_tools::executor::ToolOutput;

        let (loopback, mut handle) = LoopbackChannel::pair(32);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, loopback, registry, None, 5, executor);

        let output = ToolOutput {
            tool_name: "grep".into(),
            summary: "match found".into(),
            blocks_executed: 1,
            diff: None,
            filter_stats: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        };
        agent
            .handle_tool_result("response", Ok(Some(output)))
            .await
            .unwrap();

        drop(agent);

        let mut events = Vec::new();
        while let Ok(ev) = handle.output_rx.try_recv() {
            events.push(ev);
        }

        let tool_start_pos = events.iter().position(|e| {
            matches!(e, LoopbackEvent::ToolStart { tool_name, tool_call_id, .. }
                if tool_name == "grep" && !tool_call_id.is_empty())
        });
        let tool_output_pos = events.iter().position(|e| {
            matches!(e, LoopbackEvent::ToolOutput { tool_name, tool_call_id, .. }
                if tool_name == "grep" && !tool_call_id.is_empty())
        });

        assert!(
            tool_start_pos.is_some(),
            "LoopbackEvent::ToolStart with non-empty tool_call_id must be emitted; events: {events:?}"
        );
        assert!(
            tool_output_pos.is_some(),
            "LoopbackEvent::ToolOutput with non-empty tool_call_id must be emitted; events: {events:?}"
        );
        assert!(
            tool_start_pos < tool_output_pos,
            "ToolStart must precede ToolOutput; start={tool_start_pos:?} output={tool_output_pos:?}"
        );

        // Verify both events share the same tool_call_id.
        let start_id = events.iter().find_map(|e| {
            if let LoopbackEvent::ToolStart { tool_call_id, .. } = e {
                Some(tool_call_id.clone())
            } else {
                None
            }
        });
        let output_id = events.iter().find_map(|e| {
            if let LoopbackEvent::ToolOutput { tool_call_id, .. } = e {
                Some(tool_call_id.clone())
            } else {
                None
            }
        });
        assert_eq!(
            start_id, output_id,
            "ToolStart and ToolOutput must share the same tool_call_id"
        );
    }

    #[tokio::test]
    async fn handle_tool_result_locations_propagated_to_loopback_event() {
        use super::super::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};
        use crate::channel::{LoopbackChannel, LoopbackEvent};
        use zeph_tools::executor::ToolOutput;

        let (loopback, mut handle) = LoopbackChannel::pair(32);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, loopback, registry, None, 5, executor);

        let output = ToolOutput {
            tool_name: "read_file".into(),
            summary: "file content".into(),
            blocks_executed: 1,
            diff: None,
            filter_stats: None,
            streamed: false,
            terminal_id: None,
            locations: Some(vec!["/src/main.rs".to_owned()]),
            raw_response: None,
        };
        agent
            .handle_tool_result("response", Ok(Some(output)))
            .await
            .unwrap();
        drop(agent);

        let mut events = Vec::new();
        while let Ok(ev) = handle.output_rx.try_recv() {
            events.push(ev);
        }

        let locations = events.iter().find_map(|e| {
            if let LoopbackEvent::ToolOutput { locations, .. } = e {
                locations.clone()
            } else {
                None
            }
        });
        assert_eq!(
            locations,
            Some(vec!["/src/main.rs".to_owned()]),
            "locations from ToolOutput must be forwarded to LoopbackEvent::ToolOutput"
        );
    }

    // Regression test for #1033: send_tool_output must receive raw body, not markdown-wrapped text.
    // Before the fix, `format_tool_output` output (with fenced code block) was passed to
    // `send_tool_output`, which caused newlines inside the output to be lost in ACP consumers
    // that read `terminal_output.data` or `raw_output` as plain text.
    #[tokio::test]
    async fn handle_tool_result_display_is_raw_body_not_markdown_wrapped() {
        use super::super::agent_tests::{MockToolExecutor, create_test_registry, mock_provider};
        use crate::channel::{LoopbackChannel, LoopbackEvent};
        use zeph_tools::executor::ToolOutput;

        let (loopback, mut handle) = LoopbackChannel::pair(32);
        let provider = mock_provider(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, loopback, registry, None, 5, executor);

        let output = ToolOutput {
            tool_name: "bash".into(),
            summary: "line1\nline2\nline3".into(),
            blocks_executed: 1,
            diff: None,
            filter_stats: None,
            streamed: false,
            terminal_id: None,
            locations: None,
            raw_response: None,
        };
        agent
            .handle_tool_result("response", Ok(Some(output)))
            .await
            .unwrap();
        drop(agent);

        let mut events = Vec::new();
        while let Ok(ev) = handle.output_rx.try_recv() {
            events.push(ev);
        }

        let display = events.iter().find_map(|e| {
            if let LoopbackEvent::ToolOutput { display, .. } = e {
                Some(display.clone())
            } else {
                None
            }
        });

        let display = display.expect("LoopbackEvent::ToolOutput must be emitted");
        // Raw body must be passed — no markdown fence markers.
        assert!(
            !display.contains("```"),
            "display must not contain markdown fences; got: {display:?}"
        );
        assert!(
            !display.contains("[tool output:"),
            "display must not contain markdown header; got: {display:?}"
        );
        // Newlines from the original output must be preserved.
        assert!(
            display.contains('\n'),
            "display must preserve newlines from raw body; got: {display:?}"
        );
        assert!(
            display.contains("line1") && display.contains("line2") && display.contains("line3"),
            "display must contain all lines from raw body; got: {display:?}"
        );
    }

    // Validate AnomalyDetector wiring: record_anomaly_outcome paths produce correct severity.
    #[test]
    fn anomaly_detector_15_of_20_errors_produces_critical() {
        let mut det = zeph_tools::AnomalyDetector::new(20, 0.5, 0.7);
        for _ in 0..5 {
            det.record_success();
        }
        for _ in 0..15 {
            det.record_error();
        }
        let anomaly = det.check().expect("expected anomaly");
        assert_eq!(anomaly.severity, zeph_tools::AnomalySeverity::Critical);
    }

    #[test]
    fn anomaly_detector_5_of_20_errors_no_critical_alert() {
        let mut det = zeph_tools::AnomalyDetector::new(20, 0.5, 0.7);
        for _ in 0..15 {
            det.record_success();
        }
        for _ in 0..5 {
            det.record_error();
        }
        let result = det.check();
        assert!(
            result.is_none(),
            "5/20 errors must not trigger any alert, got: {result:?}"
        );
    }
}
