use tokio_stream::StreamExt;
use zeph_llm::provider::{LlmProvider, Message, MessagePart, Role};
use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

use crate::channel::Channel;
use crate::redact::redact_secrets;
use zeph_memory::semantic::estimate_tokens;

use super::{Agent, DOOM_LOOP_WINDOW, format_tool_output};

impl<P: LlmProvider + Clone + 'static, C: Channel, T: ToolExecutor> Agent<P, C, T> {
    pub(crate) async fn process_response(&mut self) -> Result<(), super::error::AgentError> {
        self.doom_loop_history.clear();

        for iteration in 0..self.max_tool_iterations {
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

                #[cfg(feature = "self-learning")]
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

            self.messages.push(Message {
                role: Role::Assistant,
                content: response.clone(),
                parts: vec![],
            });
            self.persist_message(Role::Assistant, &response).await;

            let result = self.tool_executor.execute(&response).await;
            if !self.handle_tool_result(&response, result).await? {
                return Ok(());
            }

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
        let llm_timeout = std::time::Duration::from_secs(self.timeouts.llm_seconds);
        let start = std::time::Instant::now();
        let prompt_estimate: u64 = self
            .messages
            .iter()
            .map(|m| u64::try_from(m.content.len()).unwrap_or(0) / 4)
            .sum();

        if self.provider.supports_streaming() {
            if let Ok(r) =
                tokio::time::timeout(llm_timeout, self.process_response_streaming()).await
            {
                let latency = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                self.update_metrics(|m| {
                    m.api_calls += 1;
                    m.last_llm_latency_ms = latency;
                    m.context_tokens = prompt_estimate;
                    m.prompt_tokens += prompt_estimate;
                    m.total_tokens = m.prompt_tokens + m.completion_tokens;
                });
                Ok(Some(r?))
            } else {
                self.channel
                    .send("LLM request timed out. Please try again.")
                    .await?;
                Ok(None)
            }
        } else {
            match tokio::time::timeout(llm_timeout, self.provider.chat(&self.messages)).await {
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

        match self.provider.chat(&messages).await {
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
        let truncated = if self.summarize_tool_output_enabled {
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

                    #[cfg(feature = "self-learning")]
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

                self.messages.push(Message::from_parts(
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
                        self.messages.push(Message::from_parts(
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

                #[cfg(feature = "self-learning")]
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

        while let Some(chunk_result) = stream.next().await {
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
        if self.security.redact_secrets {
            redact_secrets(text)
        } else {
            std::borrow::Cow::Borrowed(text)
        }
    }
}
