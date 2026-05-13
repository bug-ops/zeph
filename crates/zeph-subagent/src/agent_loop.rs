// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::Instant;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{
    ChatResponse, LlmProvider, Message, MessageMetadata, MessagePart, Role, ToolDefinition,
};
use zeph_tools::executor::{ErasedToolExecutor, ToolCall};

use super::filter::FilteredToolExecutor;
use super::grants::SecretRequest;
use super::hooks::{HookDef, SubagentHooks, fire_hooks, matching_hooks};
use super::manager::SubAgentStatus;
use super::state::SubAgentState;
use super::transcript::TranscriptWriter;

const SECRET_REQUEST_PREFIX: &str = "[REQUEST_SECRET:";

enum SecretRequestOutcome {
    NotASecretRequest,
    Handled,
    Cancelled,
}

/// Maximum byte length of `ZEPH_TOOL_ARGS_JSON` to avoid `E2BIG` when spawning hook processes.
const TOOL_ARGS_JSON_LIMIT: usize = 64 * 1024;

fn make_hook_env(
    task_id: &str,
    agent_name: &str,
    tool_name: &str,
    tool_input: &serde_json::Value,
) -> std::collections::HashMap<String, String> {
    let mut env = std::collections::HashMap::new();
    env.insert("ZEPH_AGENT_ID".to_owned(), task_id.to_owned());
    env.insert("ZEPH_AGENT_NAME".to_owned(), agent_name.to_owned());
    env.insert("ZEPH_TOOL_NAME".to_owned(), tool_name.to_owned());

    let raw = serde_json::to_string(tool_input).unwrap_or_default();
    let args_json = if raw.len() > TOOL_ARGS_JSON_LIMIT {
        tracing::warn!(
            tool = tool_name,
            len = raw.len(),
            limit = TOOL_ARGS_JSON_LIMIT,
            "ZEPH_TOOL_ARGS_JSON truncated for hook dispatch"
        );
        let limit = raw.floor_char_boundary(TOOL_ARGS_JSON_LIMIT);
        format!("{}…", &raw[..limit])
    } else {
        raw
    };
    env.insert("ZEPH_TOOL_ARGS_JSON".to_owned(), args_json);

    env
}

pub(super) struct AgentLoopArgs {
    pub(super) provider: AnyProvider,
    pub(super) executor: FilteredToolExecutor,
    pub(super) system_prompt: String,
    pub(super) task_prompt: String,
    pub(super) skills: Option<Vec<String>>,
    pub(super) max_turns: u32,
    pub(super) cancel: CancellationToken,
    pub(super) status_tx: watch::Sender<SubAgentStatus>,
    pub(super) started_at: Instant,
    pub(super) secret_request_tx: mpsc::Sender<SecretRequest>,
    pub(super) secret_rx: mpsc::Receiver<Option<String>>,
    pub(super) background: bool,
    pub(super) hooks: SubagentHooks,
    pub(super) task_id: String,
    pub(super) agent_name: String,
    pub(super) initial_messages: Vec<Message>,
    pub(super) transcript_writer: Option<TranscriptWriter>,
    pub(super) spawn_depth: u32,
    pub(super) mcp_tool_names: Vec<String>,
}

pub(super) fn make_message(role: Role, content: String) -> Message {
    Message {
        role,
        content,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }
}

