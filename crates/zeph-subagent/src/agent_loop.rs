// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{
    ChatResponse, LlmProvider, Message, MessageMetadata, MessagePart, Role, ToolDefinition,
};
use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};
use zeph_tools::executor::{ErasedToolExecutor, ToolCall};

use super::filter::FilteredToolExecutor;
use super::grants::{GrantedSecret, SecretRequest};
use super::hooks::{HookDef, SubagentHooks, fire_hooks, make_base_hook_env, matching_hooks};
use super::manager::SubAgentStatus;
use super::state::SubAgentState;
use super::transcript::TranscriptWriter;

const SECRET_REQUEST_PREFIX: &str = "[REQUEST_SECRET:";

enum SecretRequestOutcome {
    NotASecretRequest,
    Handled,
    Cancelled,
}

fn make_hook_env(
    task_id: &str,
    agent_name: &str,
    tool_name: &str,
    tool_input: &serde_json::Value,
) -> std::collections::HashMap<String, String> {
    let mut env = make_base_hook_env(tool_name, tool_input);
    env.insert("ZEPH_AGENT_ID".to_owned(), task_id.to_owned());
    env.insert("ZEPH_AGENT_NAME".to_owned(), agent_name.to_owned());
    env.insert("ZEPH_AGENT_TYPE".to_owned(), "subagent".to_owned());
    env
}

pub(super) struct AgentLoopArgs {
    pub(super) provider: AnyProvider,
    pub(super) executor: FilteredToolExecutor,
    pub(super) system_prompt: String,
    pub(super) task_prompt: String,
    pub(super) skills: Option<Vec<String>>,
    pub(super) max_turns: u32,
    /// Maximum number of messages retained in the in-memory history buffer.
    ///
    /// When the buffer exceeds this limit the oldest non-system messages are dropped
    /// from the front, keeping the system message (index 0) intact. This prevents
    /// LLM providers from rejecting requests once the model's context window is full.
    pub(super) max_history_messages: usize,
    pub(super) cancel: CancellationToken,
    pub(super) status_tx: watch::Sender<SubAgentStatus>,
    pub(super) started_at: Instant,
    pub(super) secret_request_tx: mpsc::Sender<SecretRequest>,
    pub(super) secret_rx: mpsc::Receiver<Option<GrantedSecret>>,
    pub(super) background: bool,
    pub(super) hooks: SubagentHooks,
    pub(super) task_id: String,
    pub(super) agent_name: String,
    pub(super) initial_messages: Vec<Message>,
    pub(super) transcript_writer: Option<TranscriptWriter>,
    pub(super) spawn_depth: u32,
    pub(super) mcp_tool_names: Vec<String>,
    pub(super) content_isolation: zeph_config::ContentIsolationConfig,
    /// Maximum wall time for a single LLM call inside an agent turn.
    pub(super) llm_timeout: std::time::Duration,
}

pub(super) fn make_message(role: Role, content: String) -> Message {
    Message {
        role,
        content,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }
}

#[tracing::instrument(name = "subagent.agent_loop.append_transcript", skip_all)]
pub(super) async fn append_transcript(
    writer: Option<&TranscriptWriter>,
    seq: &mut u32,
    msg: &Message,
) {
    if let Some(w) = writer {
        if let Err(e) = w.append(*seq, msg).await {
            tracing::warn!(error = %e, seq, "failed to write transcript entry");
        }
        *seq += 1;
    }
}

fn tool_def_to_definition(
    def: &zeph_tools::registry::ToolDef,
) -> zeph_llm::provider::ToolDefinition {
    let mut params = serde_json::to_value(&def.schema).unwrap_or_default();
    if let serde_json::Value::Object(ref mut map) = params {
        map.remove("$schema");
        map.remove("title");
    }
    zeph_llm::provider::ToolDefinition {
        name: def.id.to_string().into(),
        description: def.description.to_string(),
        parameters: params,
        output_schema: def.output_schema.clone(),
    }
}

