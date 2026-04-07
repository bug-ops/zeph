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

fn make_hook_env(
    task_id: &str,
    agent_name: &str,
    tool_name: &str,
) -> std::collections::HashMap<String, String> {
    let mut env = std::collections::HashMap::new();
    env.insert("ZEPH_AGENT_ID".to_owned(), task_id.to_owned());
    env.insert("ZEPH_AGENT_NAME".to_owned(), agent_name.to_owned());
    env.insert("ZEPH_TOOL_NAME".to_owned(), tool_name.to_owned());
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
        name: def.id.to_string(),
        description: def.description.to_string(),
        parameters: params,
    }
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
                    name: tc.name.clone(),
                    input: tc.input.clone(),
                });
            }
            messages.push(Message::from_parts(Role::Assistant, assistant_parts));

            let mut result_parts: Vec<MessagePart> = Vec::new();
            for tc in &tool_calls {
                let hook_env = make_hook_env(task_id, agent_name, &tc.name);

                let pre_hooks: Vec<&HookDef> = matching_hooks(&hooks.pre_tool_use, &tc.name);
                if !pre_hooks.is_empty() {
                    let pre_owned: Vec<HookDef> = pre_hooks.into_iter().cloned().collect();
                    if let Err(e) = fire_hooks(&pre_owned, &hook_env).await {
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
                };
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
                result_parts.push(MessagePart::ToolResult {
                    tool_use_id: tc.id.clone(),
                    content,
                    is_error,
                });

                if !hooks.post_tool_use.is_empty() {
                    let post_hooks: Vec<&HookDef> = matching_hooks(&hooks.post_tool_use, &tc.name);
                    if !post_hooks.is_empty() {
                        let post_owned: Vec<HookDef> = post_hooks.into_iter().cloned().collect();
                        if let Err(e) = fire_hooks(&post_owned, &hook_env).await {
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

#[allow(clippy::too_many_lines)]
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
    let _ = status_tx.send(SubAgentStatus {
        state: SubAgentState::Working,
        last_message: None,
        turns_used: 0,
        started_at,
    });

    let mut effective_system_prompt = if let Some(skill_bodies) = skills.filter(|s| !s.is_empty()) {
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
        effective_system_prompt.push_str(&mcp_annotation);
    }

    let mut messages = vec![make_message(Role::System, effective_system_prompt)];
    let history_len = initial_messages.len();
    messages.extend(initial_messages);
    messages.push(make_message(Role::User, task_prompt));

    #[allow(clippy::cast_possible_truncation)]
    let mut seq: u32 = history_len as u32;

    if let Some(writer) = &mut transcript_writer
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

        let llm_result = provider.chat_with_tools(&messages, &tool_defs).await;
        let response = match llm_result {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "sub-agent LLM call failed");
                let _ = status_tx.send(SubAgentStatus {
                    state: SubAgentState::Failed,
                    last_message: Some(e.to_string()),
                    turns_used: turns,
                    started_at,
                });
                return Err(super::error::SubAgentError::Llm(e.to_string()));
            }
        };

        let response_text = match &response {
            ChatResponse::Text(t) => t.clone(),
            ChatResponse::ToolUse { text, .. } => text.as_deref().unwrap_or_default().to_owned(),
        };

        turns += 1;
        last_result.clone_from(&response_text);
        let _ = status_tx.send(SubAgentStatus {
            state: SubAgentState::Working,
            last_message: Some(response_text.chars().take(120).collect()),
            turns_used: turns,
            started_at,
        });

        if let ChatResponse::Text(_) = &response
            && let Some(rest) = response_text.strip_prefix(SECRET_REQUEST_PREFIX)
        {
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
            if !key_name.is_empty() {
                tracing::debug!("sub-agent requested secret [key redacted]");

                if background {
                    tracing::warn!(
                        "background sub-agent secret request auto-denied (no interactive prompt)"
                    );
                    let reply = format!("[secret:{key_name}] request denied");
                    let assistant_msg = make_message(Role::Assistant, response_text);
                    let user_msg = make_message(Role::User, reply);
                    append_transcript(&mut transcript_writer, &mut seq, &assistant_msg);
                    append_transcript(&mut transcript_writer, &mut seq, &user_msg);
                    messages.push(assistant_msg);
                    messages.push(user_msg);
                    continue;
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
                            break;
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
                    let assistant_msg = make_message(Role::Assistant, response_text);
                    let user_msg = make_message(Role::User, reply);
                    append_transcript(&mut transcript_writer, &mut seq, &assistant_msg);
                    append_transcript(&mut transcript_writer, &mut seq, &user_msg);
                    messages.push(assistant_msg);
                    messages.push(user_msg);
                    continue;
                }
            }
        }

        let prev_len = messages.len();
        let no_tool = handle_tool_step(
            &executor,
            response,
            &mut messages,
            &hooks,
            &loop_task_id,
            &agent_name,
        )
        .await;

        if no_tool {
            for msg in &messages[prev_len..] {
                append_transcript(&mut transcript_writer, &mut seq, msg);
            }
            if turns == 1 && !any_tool_called {
                tracing::debug!("sub-agent text-only first turn — sending nudge to use tools");
                let nudge = make_message(
                    Role::User,
                    "Please use the available tools to complete the task. \
                     Do not announce intentions — execute them."
                        .into(),
                );
                append_transcript(&mut transcript_writer, &mut seq, &nudge);
                messages.push(nudge);
                continue;
            }
            break;
        }
        any_tool_called = true;
        for msg in &messages[prev_len..] {
            append_transcript(&mut transcript_writer, &mut seq, msg);
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
