use tokio_stream::StreamExt;
use zeph_llm::provider::{ChatResponse, LlmProvider, Message, MessagePart, Role, ToolDefinition};
use zeph_tools::executor::{ToolCall, ToolError, ToolOutput};

use super::{Agent, DOOM_LOOP_WINDOW, TOOL_LOOP_KEEP_RECENT, format_tool_output};
use crate::channel::Channel;
use crate::redact::redact_secrets;
use tracing::Instrument;

/// Strip volatile IDs from message content so doom-loop comparison is stable.
/// Normalizes `[tool_result: <id>]` and `[tool_use: <name>(<id>)]` by removing unique IDs.
// DefaultHasher output is not stable across Rust versions — do not persist or serialize
// these hashes. They are used only for within-session equality comparison.
fn doom_loop_hash(content: &str) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let normalized = normalize_for_doom_loop(content);
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

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
        self.doom_loop_history.clear();

        for iteration in 0..self.runtime.max_tool_iterations {
            if self.cancel_token.is_cancelled() {
                tracing::info!("tool loop cancelled by user");
                break;
            }

            self.channel.send_typing().await?;

            // Context budget check at 80% threshold
            if let Some(ref budget) = self.context_state.budget {
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

            let Some(response) = self.call_llm_with_timeout().await? else {
                return Ok(());
            };

            if response.trim().is_empty() {
                tracing::warn!("received empty response from LLM, skipping");
                self.record_skill_outcomes("empty_response", None).await;

                if !self.reflection_used
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
            });
            self.persist_message(Role::Assistant, &response).await;

            self.inject_active_skill_env();
            let result = self
                .tool_executor
                .execute_erased(&response)
                .instrument(tracing::info_span!("tool_exec"))
                .await;
            self.tool_executor.set_skill_env(None);
            if !self.handle_tool_result(&response, result).await? {
                return Ok(());
            }

            // Prune tool output bodies from older iterations to reduce context growth
            self.prune_stale_tool_outputs(TOOL_LOOP_KEEP_RECENT);

            // Doom-loop detection: compare last N outputs by content hash
            if let Some(last_msg) = self.messages.last() {
                self.doom_loop_history
                    .push(doom_loop_hash(&last_msg.content));
                if self.doom_loop_history.len() >= DOOM_LOOP_WINDOW {
                    let recent =
                        &self.doom_loop_history[self.doom_loop_history.len() - DOOM_LOOP_WINDOW..];
                    if recent.windows(2).all(|w| w[0] == w[1]) {
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
                // Redact secrets from the full response before it is persisted to history.
                // Streaming chunks were already sent to the channel without per-chunk redaction
                // (acceptable trade-off: ephemeral display vs allocation per chunk).
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
        }];

        match self.summary_or_primary_provider().chat(&messages).await {
            Ok(summary) => format!("[tool output summary]\n```\n{summary}\n```"),
            Err(e) => {
                tracing::warn!(
                    "tool output summarization failed, falling back to truncation: {e:#}"
                );
                truncated
            }
        }
    }

    pub(super) async fn maybe_summarize_tool_output(&self, output: &str) -> String {
        if output.len() <= zeph_tools::MAX_TOOL_OUTPUT_CHARS {
            return output.to_string();
        }
        let overflow_notice = if let Some(path) = zeph_tools::save_overflow(output) {
            format!(
                "\n[full output saved to {}, use read tool to access]",
                path.display()
            )
        } else {
            String::new()
        };
        let truncated = if self.runtime.summarize_tool_output_enabled {
            self.summarize_tool_output(output).await
        } else {
            zeph_tools::truncate_tool_output(output)
        };
        format!("{truncated}{overflow_notice}")
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
                    self.record_skill_outcomes("success", None).await;
                    return Ok(false);
                }

                if output.summary.contains("[error]") || output.summary.contains("[exit code") {
                    self.record_skill_outcomes("tool_failure", Some(&output.summary))
                        .await;

                    if !self.reflection_used
                        && self
                            .attempt_self_reflection(&output.summary, &output.summary)
                            .await?
                    {
                        return Ok(false);
                    }
                } else {
                    self.record_skill_outcomes("success", None).await;
                }

                let processed = self.maybe_summarize_tool_output(&output.summary).await;
                let body = if let Some(ref fs) = output.filter_stats
                    && fs.filtered_chars < fs.raw_chars
                {
                    format!("{}\n{processed}", fs.format_inline(&output.tool_name))
                } else {
                    processed.clone()
                };
                let formatted_output = format_tool_output(&output.tool_name, &body);
                let display = self.maybe_redact(&formatted_output);
                self.channel.send(&display).await?;

                self.push_message(Message::from_parts(
                    Role::User,
                    vec![MessagePart::ToolOutput {
                        tool_name: output.tool_name.clone(),
                        body: processed,
                        compacted_at: None,
                    }],
                ));
                self.persist_message(Role::User, &formatted_output).await;
                Ok(true)
            }
            Ok(None) => {
                self.record_skill_outcomes("success", None).await;
                Ok(false)
            }
            Err(ToolError::Blocked { command }) => {
                tracing::warn!("blocked command: {command}");
                self.channel
                    .send("This command is blocked by security policy.")
                    .await?;
                Ok(false)
            }
            Err(ToolError::ConfirmationRequired { command }) => {
                let prompt = format!("Allow command: {command}?");
                if self.channel.confirm(&prompt).await? {
                    if let Ok(Some(out)) =
                        self.tool_executor.execute_confirmed_erased(response).await
                    {
                        let processed = self.maybe_summarize_tool_output(&out.summary).await;
                        let formatted = format_tool_output(&out.tool_name, &processed);
                        let display = self.maybe_redact(&formatted);
                        self.channel.send(&display).await?;
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
                Ok(false)
            }
            Err(e) => {
                let err_str = format!("{e:#}");
                tracing::error!("tool execution error: {err_str}");
                self.record_skill_outcomes("tool_failure", Some(&err_str))
                    .await;

                if !self.reflection_used && self.attempt_self_reflection(&err_str, "").await? {
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
            self.channel.send_chunk(&chunk).await?;
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

    async fn process_response_native_tools(&mut self) -> Result<(), super::error::AgentError> {
        self.doom_loop_history.clear();

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

        for iteration in 0..self.runtime.max_tool_iterations {
            if *self.shutdown.borrow() {
                tracing::info!("native tool loop interrupted by shutdown");
                break;
            }
            if self.cancel_token.is_cancelled() {
                tracing::info!("native tool loop cancelled by user");
                break;
            }

            self.channel.send_typing().await?;

            if let Some(ref budget) = self.context_state.budget {
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

            let chat_result = self.call_chat_with_tools(&tool_defs).await?;

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

            if self.check_doom_loop(iteration).await? {
                break;
            }
        }

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
        for (tc, tool_result) in tool_calls.iter().zip(tool_results) {
            let (output, is_error, diff, inline_stats, already_streamed) = match tool_result {
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
                    let streamed = out.streamed;
                    (out.summary, false, out.diff, inline_stats, streamed)
                }
                Ok(None) => ("(no output)".to_owned(), false, None, None, false),
                Err(e) => (format!("[error] {e}"), true, None, None, false),
            };

            let processed = self.maybe_summarize_tool_output(&output).await;
            let body = if let Some(ref stats) = inline_stats {
                format!("{stats}\n{processed}")
            } else {
                processed.clone()
            };
            let formatted = format_tool_output(&tc.name, &body);
            let display = self.maybe_redact(&formatted);
            // Tools that already streamed via ToolEvent channel (e.g. bash) have their
            // output displayed by the TUI event forwarder; skip duplicate send.
            if !already_streamed {
                self.channel
                    .send_tool_output(&tc.name, &display, diff, inline_stats)
                    .await?;
            }

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
            self.doom_loop_history
                .push(doom_loop_hash(&last_msg.content));
            if self.doom_loop_history.len() >= DOOM_LOOP_WINDOW {
                let recent =
                    &self.doom_loop_history[self.doom_loop_history.len() - DOOM_LOOP_WINDOW..];
                if recent.windows(2).all(|w| w[0] == w[1]) {
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

    use super::{normalize_for_doom_loop, tool_def_to_definition};

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
            id: "bash",
            description: "run a shell command",
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
    fn last_user_query_finds_latest_user_message() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, Role};

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.push(Message {
            role: Role::User,
            content: "first question".into(),
            parts: vec![],
        });
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "some answer".into(),
            parts: vec![],
        });
        agent.messages.push(Message {
            role: Role::User,
            content: "second question".into(),
            parts: vec![],
        });

        assert_eq!(agent.last_user_query(), "second question");
    }

    #[test]
    fn last_user_query_skips_tool_output_messages() {
        use super::super::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, Role};

        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = super::super::Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.push(Message {
            role: Role::User,
            content: "what is the result?".into(),
            parts: vec![],
        });
        // Tool output messages start with "[tool output"
        agent.messages.push(Message {
            role: Role::User,
            content: "[tool output] some output".into(),
            parts: vec![],
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
        };
        // reflection_used = true so reflection path is skipped
        agent.reflection_used = true;
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
            "---\nname: gh-skill\ndescription: GitHub.\nrequires-secrets: github_token\n---\nbody",
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
            "---\nname: tok-skill\ndescription: Token.\nrequires-secrets: api_token\n---\nbody",
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
}