fn build_effective_system_prompt(
    system_prompt: String,
    skills: Option<Vec<String>>,
    mcp_tool_names: &[String],
) -> String {
    let mut effective = if let Some(skill_bodies) = skills.filter(|s| !s.is_empty()) {
        let skill_block = skill_bodies.join("\n\n");
        format!("{system_prompt}\n\n```skills\n{skill_block}\n```")
    } else {
        system_prompt
    };

    if !mcp_tool_names.is_empty() {
        let mcp_annotation = format!(
            "\n\n## Available MCP Tools\n{}",
            mcp_tool_names
                .iter()
                .map(|n| format!("- {n}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        effective.push_str(&mcp_annotation);
    }

    effective
}

#[tracing::instrument(name = "subagent.agent_loop.call_provider", skip_all, err)]
async fn call_provider_with_status(
    provider: &AnyProvider,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    status_tx: &watch::Sender<SubAgentStatus>,
    turns: u32,
    started_at: Instant,
    llm_timeout: std::time::Duration,
) -> Result<ChatResponse, super::error::SubAgentError> {
    let llm_result =
        tokio::time::timeout(llm_timeout, provider.chat_with_tools(messages, tool_defs))
            .await
            .map_err(|_| {
                tracing::warn!(
                    timeout_secs = llm_timeout.as_secs(),
                    "sub-agent LLM call timed out"
                );
                super::error::SubAgentError::Llm("LLM call timed out".to_owned())
            })?;
    match llm_result {
        Ok(r) => Ok(r),
        Err(e) => {
            tracing::error!(error = %e, "sub-agent LLM call failed");
            let _ = status_tx.send(SubAgentStatus {
                state: SubAgentState::Failed,
                last_message: Some(e.to_string()),
                turns_used: turns,
                started_at,
            });
            Err(super::error::SubAgentError::Llm(e.to_string()))
        }
    }
}

fn emit_working_status(
    status_tx: &watch::Sender<SubAgentStatus>,
    response_text: &str,
    turns: u32,
    started_at: Instant,
) {
    let _ = status_tx.send(SubAgentStatus {
        state: SubAgentState::Working,
        last_message: Some(response_text.chars().take(120).collect()),
        turns_used: turns,
        started_at,
    });
}

#[tracing::instrument(name = "subagent.agent_loop.handle_secret_request", skip_all)]
#[allow(clippy::too_many_arguments)]
async fn handle_secret_request(
    transcript_writer: Option<&TranscriptWriter>,
    seq: &mut u32,
    messages: &mut Vec<Message>,
    granted_secrets: &mut HashMap<String, GrantedSecret>,
    secret_request_tx: &mpsc::Sender<SecretRequest>,
    secret_rx: &mut mpsc::Receiver<Option<GrantedSecret>>,
    cancel: &CancellationToken,
    background: bool,
    is_text_response: bool,
    response_text: &str,
) -> SecretRequestOutcome {
    if !is_text_response {
        return SecretRequestOutcome::NotASecretRequest;
    }
    let Some(rest) = response_text.strip_prefix(SECRET_REQUEST_PREFIX) else {
        return SecretRequestOutcome::NotASecretRequest;
    };

    let raw_key = rest.split(']').next().unwrap_or("").trim().to_owned();
    let key_name = if raw_key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !raw_key.is_empty()
        && raw_key.len() <= 100
    {
        raw_key
    } else {
        tracing::warn!("sub-agent emitted invalid secret key name — ignoring request");
        String::new()
    };

    if key_name.is_empty() {
        return SecretRequestOutcome::NotASecretRequest;
    }

    tracing::debug!("sub-agent requested secret [key redacted]");

    if background {
        tracing::warn!("background sub-agent secret request auto-denied (no interactive prompt)");
        let reply = format!("[secret:{key_name}] request denied");
        let assistant_msg = make_message(Role::Assistant, response_text.to_owned());
        let user_msg = make_message(Role::User, reply);
        append_transcript(transcript_writer, seq, &assistant_msg).await;
        append_transcript(transcript_writer, seq, &user_msg).await;
        messages.push(assistant_msg);
        messages.push(user_msg);
        return SecretRequestOutcome::Handled;
    }

    let req = SecretRequest {
        secret_key: key_name.clone(),
        reason: None,
    };
    if secret_request_tx.send(req).await.is_ok() {
        let outcome = tokio::select! {
            msg = secret_rx.recv() => msg,
            () = cancel.cancelled() => {
                tracing::debug!("sub-agent cancelled while waiting for secret approval");
                return SecretRequestOutcome::Cancelled;
            }
        };
        let reply = match outcome {
            Some(Some(granted)) => {
                // Accumulated for the loop's lifetime and attached to every subsequent tool
                // call's `ExecutionContext` (see `handle_tool_step`) — a per-call override,
                // never written into the shared `ShellExecutor::skill_env` slot, because that
                // slot is on the same executor instance the parent agent uses and would leak
                // the secret into unrelated tool calls made outside this sub-agent's run.
                // `handle_tool_step` re-checks `granted.is_expired()` before every tool call,
                // so the cached value stops being usable once its grant's TTL elapses.
                granted_secrets.insert(key_name.clone(), granted);
                format!(
                    "[secret:{key_name}] approved — available as ${key_name} in the tool \
                     execution environment"
                )
            }
            Some(None) | None => {
                format!("[secret:{key_name}] request denied")
            }
        };
        let assistant_msg = make_message(Role::Assistant, response_text.to_owned());
        let user_msg = make_message(Role::User, reply);
        append_transcript(transcript_writer, seq, &assistant_msg).await;
        append_transcript(transcript_writer, seq, &user_msg).await;
        messages.push(assistant_msg);
        messages.push(user_msg);
        return SecretRequestOutcome::Handled;
    }

    SecretRequestOutcome::NotASecretRequest
}

/// What the agent loop should do after a no-tool (text-only) response.
enum NoToolAction {
    /// Send nudge and continue the loop.
    Nudge,
    /// No nudge needed — break the loop.
    Break,
}

/// Handle the case where the LLM responded with plain text (no tool calls).
///
/// Appends new messages to the transcript, and optionally sends a one-time
/// nudge on the first turn when no tools have been called yet.
async fn handle_no_tool_response(
    transcript_writer: Option<&TranscriptWriter>,
    seq: &mut u32,
    messages: &[Message],
    prev_len: usize,
    turns: u32,
    any_tool_called: bool,
    nudge_messages: &mut Vec<Message>,
) -> NoToolAction {
    for msg in &messages[prev_len..] {
        append_transcript(transcript_writer, seq, msg).await;
    }
    if turns == 1 && !any_tool_called {
        tracing::debug!("sub-agent text-only first turn — sending nudge to use tools");
        let nudge = make_message(
            Role::User,
            "Please use the available tools to complete the task. \
             Do not announce intentions — execute them."
                .into(),
        );
        append_transcript(transcript_writer, seq, &nudge).await;
        nudge_messages.push(nudge);
        NoToolAction::Nudge
    } else {
        NoToolAction::Break
    }
}

/// Initialise per-loop state: send the initial Working status, build the
/// message list from history + task prompt, write the task message to the
/// transcript, and collect tool definitions.
#[tracing::instrument(name = "subagent.agent_loop.init_loop_state", skip_all)]
async fn init_loop_state(
    status_tx: &watch::Sender<SubAgentStatus>,
    started_at: Instant,
    effective_system_prompt: String,
    initial_messages: Vec<Message>,
    task_prompt: String,
    executor: &FilteredToolExecutor,
    transcript_writer: Option<&TranscriptWriter>,
) -> (Vec<Message>, u32, Vec<ToolDefinition>) {
    let _ = status_tx.send(SubAgentStatus {
        state: SubAgentState::Working,
        last_message: None,
        turns_used: 0,
        started_at,
    });

    let mut messages = vec![make_message(Role::System, effective_system_prompt)];
    let history_len = initial_messages.len();
    messages.extend(initial_messages);
    messages.push(make_message(Role::User, task_prompt));

    #[allow(clippy::cast_possible_truncation)]
    let mut seq: u32 = history_len as u32;

    if let Some(writer) = transcript_writer
        && let Some(task_msg) = messages.last()
    {
        if let Err(e) = writer.append(seq, task_msg).await {
            tracing::warn!(error = %e, "failed to write transcript entry");
        }
        seq += 1;
    }

    let tool_defs: Vec<ToolDefinition> = executor
        .tool_definitions_erased()
        .iter()
        .map(tool_def_to_definition)
        .collect();

    (messages, seq, tool_defs)
}

/// Outcome of a single agent turn.
enum TurnOutcome {
    /// Tool was called; the loop should continue.
    ToolCalled,
    /// No tool was called and a nudge was added; the loop should continue.
    NudgeSent,
    /// No tool was called and no nudge is needed; the loop should break.
    Done,
    /// A secret request was handled; the loop should continue.
    SecretHandled,
    /// The agent was cancelled; the loop should break.
    Cancelled,
}

/// Execute a single LLM turn: call the provider, handle secret requests,
/// dispatch tool calls, and write transcript entries.
///
/// Returns a [`TurnOutcome`] that drives the loop control flow in
/// [`run_agent_loop`].
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "subagent.agent_loop.run_turn", skip_all, fields(task_id = task_id, turn = *turns))]
async fn run_turn(
    provider: &AnyProvider,
    executor: &FilteredToolExecutor,
    messages: &mut Vec<Message>,
    tool_defs: &[ToolDefinition],
    hooks: &SubagentHooks,
    task_id: &str,
    agent_name: &str,
    status_tx: &watch::Sender<SubAgentStatus>,
    transcript_writer: Option<&TranscriptWriter>,
    seq: &mut u32,
    turns: &mut u32,
    last_result: &mut String,
    any_tool_called: bool,
    cancel: &CancellationToken,
    background: bool,
    started_at: Instant,
    secret_request_tx: &mpsc::Sender<SecretRequest>,
    secret_rx: &mut mpsc::Receiver<Option<GrantedSecret>>,
    granted_secrets: &mut HashMap<String, GrantedSecret>,
    sanitizer: &ContentSanitizer,
    llm_timeout: std::time::Duration,
) -> Result<TurnOutcome, super::error::SubAgentError> {
    let response = call_provider_with_status(
        provider,
        messages,
        tool_defs,
        status_tx,
        *turns,
        started_at,
        llm_timeout,
    )
    .await?;

    let response_text = match &response {
        ChatResponse::Text(t) => t.clone(),
        ChatResponse::ToolUse { text, .. } => text.as_deref().unwrap_or_default().to_owned(),
        _ => String::new(),
    };

    *turns += 1;
    last_result.clone_from(&response_text);
    emit_working_status(status_tx, &response_text, *turns, started_at);

    let is_text_response = matches!(&response, ChatResponse::Text(_));
    match handle_secret_request(
        transcript_writer,
        seq,
        messages,
        granted_secrets,
        secret_request_tx,
        secret_rx,
        cancel,
        background,
        is_text_response,
        &response_text,
    )
    .await
    {
        SecretRequestOutcome::Handled => return Ok(TurnOutcome::SecretHandled),
        SecretRequestOutcome::Cancelled => return Ok(TurnOutcome::Cancelled),
        SecretRequestOutcome::NotASecretRequest => {}
    }

    let prev_len = messages.len();
    let no_tool = handle_tool_step(
        executor,
        response,
        messages,
        hooks,
        task_id,
        agent_name,
        sanitizer,
        granted_secrets,
    )
    .await;

    if no_tool {
        let mut nudge_messages = Vec::new();
        match handle_no_tool_response(
            transcript_writer,
            seq,
            messages,
            prev_len,
            *turns,
            any_tool_called,
            &mut nudge_messages,
        )
        .await
        {
            NoToolAction::Nudge => {
                messages.extend(nudge_messages);
                return Ok(TurnOutcome::NudgeSent);
            }
            NoToolAction::Break => return Ok(TurnOutcome::Done),
        }
    }

    for msg in &messages[prev_len..] {
        append_transcript(transcript_writer, seq, msg).await;
    }
    Ok(TurnOutcome::ToolCalled)
}

// Returns `true` if no tool was called (loop should break).
#[tracing::instrument(name = "subagent.agent_loop.handle_tool_step", skip_all)]
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn handle_tool_step(
    executor: &FilteredToolExecutor,
    response: ChatResponse,
    messages: &mut Vec<Message>,
    hooks: &SubagentHooks,
    task_id: &str,
    agent_name: &str,
    sanitizer: &ContentSanitizer,
    granted_secrets: &mut HashMap<String, GrantedSecret>,
) -> bool {
    match response {
        ChatResponse::Text(text) => {
            messages.push(make_message(Role::Assistant, text));
            true
        }
        ChatResponse::ToolUse {
            text,
            tool_calls,
            thinking_blocks: _,
        } => {
            let mut assistant_parts: Vec<MessagePart> = Vec::new();
            if let Some(ref t) = text
                && !t.is_empty()
            {
                assistant_parts.push(MessagePart::Text { text: t.clone() });
            }
            for tc in &tool_calls {
                assistant_parts.push(MessagePart::ToolUse {
                    id: tc.id.clone(),
                    name: tc.name.to_string(),
                    input: tc.input.clone(),
                });
            }
            messages.push(Message::from_parts(Role::Assistant, assistant_parts));

            let mut result_parts: Vec<MessagePart> = Vec::new();
            for tc in &tool_calls {
                let pre_hooks: Vec<&HookDef> =
                    matching_hooks(&hooks.pre_tool_use, tc.name.as_str());
                if !pre_hooks.is_empty() {
                    let hook_env = make_hook_env(task_id, agent_name, tc.name.as_str(), &tc.input);
                    let pre_owned: Vec<HookDef> = pre_hooks.into_iter().cloned().collect();
                    // MCP dispatch is not available in the subagent execution path.
                    if let Err(e) = fire_hooks(&pre_owned, &hook_env, None, None).await {
                        tracing::warn!(error = %e, tool = %tc.name, "PreToolUse hook failed");
                    }
                }

                let params: serde_json::Map<String, serde_json::Value> =
                    if let serde_json::Value::Object(map) = &tc.input {
                        map.clone()
                    } else {
                        serde_json::Map::new()
                    };
                // Approved sub-agent secrets are attached as a per-call `ExecutionContext`
                // env override (highest-priority, call-scoped) rather than the executor's
                // shared `skill_env` slot, which is aliased with the parent agent's own tool
                // executor and would leak the secret beyond this sub-agent's run.
                //
                // Re-check the TTL live before every tool call rather than trusting the
                // one-time gate at delivery time: a long-running turn loop can otherwise keep
                // using a secret well after its grant expired (issue #5991).
                //
                // This only reacts to TTL expiry, not to an explicit mid-loop revoke of a
                // single grant — currently unreachable since every `revoke_all()` call site
                // either fires `cancel.cancel()` alongside it or only runs once the loop has
                // already reported a terminal status (see manager/collect.rs, manager/spawn.rs,
                // manager/mod.rs's `Drop for SubAgentHandle`).
                granted_secrets.retain(|_, granted| !granted.is_expired());
                let exec_ctx = if granted_secrets.is_empty() {
                    None
                } else {
                    Some(
                        zeph_tools::ExecutionContext::new().with_envs(
                            granted_secrets
                                .iter()
                                .map(|(k, v)| (k.clone(), v.value.expose().to_owned())),
                        ),
                    )
                };
                let call = ToolCall {
                    tool_id: tc.name.clone(),
                    params,
                    caller_id: None,
                    context: exec_ctx,
                    tool_call_id: String::new(),
                    skill_name: None,
                };
                let tool_start = Instant::now();
                let exec_result = executor.execute_tool_call_erased(&call).await;
                let duration_ms =
                    u64::try_from(tool_start.elapsed().as_millis()).unwrap_or(u64::MAX);

                let (mut content, is_error) = match &exec_result {
                    Ok(Some(output)) => (
                        format!(
                            "[tool output: {}]\n```\n{}\n```",
                            output.tool_name, output.summary
                        ),
                        false,
                    ),
                    Ok(None) => (String::new(), false),
                    Err(e) => {
                        tracing::warn!(error = %e, tool = %tc.name, "sub-agent tool execution failed");
                        (format!("[tool error]: {e}"), true)
                    }
                };

                if !hooks.post_tool_use.is_empty() {
                    let post_hooks: Vec<&HookDef> =
                        matching_hooks(&hooks.post_tool_use, tc.name.as_str());
                    if !post_hooks.is_empty() {
                        let mut hook_env =
                            make_hook_env(task_id, agent_name, tc.name.as_str(), &tc.input);
                        hook_env
                            .insert("ZEPH_TOOL_DURATION_MS".to_owned(), duration_ms.to_string());
                        let post_owned: Vec<HookDef> = post_hooks.into_iter().cloned().collect();
                        let tool_output_text = exec_result
                            .as_ref()
                            .ok()
                            .and_then(|r| r.as_ref())
                            .map(|o| o.summary.as_str());
                        let tool_error_text = exec_result
                            .as_ref()
                            .err()
                            .map(std::string::ToString::to_string);
                        let hook_input = super::hooks::PostToolUseHookInput {
                            tool_name: tc.name.as_str(),
                            tool_args: &tc.input,
                            session_id: None,
                            duration_ms,
                            tool_output: tool_output_text,
                            tool_error: tool_error_text.as_deref(),
                            agent_id: Some(task_id),
                            agent_type: "subagent",
                        };
                        let stdin_bytes = serde_json::to_vec(&hook_input).ok();
                        // MCP dispatch is not available in the subagent execution path.
                        match fire_hooks(&post_owned, &hook_env, None, stdin_bytes.as_deref()).await
                        {
                            Ok(run_result) => {
                                if let Some(replacement) = run_result.output.updated_tool_output {
                                    tracing::debug!(
                                        tool = %tc.name,
                                        "PostToolUse hook replaced sub-agent tool output"
                                    );
                                    let source = if tc.name.as_str().contains(':') {
                                        ContentSource::new(ContentSourceKind::McpResponse)
                                            .with_identifier(tc.name.as_str())
                                    } else {
                                        ContentSource::new(ContentSourceKind::ToolResult)
                                            .with_identifier(tc.name.as_str())
                                    };
                                    let san_result = sanitizer.sanitize(&replacement, source);
                                    if !san_result.injection_flags.is_empty() {
                                        tracing::warn!(
                                            tool = %tc.name,
                                            flags = san_result.injection_flags.len(),
                                            "injection patterns detected in hook-replaced sub-agent tool output"
                                        );
                                    }
                                    content = san_result.body;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    tool = %tc.name,
                                    "PostToolUse hook failed"
                                );
                            }
                        }
                    }
                }

                result_parts.push(MessagePart::ToolResult {
                    tool_use_id: tc.id.clone(),
                    content,
                    is_error,
                });
            }

            messages.push(Message::from_parts(Role::User, result_parts));
            false
        }
        _ => true,
    }
}

/// Drop the oldest non-system messages when `messages` exceeds `limit`.
///
/// The system message at index 0 (if `role == System`) is always preserved.
/// When `limit == 0` the function is a no-op.
fn trim_message_history(messages: &mut Vec<Message>, limit: usize) {
    if limit == 0 || messages.len() <= limit {
        return;
    }
    let has_system = messages.first().is_some_and(|m| m.role == Role::System);
    let excess = messages.len() - limit;
    // Remove from the front, but skip index 0 when a system message is present.
    let start = usize::from(has_system);
    let drain_end = (start + excess).min(messages.len());
    tracing::debug!(
        dropped = drain_end - start,
        remaining = messages.len() - (drain_end - start),
        "trimming subagent message history"
    );
    messages.drain(start..drain_end);
}

#[tracing::instrument(name = "subagent.agent_loop.run", skip_all, fields(task_id = %args.task_id, agent_name = %args.agent_name))]
pub(super) async fn run_agent_loop(
    args: AgentLoopArgs,
) -> Result<String, super::error::SubAgentError> {
    let AgentLoopArgs {
        provider,
        executor,
        system_prompt,
        task_prompt,
        skills,
        max_turns,
        max_history_messages,
        cancel,
        status_tx,
        started_at,
        secret_request_tx,
        mut secret_rx,
        background,
        hooks,
        task_id: loop_task_id,
        agent_name,
        initial_messages,
        transcript_writer,
        spawn_depth: _spawn_depth,
        mcp_tool_names,
        content_isolation,
        llm_timeout,
    } = args;

    let sanitizer = ContentSanitizer::new(&content_isolation);

    let effective_system_prompt =
        build_effective_system_prompt(system_prompt, skills, &mcp_tool_names);

    let (mut messages, mut seq, tool_defs) = init_loop_state(
        &status_tx,
        started_at,
        effective_system_prompt,
        initial_messages,
        task_prompt,
        &executor,
        transcript_writer.as_ref(),
    )
    .await;

    let mut turns: u32 = 0;
    let mut last_result = String::new();
    let mut any_tool_called = false;
    // Accumulates resolved secret values across the loop's lifetime so a later approval
    // does not evict an earlier one from the tool executor's environment (see
    // `handle_secret_request`). Entries whose grant TTL has elapsed are evicted live in
    // `handle_tool_step`, not just gated once at delivery time.
    let mut granted_secrets: HashMap<String, GrantedSecret> = HashMap::new();

    loop {
        if cancel.is_cancelled() {
            tracing::debug!("sub-agent cancelled, stopping loop");
            break;
        }
        if turns >= max_turns {
            tracing::debug!(turns, max_turns, "sub-agent reached max_turns limit");
            break;
        }

        match run_turn(
            &provider,
            &executor,
            &mut messages,
            &tool_defs,
            &hooks,
            &loop_task_id,
            &agent_name,
            &status_tx,
            transcript_writer.as_ref(),
            &mut seq,
            &mut turns,
            &mut last_result,
            any_tool_called,
            &cancel,
            background,
            started_at,
            &secret_request_tx,
            &mut secret_rx,
            &mut granted_secrets,
            &sanitizer,
            llm_timeout,
        )
        .await?
        {
            TurnOutcome::ToolCalled => any_tool_called = true,
            TurnOutcome::NudgeSent | TurnOutcome::SecretHandled => {}
            TurnOutcome::Done | TurnOutcome::Cancelled => break,
        }

        trim_message_history(&mut messages, max_history_messages);
    }

    let _ = status_tx.send(SubAgentStatus {
        state: SubAgentState::Completed,
        last_message: Some(last_result.chars().take(120).collect()),
        turns_used: turns,
        started_at,
    });

    Ok(last_result)
}

#[cfg(test)]
mod trim_message_history_tests {
    use super::*;

    fn sys(text: &str) -> Message {
        make_message(Role::System, text.to_owned())
    }
    fn usr(text: &str) -> Message {
        make_message(Role::User, text.to_owned())
    }
    fn asst(text: &str) -> Message {
        make_message(Role::Assistant, text.to_owned())
    }

    #[test]
    fn noop_when_within_limit() {
        let mut msgs = vec![sys("sys"), usr("u1"), asst("a1")];
        trim_message_history(&mut msgs, 10);
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn noop_when_limit_zero() {
        let mut msgs = vec![sys("sys"), usr("u1"), asst("a1"), usr("u2")];
        trim_message_history(&mut msgs, 0);
        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn preserves_system_message_and_trims_oldest() {
        // 6 messages, limit 4 → drop 2 oldest non-system
        let mut msgs = vec![
            sys("sys"),
            usr("u1"),
            asst("a1"),
            usr("u2"),
            asst("a2"),
            usr("u3"),
        ];
        trim_message_history(&mut msgs, 4);
        assert_eq!(msgs.len(), 4, "should have 4 messages after trim");
        assert_eq!(
            msgs[0].role,
            Role::System,
            "system message must be at index 0"
        );
        assert_eq!(msgs[1].content, "u2");
        assert_eq!(msgs[2].content, "a2");
        assert_eq!(msgs[3].content, "u3");
    }

    #[test]
    fn no_system_message_trims_from_front() {
        let mut msgs = vec![usr("u1"), asst("a1"), usr("u2"), asst("a2"), usr("u3")];
        trim_message_history(&mut msgs, 3);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].content, "u2");
    }

    #[test]
    fn exactly_at_limit_is_noop() {
        let mut msgs = vec![sys("sys"), usr("u1"), asst("a1")];
        trim_message_history(&mut msgs, 3);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, Role::System);
    }
}