pub(super) fn append_transcript(
    writer: &mut Option<TranscriptWriter>,
    seq: &mut u32,
    msg: &Message,
) {
    if let Some(w) = writer {
        if let Err(e) = w.append(*seq, msg) {
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

async fn call_provider_with_status(
    provider: &AnyProvider,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    status_tx: &watch::Sender<SubAgentStatus>,
    turns: u32,
    started_at: Instant,
) -> Result<ChatResponse, super::error::SubAgentError> {
    let llm_result = provider.chat_with_tools(messages, tool_defs).await;
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

#[allow(clippy::too_many_arguments)]
async fn handle_secret_request(
    transcript_writer: &mut Option<TranscriptWriter>,
    seq: &mut u32,
    messages: &mut Vec<Message>,
    secret_request_tx: &mpsc::Sender<SecretRequest>,
    secret_rx: &mut mpsc::Receiver<Option<String>>,
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
        append_transcript(transcript_writer, seq, &assistant_msg);
        append_transcript(transcript_writer, seq, &user_msg);
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
            Some(Some(_)) => {
                format!("[secret:{key_name} approved — value available via grants]")
            }
            Some(None) | None => {
                format!("[secret:{key_name}] request denied")
            }
        };
        let assistant_msg = make_message(Role::Assistant, response_text.to_owned());
        let user_msg = make_message(Role::User, reply);
        append_transcript(transcript_writer, seq, &assistant_msg);
        append_transcript(transcript_writer, seq, &user_msg);
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
fn handle_no_tool_response(
    transcript_writer: &mut Option<TranscriptWriter>,
    seq: &mut u32,
    messages: &[Message],
    prev_len: usize,
    turns: u32,
    any_tool_called: bool,
    nudge_messages: &mut Vec<Message>,
) -> NoToolAction {
    for msg in &messages[prev_len..] {
        append_transcript(transcript_writer, seq, msg);
    }
    if turns == 1 && !any_tool_called {
        tracing::debug!("sub-agent text-only first turn — sending nudge to use tools");
        let nudge = make_message(
            Role::User,
            "Please use the available tools to complete the task. \
             Do not announce intentions — execute them."
                .into(),
        );
        append_transcript(transcript_writer, seq, &nudge);
        nudge_messages.push(nudge);
        NoToolAction::Nudge
    } else {
        NoToolAction::Break
    }
}

/// Initialise per-loop state: send the initial Working status, build the
/// message list from history + task prompt, write the task message to the
/// transcript, and collect tool definitions.
fn init_loop_state(
    status_tx: &watch::Sender<SubAgentStatus>,
    started_at: Instant,
    effective_system_prompt: String,
    initial_messages: Vec<Message>,
    task_prompt: String,
    executor: &FilteredToolExecutor,
    transcript_writer: &mut Option<TranscriptWriter>,
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
        if let Err(e) = writer.append(seq, task_msg) {
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
    transcript_writer: &mut Option<TranscriptWriter>,
    seq: &mut u32,
    turns: &mut u32,
    last_result: &mut String,
    any_tool_called: bool,
    cancel: &CancellationToken,
    background: bool,
    started_at: Instant,
    secret_request_tx: &mpsc::Sender<SecretRequest>,
    secret_rx: &mut mpsc::Receiver<Option<String>>,
) -> Result<TurnOutcome, super::error::SubAgentError> {
    let response =
        call_provider_with_status(provider, messages, tool_defs, status_tx, *turns, started_at)
            .await?;

    let response_text = match &response {
        ChatResponse::Text(t) => t.clone(),
        ChatResponse::ToolUse { text, .. } => text.as_deref().unwrap_or_default().to_owned(),
    };

    *turns += 1;
    last_result.clone_from(&response_text);
    emit_working_status(status_tx, &response_text, *turns, started_at);

    let is_text_response = matches!(&response, ChatResponse::Text(_));
    match handle_secret_request(
        transcript_writer,
        seq,
        messages,
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
    let no_tool = handle_tool_step(executor, response, messages, hooks, task_id, agent_name).await;

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
        ) {
            NoToolAction::Nudge => {
                messages.extend(nudge_messages);
                return Ok(TurnOutcome::NudgeSent);
            }
            NoToolAction::Break => return Ok(TurnOutcome::Done),
        }
    }

    for msg in &messages[prev_len..] {
        append_transcript(transcript_writer, seq, msg);
    }
    Ok(TurnOutcome::ToolCalled)
}

// Returns `true` if no tool was called (loop should break).
async fn handle_tool_step(
    executor: &FilteredToolExecutor,
    response: ChatResponse,
    messages: &mut Vec<Message>,
    hooks: &SubagentHooks,
    task_id: &str,
    agent_name: &str,
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
                    if let Err(e) = fire_hooks(&pre_owned, &hook_env, None).await {
                        tracing::warn!(error = %e, tool = %tc.name, "PreToolUse hook failed");
                    }
                }

                let params: serde_json::Map<String, serde_json::Value> =
                    if let serde_json::Value::Object(map) = &tc.input {
                        map.clone()
                    } else {
                        serde_json::Map::new()
                    };
                let call = ToolCall {
                    tool_id: tc.name.clone(),
                    params,
                    caller_id: None,
                    context: None,

                    tool_call_id: String::new(),
                };
                let tool_start = Instant::now();
                let (content, is_error) = match executor.execute_tool_call_erased(&call).await {
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
                let duration_ms = tool_start.elapsed().as_millis();
                result_parts.push(MessagePart::ToolResult {
                    tool_use_id: tc.id.clone(),
                    content,
                    is_error,
                });

                if !hooks.post_tool_use.is_empty() {
                    let post_hooks: Vec<&HookDef> =
                        matching_hooks(&hooks.post_tool_use, tc.name.as_str());
                    if !post_hooks.is_empty() {
                        let mut hook_env =
                            make_hook_env(task_id, agent_name, tc.name.as_str(), &tc.input);
                        hook_env
                            .insert("ZEPH_TOOL_DURATION_MS".to_owned(), duration_ms.to_string());
                        let post_owned: Vec<HookDef> = post_hooks.into_iter().cloned().collect();
                        // MCP dispatch is not available in the subagent execution path.
                        if let Err(e) = fire_hooks(&post_owned, &hook_env, None).await {
                            tracing::warn!(
                                error = %e,
                                tool = %tc.name,
                                "PostToolUse hook failed"
                            );
                        }
                    }
                }
            }

            messages.push(Message::from_parts(Role::User, result_parts));
            false
        }
    }
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
        mut transcript_writer,
        spawn_depth: _spawn_depth,
        mcp_tool_names,
    } = args;

    let effective_system_prompt =
        build_effective_system_prompt(system_prompt, skills, &mcp_tool_names);

    let (mut messages, mut seq, tool_defs) = init_loop_state(
        &status_tx,
        started_at,
        effective_system_prompt,
        initial_messages,
        task_prompt,
        &executor,
        &mut transcript_writer,
    );

    let mut turns: u32 = 0;
    let mut last_result = String::new();
    let mut any_tool_called = false;

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
            &mut transcript_writer,
            &mut seq,
            &mut turns,
            &mut last_result,
            any_tool_called,
            &cancel,
            background,
            started_at,
            &secret_request_tx,
            &mut secret_rx,
        )
        .await?
        {
            TurnOutcome::ToolCalled => any_tool_called = true,
            TurnOutcome::NudgeSent | TurnOutcome::SecretHandled => {}
            TurnOutcome::Done | TurnOutcome::Cancelled => break,
        }
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
mod make_hook_env_tests {
    use super::*;

    #[test]
    fn sets_agent_id_and_name() {
        let env = make_hook_env("task-1", "bot", "Edit", &serde_json::Value::Null);
        assert_eq!(env.get("ZEPH_AGENT_ID").map(String::as_str), Some("task-1"));
        assert_eq!(env.get("ZEPH_AGENT_NAME").map(String::as_str), Some("bot"));
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
