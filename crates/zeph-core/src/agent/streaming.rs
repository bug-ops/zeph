use tokio_stream::StreamExt;
use zeph_llm::provider::{ChatResponse, LlmProvider, Message, MessagePart, Role, ToolDefinition};
use zeph_tools::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};

use crate::channel::Channel;
use crate::redact::redact_secrets;
use zeph_memory::semantic::estimate_tokens;

use super::{Agent, DOOM_LOOP_WINDOW, TOOL_LOOP_KEEP_RECENT, format_tool_output};
use tracing::Instrument;

impl<C: Channel, T: ToolExecutor> Agent<C, T> {
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
            self.channel.send_typing().await?;

            // Context budget check at 80% threshold
            if let Some(ref budget) = self.context_state.budget {
                let used: usize = self
                    .messages
                    .iter()
                    .map(|m| estimate_tokens(&m.content))
                    .sum();
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

            let result = self
                .tool_executor
                .execute(&response)
                .instrument(tracing::info_span!("tool_exec"))
                .await;
            if !self.handle_tool_result(&response, result).await? {
                return Ok(());
            }

            // Prune tool output bodies from older iterations to reduce context growth
            self.prune_stale_tool_outputs(TOOL_LOOP_KEEP_RECENT);

            // Doom-loop detection: compare last N outputs by string equality
            if let Some(last_msg) = self.messages.last() {
                self.doom_loop_history.push(last_msg.content.clone());
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

    pub(crate) async fn call_llm_with_timeout(
        &mut self,
    ) -> Result<Option<String>, super::error::AgentError> {
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
            if let Ok(r) = tokio::time::timeout(
                llm_timeout,
                self.process_response_streaming().instrument(llm_span),
            )
            .await
            {
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
                Ok(Some(r?))
            } else {
                self.channel
                    .send("LLM request timed out. Please try again.")
                    .await?;
                Ok(None)
            }
        } else {
            match tokio::time::timeout(
                llm_timeout,
                self.provider.chat(&self.messages).instrument(llm_span),
            )
            .await
            {
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

    pub(crate) fn last_user_query(&self) -> &str {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User && !m.content.starts_with("[tool output"))
            .map_or("", |m| m.content.as_str())
    }

    pub(crate) async fn summarize_tool_output(&self, output: &str) -> String {
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

    pub(crate) async fn maybe_summarize_tool_output(&self, output: &str) -> String {
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
    pub(crate) async fn handle_tool_result(
        &mut self,
        response: &str,
        result: Result<Option<ToolOutput>, ToolError>,
    ) -> Result<bool, super::error::AgentError> {
        match result {
            Ok(Some(output)) => {
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
                let formatted_output = format_tool_output(&output.tool_name, &processed);
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
                    if let Ok(Some(out)) = self.tool_executor.execute_confirmed(response).await {
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

    pub(crate) async fn process_response_streaming(
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
            };
            let chunk: String = chunk_result?;
            response.push_str(&chunk);
            let display = self.maybe_redact(&chunk);
            self.channel.send_chunk(&display).await?;
        }

        self.channel.flush_chunks().await?;

        let completion_estimate = u64::try_from(response.len()).unwrap_or(0) / 4;
        self.update_metrics(|m| {
            m.completion_tokens += completion_estimate;
            m.total_tokens = m.prompt_tokens + m.completion_tokens;
        });

        Ok(response)
    }

    pub(crate) fn maybe_redact<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
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
            .tool_definitions()
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

            self.channel.send_typing().await?;

            if let Some(ref budget) = self.context_state.budget {
                let used: usize = self
                    .messages
                    .iter()
                    .map(|m| estimate_tokens(&m.content))
                    .sum();
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
        let result = if let Ok(result) = tokio::time::timeout(
            llm_timeout,
            self.provider
                .chat_with_tools(&self.messages, tool_defs)
                .instrument(llm_span),
        )
        .await
        {
            result?
        } else {
            self.channel
                .send("LLM request timed out. Please try again.")
                .await?;
            return Ok(None);
        };

        let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.update_metrics(|m| {
            m.api_calls += 1;
            m.last_llm_latency_ms = latency;
        });
        self.record_cache_usage();

        Ok(Some(result))
    }

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

        let mut result_parts: Vec<MessagePart> = Vec::new();
        for tc in tool_calls {
            let params: std::collections::HashMap<String, serde_json::Value> =
                if let serde_json::Value::Object(map) = &tc.input {
                    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                } else {
                    std::collections::HashMap::new()
                };

            let call = ToolCall {
                tool_id: tc.name.clone(),
                params,
            };

            let tool_result = self
                .tool_executor
                .execute_tool_call(&call)
                .instrument(tracing::info_span!("tool_exec", tool_name = %tc.name))
                .await;
            let (output, is_error) = match tool_result {
                Ok(Some(out)) => (out.summary, false),
                Ok(None) => ("(no output)".to_owned(), false),
                Err(e) => (format!("[error] {e}"), true),
            };

            let processed = self.maybe_summarize_tool_output(&output).await;
            let formatted = format_tool_output(&tc.name, &processed);
            let display = self.maybe_redact(&formatted);
            self.channel.send(&display).await?;

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

    /// Returns `true` if a doom loop was detected and the caller should break.
    async fn check_doom_loop(
        &mut self,
        iteration: usize,
    ) -> Result<bool, super::error::AgentError> {
        if let Some(last_msg) = self.messages.last() {
            self.doom_loop_history.push(last_msg.content.clone());
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
    ToolDefinition {
        name: def.id.to_string(),
        description: def.description.to_string(),
        parameters: serde_json::to_value(&def.schema).unwrap_or_default(),
    }
}