#[cfg(test)]
mod make_hook_env_tests {
    use super::super::hooks::TOOL_ARGS_JSON_LIMIT;
    use super::*;

    #[test]
    fn sets_agent_id_and_name() {
        let env = make_hook_env("task-1", "bot", "Edit", &serde_json::Value::Null);
        assert_eq!(env.get("ZEPH_AGENT_ID").map(String::as_str), Some("task-1"));
        assert_eq!(env.get("ZEPH_AGENT_NAME").map(String::as_str), Some("bot"));
        assert_eq!(
            env.get("ZEPH_AGENT_TYPE").map(String::as_str),
            Some("subagent")
        );
    }

    #[test]
    fn truncation_lands_on_char_boundary() {
        let mut big = String::from(r#"{"d":""#);
        while big.len() < TOOL_ARGS_JSON_LIMIT - 3 {
            big.push('a');
        }
        big.push('€'); // 3-byte UTF-8 char that may straddle the boundary
        while big.len() < TOOL_ARGS_JSON_LIMIT + 50 {
            big.push('b');
        }
        big.push_str(r#""}"#);
        let input: serde_json::Value = serde_json::from_str(&big).unwrap_or_default();
        let env = make_hook_env("Shell", "bot", "Shell", &input);
        let args = env
            .get("ZEPH_TOOL_ARGS_JSON")
            .expect("ZEPH_TOOL_ARGS_JSON missing");
        assert!(
            args.ends_with('…'),
            "truncated value should end with ellipsis"
        );
        assert!(args.is_char_boundary(args.len()));
    }
}

#[cfg(test)]
mod handle_tool_step_granted_secrets_tests {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use zeph_common::secret::Secret;
    use zeph_llm::provider::ToolUseRequest;
    use zeph_tools::executor::{ErasedToolExecutor, ToolCall, ToolError, ToolOutput};
    use zeph_tools::registry::ToolDef;

    use super::*;
    use crate::def::ToolPolicy;
    use crate::filter::FilteredToolExecutor;
    use crate::hooks::SubagentHooks;

    /// Records every `ToolCall` it receives so tests can inspect `call.context`.
    #[derive(Default)]
    struct RecordingExecutor {
        calls: Mutex<Vec<ToolCall>>,
    }

    impl ErasedToolExecutor for RecordingExecutor {
        fn execute_erased<'a>(
            &'a self,
            _response: &'a str,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn execute_confirmed_erased<'a>(
            &'a self,
            _response: &'a str,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(Ok(None)))
        }

        fn tool_definitions_erased(&self) -> Vec<ToolDef> {
            vec![]
        }

        fn execute_tool_call_erased<'a>(
            &'a self,
            call: &'a ToolCall,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            self.calls.lock().unwrap().push(call.clone());
            Box::pin(std::future::ready(Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ok".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
                ..Default::default()
            }))))
        }

        fn is_tool_retryable_erased(&self, _tool_id: &str) -> bool {
            false
        }

        fn requires_confirmation_erased(&self, _call: &ToolCall) -> bool {
            false
        }

        fn execute_tool_call_confirmed_erased<'a>(
            &'a self,
            call: &'a ToolCall,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<ToolOutput>, ToolError>> + Send + 'a,
            >,
        > {
            self.execute_tool_call_erased(call)
        }

        fn checkpoint_undo_erased(&self, _n: usize) -> zeph_tools::CheckpointActionResult {
            zeph_tools::CheckpointActionResult::unsupported()
        }

        fn checkpoint_redo_erased(&self) -> zeph_tools::CheckpointActionResult {
            zeph_tools::CheckpointActionResult::unsupported()
        }

        fn checkpoint_list_erased(&self) -> zeph_tools::CheckpointListResult {
            zeph_tools::CheckpointListResult::default()
        }

        fn is_tool_speculatable_erased(&self, _tool_id: &str) -> bool {
            false
        }
    }

    fn tool_use_response() -> ChatResponse {
        ChatResponse::ToolUse {
            text: None,
            tool_calls: vec![ToolUseRequest {
                id: "call-1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "echo $SOME_VAULT_KEY"}),
            }],
            thinking_blocks: vec![],
        }
    }

    #[tokio::test]
    async fn granted_secret_is_attached_to_tool_call_context() {
        let recorder = Arc::new(RecordingExecutor::default());
        let executor = FilteredToolExecutor::new(
            Arc::clone(&recorder) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let mut messages = Vec::new();
        let mut granted_secrets = HashMap::new();
        granted_secrets.insert(
            "SOME_VAULT_KEY".to_owned(),
            GrantedSecret {
                value: Secret::new("the-secret-value"),
                expires_at: Instant::now() + Duration::from_mins(5),
            },
        );
        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
        )
        .await;
        assert!(!no_tool, "a tool call was made");

        let calls = recorder.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let context = calls[0]
            .context
            .as_ref()
            .expect("granted secrets must produce a Some(ExecutionContext)");
        assert_eq!(
            context
                .env_overrides()
                .get("SOME_VAULT_KEY")
                .map(String::as_str),
            Some("the-secret-value")
        );
    }

    #[tokio::test]
    async fn no_granted_secrets_leaves_tool_call_context_none() {
        // Regression guard: with no granted secrets, handle_tool_step must build
        // context: None — identical to pre-fix behavior.
        let recorder = Arc::new(RecordingExecutor::default());
        let executor = FilteredToolExecutor::new(
            Arc::clone(&recorder) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let mut messages = Vec::new();
        let mut granted_secrets: HashMap<String, GrantedSecret> = HashMap::new();
        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
        )
        .await;
        assert!(!no_tool);

        let calls = recorder.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].context.is_none(),
            "no granted secrets must leave context as None"
        );
    }

    #[tokio::test]
    async fn expired_granted_secret_is_not_attached_and_is_evicted() {
        // Regression guard for #5991: a secret whose grant TTL has already elapsed must
        // not be re-injected into a tool call's ExecutionContext, and must be dropped
        // from the loop's local cache rather than lingering for the rest of the run.
        let recorder = Arc::new(RecordingExecutor::default());
        let executor = FilteredToolExecutor::new(
            Arc::clone(&recorder) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let mut messages = Vec::new();
        let mut granted_secrets = HashMap::new();
        granted_secrets.insert(
            "SOME_VAULT_KEY".to_owned(),
            GrantedSecret {
                value: Secret::new("the-secret-value"),
                expires_at: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            },
        );

        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
        )
        .await;
        assert!(!no_tool, "a tool call was made");

        let calls = recorder.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].context.is_none(),
            "an expired grant must not be attached to the tool call context"
        );
        assert!(
            granted_secrets.is_empty(),
            "an expired grant must be evicted from the local cache"
        );
    }

    #[tokio::test]
    async fn mixed_expired_and_live_secrets_expired_evicted_live_survives() {
        // Regression guard for #6124: a `granted_secrets` map holding both an expired and
        // a live grant at once must evict only the expired entry — the live one must
        // survive eviction and still be attached to the tool call context.
        let recorder = Arc::new(RecordingExecutor::default());
        let executor = FilteredToolExecutor::new(
            Arc::clone(&recorder) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let mut messages = Vec::new();
        let mut granted_secrets = HashMap::new();
        granted_secrets.insert(
            "EXPIRED_KEY".to_owned(),
            GrantedSecret {
                value: Secret::new("expired-value"),
                expires_at: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            },
        );
        granted_secrets.insert(
            "LIVE_KEY".to_owned(),
            GrantedSecret {
                value: Secret::new("live-value"),
                expires_at: Instant::now() + Duration::from_mins(5),
            },
        );

        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
        )
        .await;
        assert!(!no_tool, "a tool call was made");

        assert_eq!(
            granted_secrets.len(),
            1,
            "only the expired grant should be evicted"
        );
        assert!(
            !granted_secrets.contains_key("EXPIRED_KEY"),
            "expired grant must be evicted"
        );
        assert!(
            granted_secrets.contains_key("LIVE_KEY"),
            "live grant must survive eviction"
        );

        let calls = recorder.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let context = calls[0]
            .context
            .as_ref()
            .expect("the live grant must still produce a Some(ExecutionContext)");
        assert_eq!(
            context.env_overrides().get("LIVE_KEY").map(String::as_str),
            Some("live-value")
        );
        assert!(
            context.env_overrides().get("EXPIRED_KEY").is_none(),
            "expired grant must not be attached to the tool call context"
        );
    }
}

#[cfg(test)]
mod build_effective_system_prompt_tests {
    use super::*;

    /// #5712 regression: confirms the "Available MCP Tools" annotation actually reaches
    /// the effective system prompt when `extract_mcp_tool_names` returns non-empty names —
    /// the end of the pipeline that the dead `"mcp_"` prefix check silently starved.
    #[test]
    fn appends_mcp_tool_annotation_when_names_present() {
        let mcp_tool_names = vec!["github_create_issue".to_owned(), "slack_post".to_owned()];
        let effective =
            build_effective_system_prompt("base prompt".to_owned(), None, &mcp_tool_names);

        assert!(effective.starts_with("base prompt"));
        assert!(effective.contains("## Available MCP Tools"));
        assert!(effective.contains("- github_create_issue"));
        assert!(effective.contains("- slack_post"));
    }

    #[test]
    fn omits_mcp_annotation_when_names_empty() {
        let effective = build_effective_system_prompt("base prompt".to_owned(), None, &[]);
        assert_eq!(effective, "base prompt");
    }

    #[test]
    fn combines_skills_block_and_mcp_annotation() {
        let skills = Some(vec!["skill body".to_owned()]);
        let mcp_tool_names = vec!["github_create_issue".to_owned()];
        let effective =
            build_effective_system_prompt("base prompt".to_owned(), skills, &mcp_tool_names);

        let skills_idx = effective
            .find("```skills")
            .expect("skills block must be present");
        let mcp_idx = effective
            .find("## Available MCP Tools")
            .expect("mcp annotation must be present");
        assert!(
            skills_idx < mcp_idx,
            "skills block must precede the mcp annotation"
        );
    }
}
