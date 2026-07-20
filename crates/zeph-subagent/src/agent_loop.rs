// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use tokio::sync::{mpsc, watch};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{
    ChatResponse, LlmProvider, Message, MessageMetadata, MessagePart, Role, ThinkingBlock,
    ToolDefinition, ToolUseRequest,
};
use zeph_llm::sse::{ToolSseEvent, ToolSseStream};
use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};
use zeph_tools::executor::{ErasedToolExecutor, ToolCall};

use super::filter::FilteredToolExecutor;
use super::forward::ForwardSender;
use super::grants::{GrantedSecret, PermissionGrants, SecretRequest, ToolGrantCheck};
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
    /// Shared progress heartbeat for idle-timeout detection (issue #6245).
    ///
    /// `Some` when this spawn is orchestration-dispatched via `DagScheduler` — the driver
    /// clones the `Arc` in here and keeps the original for `DagScheduler::record_spawn`'s
    /// `last_progress_at`. `run_agent_loop` stores `monotonic_millis()` into it once per
    /// turn boundary. `None` for spawns not tracked by an orchestration scheduler (the
    /// standalone `/agent run`/`resume` commands), which are never idle-tracked.
    pub(super) progress_at: Option<Arc<AtomicU64>>,
    /// Cross-crate debug-dump sink, threaded down from `SpawnContext::debug_dump_sink`
    /// (issue #6391). `None` when debug dumps are disabled.
    pub(super) debug_dump_sink: Option<Arc<dyn zeph_llm::debug_dump::DebugDumpSink>>,
    /// Live transcript forwarding sender (issue #6359 FR-002a per-turn; issue #6456 FR-002b
    /// token-level intra-turn deltas on providers with native tool-streaming support). `None`
    /// when forwarding is disabled, no consumer surface is active, or no `TaskSupervisor` is
    /// wired — the `if let Some(f)` gate at every call site is then a genuine no-op (FR-007):
    /// no allocation, no clone, nothing sent. Owned exclusively by this run's own turn loop
    /// for its lifetime; never clone the inner sender into a longer-lived struct (see
    /// `forward::ForwardSender` docs).
    pub(super) forward: Option<ForwardSender>,
    /// Shared secret-mask registry (issue #6492), the same `Arc` used for the parent's
    /// outbound-LLM masking and the forwarding drain's `SanitizeLayers`. Applied to every
    /// tool-result's `content` in [`handle_tool_step`] immediately before it is pushed into
    /// `messages` — the single chokepoint feeding the transcript, the next turn's LLM
    /// context, and the debug dump. `None` when no registry is wired (mirrors
    /// `SubAgentManager::secret_registry`'s `None` default).
    pub(super) secret_registry: Option<Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
    /// Shared, task-scoped `GrantKind::Tool` permission grants for this sub-agent, checked
    /// live in [`handle_tool_step`] immediately before every tool dispatch (issue #6567).
    /// Shared with the manager-side `SubAgentHandle::grants` via `Arc<Mutex<..>>` rather than
    /// delivered as a point-in-time snapshot: unlike `GrantKind::Secret`, which is explicitly
    /// requested and delivered per-key, a `Tool` grant applies implicitly to every matching
    /// tool-name dispatch, so `PermissionGrants::revoke_all()` (task collection, cancellation,
    /// or drop) must be immediately visible to this running loop task without a delivery
    /// round-trip.
    pub(super) tool_grants: Arc<Mutex<PermissionGrants>>,
}

/// Record a progress heartbeat, if this loop has a live handle. No-op for `None` (untracked
/// or `RunInline`-style spawns — see [`AgentLoopArgs::progress_at`]).
fn record_progress(progress_at: Option<&Arc<AtomicU64>>) {
    if let Some(p) = progress_at {
        p.store(zeph_common::monotonic_millis(), Ordering::Relaxed);
    }
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

/// Drive a `ToolSseStream` from a provider's native streaming-with-tools path (issue #6456,
/// FR-002b), forwarding each text/thinking delta through `forward` as it arrives and
/// assembling the same [`ChatResponse`] shape [`LlmProvider::chat_with_tools`] would return.
///
/// Forwarded deltas are display-only (see `forward.rs` module docs' "Design contract"
/// section): `text_buf`/`thinking_blocks`/`tool_calls` are accumulated locally regardless of
/// whether any individual delta actually made it onto the (tail-drop) forward channel, so a
/// dropped delta only ever produces a display gap — the `ChatResponse` this function returns
/// is unaffected and remains the sole input to the next turn's LLM context.
///
/// Mirrors `zeph-core`'s `SpeculativeStreamDrainer::drive` accumulation logic (same
/// `ToolSseStream`/`ToolSseEvent` contract), minus speculative dispatch, which is out of
/// scope for the subagent loop.
///
/// # Errors
///
/// Returns the first `LlmError` reported by the stream.
#[tracing::instrument(name = "subagent.agent_loop.stream_turn", skip_all)]
async fn drive_tool_stream(
    mut stream: ToolSseStream,
    forward: &ForwardSender,
) -> Result<ChatResponse, zeph_llm::LlmError> {
    let mut tool_calls: Vec<ToolUseRequest> = Vec::new();
    let mut thinking_blocks: Vec<ThinkingBlock> = Vec::new();
    let mut text_buf = String::new();

    while let Some(event) = stream.next().await {
        match event {
            ToolSseEvent::ContentChunk(text) => {
                forward.send_text(&text);
                text_buf.push_str(&text);
            }
            ToolSseEvent::ThinkingChunk(text) => {
                forward.send_thinking(&text);
            }
            ToolSseEvent::ThinkingBlockDone(block) => {
                thinking_blocks.push(block);
            }
            ToolSseEvent::ToolCallComplete {
                id,
                name,
                full_json,
                ..
            } => {
                let input = serde_json::from_str(&full_json)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                tool_calls.push(ToolUseRequest {
                    id,
                    name: name.into(),
                    input,
                });
            }
            ToolSseEvent::Error(e) => return Err(e),
            // `ToolBlockStart`/`InputJsonDelta` only matter for the speculative-dispatch
            // drainer (tool metadata ahead of the final JSON, used for early dispatch);
            // `Compaction` summaries are a known TODO (matches `SpeculativeStreamDrainer::
            // drive`'s same limitation — not yet surfaced to the caller). Any future
            // `#[non_exhaustive]` variant is likewise a no-op here rather than a compile
            // error on an upstream addition.
            _ => {}
        }
    }

    let text = if text_buf.is_empty() {
        None
    } else {
        Some(text_buf)
    };
    if tool_calls.is_empty() {
        Ok(ChatResponse::Text(text.unwrap_or_default()))
    } else {
        Ok(ChatResponse::ToolUse {
            text,
            tool_calls,
            thinking_blocks,
        })
    }
}

/// Attempt the streaming-with-tools path when forwarding is active, falling back to the
/// plain [`LlmProvider::chat_with_tools`] call otherwise or on any streaming-setup error
/// (issue #6456, FR-002b).
///
/// Returns `(response, streamed)`; `streamed == true` means `forward` already received this
/// turn's text/thinking as deltas via [`drive_tool_stream`], so the caller (`run_turn`) must
/// not forward the same content again as a whole-turn chunk.
///
/// Providers without a native tool-streaming implementation (every backend except Claude,
/// see `AnyProvider::chat_with_tools_stream`) return `Err(LlmError::Unavailable)` immediately
/// — no network round-trip is attempted before falling back, so this costs nothing beyond a
/// synchronous match for non-Claude providers, and per-turn forwarding behaves exactly as it
/// did before this issue (FR-002a, unchanged).
async fn call_provider_streaming_or_plain(
    provider: &AnyProvider,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    forward: Option<&ForwardSender>,
) -> Result<(ChatResponse, bool), zeph_llm::LlmError> {
    if let Some(f) = forward {
        match provider.chat_with_tools_stream(messages, tool_defs).await {
            Ok(stream) => return drive_tool_stream(stream, f).await.map(|r| (r, true)),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "provider has no native tool-streaming support (or the streaming request \
                     failed); falling back to non-streaming chat_with_tools"
                );
            }
        }
    }
    provider
        .chat_with_tools(messages, tool_defs)
        .await
        .map(|r| (r, false))
}

#[tracing::instrument(name = "subagent.agent_loop.call_provider", skip_all, err)]
#[allow(clippy::too_many_arguments)]
async fn call_provider_with_status(
    provider: &AnyProvider,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    status_tx: &watch::Sender<SubAgentStatus>,
    turns: u32,
    started_at: Instant,
    llm_timeout: std::time::Duration,
    debug_dump_sink: Option<&dyn zeph_llm::debug_dump::DebugDumpSink>,
    forward: Option<&ForwardSender>,
) -> Result<(ChatResponse, bool), super::error::SubAgentError> {
    // Mirrors `zeph-core`'s `prepare_chat_debug_dump`/`write_chat_debug_dump` pair so
    // sub-agent LLM calls are captured through the same `--debug-dump` pipeline as the
    // top-level agent loop (#6391). `None` when debug dumps are disabled.
    let dump_id = debug_dump_sink.map(|sink| {
        let provider_request = if sink.is_trace_format() {
            serde_json::Value::Null
        } else {
            provider.debug_request_json(messages, tool_defs, false) // lgtm[rust/cleartext-logging]
        };
        sink.dump_request(provider.name(), messages, tool_defs, provider_request)
    });

    let llm_result = tokio::time::timeout(
        llm_timeout,
        call_provider_streaming_or_plain(provider, messages, tool_defs, forward),
    )
    .await
    .map_err(|_| {
        tracing::warn!(
            timeout_secs = llm_timeout.as_secs(),
            "sub-agent LLM call timed out"
        );
        let timeout_err = super::error::SubAgentError::Llm("LLM call timed out".to_owned());
        // Without this, status_tx stays frozen at its last `Working` value forever —
        // the TUI sidebar and `collect_finished_subagents()` never see a terminal
        // state, so the handle is never reaped (#6381, same defect class as #6257's
        // setup-phase fix).
        let _ = status_tx.send(SubAgentStatus {
            state: SubAgentState::Failed,
            last_message: Some(timeout_err.to_string()),
            turns_used: turns,
            started_at,
        });
        if let Some(f) = forward {
            f.send_terminal(SubAgentState::Failed);
        }
        timeout_err
    })?;
    match llm_result {
        Ok((r, streamed)) => {
            if let (Some(sink), Some(id)) = (debug_dump_sink, dump_id) {
                sink.dump_response(id, &r);
            }
            Ok((r, streamed))
        }
        Err(e) => {
            tracing::error!(error = %e, "sub-agent LLM call failed");
            let _ = status_tx.send(SubAgentStatus {
                state: SubAgentState::Failed,
                last_message: Some(e.to_string()),
                turns_used: turns,
                started_at,
            });
            if let Some(f) = forward {
                f.send_terminal(SubAgentState::Failed);
            }
            Err(super::error::SubAgentError::Llm(e.to_string()))
        }
    }
}

/// Publish the loop's final status and, if forwarding is active, its matching terminal chunk
/// — kept as one call site so `run_agent_loop` stays under clippy's `too_many_lines`
/// threshold.
///
/// `status_tx` always publishes `Completed` here, matching this loop's pre-existing behavior
/// (unchanged, including for the graceful-cancel and max-turns-exhausted break paths — not in
/// scope to change here). The *forwarded* terminal state is independently accurate: callers
/// pass `forward_state = SubAgentState::Canceled` when the loop broke due to cancellation
/// (impl-critic M1) so a `--bare` consumer reading the forwarded terminal isn't misled, while
/// every other exit path passes `Completed` (identical to `status_tx`).
fn publish_completed_status(
    status_tx: &watch::Sender<SubAgentStatus>,
    forward: Option<&ForwardSender>,
    forward_state: SubAgentState,
    last_result: &str,
    turns: u32,
    started_at: Instant,
) {
    let _ = status_tx.send(SubAgentStatus {
        state: SubAgentState::Completed,
        last_message: Some(last_result.chars().take(120).collect()),
        turns_used: turns,
        started_at,
    });
    if let Some(f) = forward {
        f.send_terminal(forward_state);
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
    debug_dump_sink: Option<&dyn zeph_llm::debug_dump::DebugDumpSink>,
    forward: Option<&ForwardSender>,
    secret_registry: Option<&zeph_sanitizer::secret_mask::SecretMaskRegistry>,
    tool_grants: &Arc<Mutex<PermissionGrants>>,
    active_tool_grants_seen: &mut std::collections::HashSet<String>,
) -> Result<TurnOutcome, super::error::SubAgentError> {
    let (response, streamed) = call_provider_with_status(
        provider,
        messages,
        tool_defs,
        status_tx,
        *turns,
        started_at,
        llm_timeout,
        debug_dump_sink,
        forward,
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

    // FR-002a/FR-002b/FR-007: forward this turn's text + any visible thinking blocks the
    // instant they are available. When `streamed` is true, `call_provider_with_status` already
    // forwarded every delta incrementally via `drive_tool_stream` while the turn was still in
    // flight — forwarding the same content again here as one whole-turn chunk would just
    // double it on the display surfaces, so this block is skipped in that case. The `if let
    // Some(f)` gate wraps the thinking extraction itself, not just the send — zero allocation
    // when forwarding is inactive (critic M2). Must run before `response` is moved into
    // `handle_tool_step` below.
    if !streamed && let Some(f) = forward {
        f.send_text(&response_text);
        if let ChatResponse::ToolUse {
            thinking_blocks, ..
        } = &response
        {
            for block in thinking_blocks {
                if let ThinkingBlock::Thinking { thinking, .. } = block {
                    f.send_thinking(thinking);
                }
            }
        }
    }

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
        secret_registry,
        tool_grants,
        active_tool_grants_seen,
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
    secret_registry: Option<&zeph_sanitizer::secret_mask::SecretMaskRegistry>,
    tool_grants: &Arc<Mutex<PermissionGrants>>,
    active_tool_grants_seen: &mut std::collections::HashSet<String>,
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
                // #6567 (FR-001/FR-002/FR-003/NFR-001): a `GrantKind::Tool` grant for this tool
                // name, if one exists, must still be active or the call is rejected outright —
                // before any hook fires or the executor runs. This differs from the `Secret`
                // path's re-check below, which only *omits* an expired credential from the call
                // (the tool still runs); an expired/revoked `Tool` grant means the sub-agent is
                // no longer authorized to use this tool at all. A tool name with no grant record
                // at all is today's universal case (no production caller creates `Tool` grants
                // yet) and is left unrestricted — see FR-004/NFR-004.
                //
                // Semantics (confirmed, not allow-list — see `PermissionGrants::check_tool_grant`
                // for the full explanation): a `Tool` grant only time-boxes a tool the sub-agent
                // is already permitted to call under its static `ToolPolicy`/`AutonomyLevel`. A
                // grant for one tool name never restricts any other tool name — there is no
                // "presence of any grant implies allow-list" mode. Matching is exact tool-name
                // equality only, no prefix/glob for tool families (e.g. MCP server-scoped names).
                //
                // `revoke_all()` (and `check_tool_grant`'s own eviction of a stale entry) clears
                // the shared `PermissionGrants` Vec entirely, so a fresh `check_tool_grant`
                // alone cannot distinguish "revoked"/"expired-and-evicted" from "never granted"
                // — both read back as `NoGrant`. `active_tool_grants_seen` closes that gap: once
                // this loop has observed a tool name as `Active` OR `Expired` — i.e. under grant
                // management at all — it stays enforced (denied on any later `NoGrant` reading)
                // for the rest of the run, never silently reverting to unrestricted. A fresh
                // `Active` reading still unconditionally overrides and re-permits (a legitimate
                // re-grant), since only the `NoGrant` arm consults this set. This mirrors the
                // `Secret` path's own documented scope for `revoke_all()`: reliable for
                // concurrent revocation observed within an in-flight tool-call batch (the case
                // the FR-003 edge cases table describes), not a global "was ever granted before
                // this task even started checking" guarantee, since the shared store retains no
                // tombstone.
                let tool_grant_check = tool_grants
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .check_tool_grant(tc.name.as_str());
                let tool_grant_rejected = match tool_grant_check {
                    ToolGrantCheck::Active => {
                        active_tool_grants_seen.insert(tc.name.to_string());
                        false
                    }
                    ToolGrantCheck::Expired => {
                        // Also mark as seen here, not just on `Active`: a grant that is already
                        // expired on its very first check (e.g. created with a zero/negative
                        // effective TTL) would otherwise never enter this set, so a later
                        // `NoGrant` reading (once `check_tool_grant`'s sweep evicts the stale
                        // Vec entry) would be indistinguishable from "never granted" and
                        // silently re-permit the tool on the next dispatch.
                        active_tool_grants_seen.insert(tc.name.to_string());
                        true
                    }
                    ToolGrantCheck::NoGrant => active_tool_grants_seen.contains(tc.name.as_str()),
                };
                if tool_grant_rejected {
                    // Deliberately NOT removed from `active_tool_grants_seen` here: this tool
                    // name was observed `Active` at least once, so it must stay enforced for
                    // the rest of the run (see the comment above) — removing it on rejection
                    // would make the very next dispatch of the same tool read back `NoGrant`
                    // (the Vec entry is already gone, evicted by `check_tool_grant`'s
                    // `is_active`/`sweep_expired`) and be treated as "never granted", silently
                    // re-permitting a tool whose grant just expired or was revoked. A later
                    // legitimate re-grant still overrides this: `ToolGrantCheck::Active` is
                    // unconditional and does not consult this set.
                    tracing::warn!(
                        tool = %tc.name,
                        task_id,
                        "tool call rejected: GrantKind::Tool grant expired or revoked"
                    );
                    result_parts.push(MessagePart::ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: format!(
                            "[tool error]: capability grant for tool '{}' has expired or been \
                             revoked",
                            tc.name
                        ),
                        is_error: true,
                    });
                    continue;
                }

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

                // #6492: mask any known vault secret (this run's own grants and any other
                // registered secret) out of the tool result before it reaches `messages` —
                // the single chokepoint feeding the transcript, the next turn's LLM context,
                // and the debug dump. `would_mask` is the allocation-free pre-check so a turn
                // with no secret in it pays no extra cost. This mitigation is contingent on
                // `secret_masking.enabled` (default `true` — `secret_registry` is `None` when
                // disabled) and on the secret being at least `MIN_SECRET_LEN` (8) bytes.
                if let Some(registry) = secret_registry
                    && registry.would_mask(&content)
                {
                    content = registry.mask(&content);
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
#[allow(clippy::too_many_lines)] // top-level orchestration function; same precedent as handle_tool_step/spawn/resume in this crate
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
        progress_at,
        debug_dump_sink,
        forward,
        secret_registry,
        tool_grants,
    } = args;
    let debug_dump_sink = debug_dump_sink.as_deref();
    let secret_registry = secret_registry.as_deref();

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
    // Accumulates resolved secrets so a later approval doesn't evict an earlier one; TTL'd
    // entries are evicted live in `handle_tool_step` (see `handle_secret_request`).
    let mut granted_secrets: HashMap<String, GrantedSecret> = HashMap::new();
    // Tool names this loop has observed as actively `GrantKind::Tool`-granted at least once;
    // see the comment in `handle_tool_step` on why this local memory is required to detect
    // `revoke_all()`-triggered revocation (issue #6567).
    let mut active_tool_grants_seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Forwarded terminal state, independent of status_tx's always-Completed publish below
    // (impl-critic M1) — set to Canceled on either cancellation exit path.
    let mut forward_terminal_state = SubAgentState::Completed;
    // #6494: captures a `run_turn` error so control still falls through to the unconditional
    // post-loop finalize block below instead of an early `?`-return skipping it — a skipped
    // finalize leaves the transcript chained-but-unanchored, reopening the whole-strip
    // downgrade #6449/#6461 closed for any turn that ends on an LLM error/timeout.
    let mut pending_error: Option<super::error::SubAgentError> = None;

    loop {
        record_progress(progress_at.as_ref());

        if cancel.is_cancelled() {
            tracing::debug!("sub-agent cancelled, stopping loop");
            forward_terminal_state = SubAgentState::Canceled;
            break;
        }
        if turns >= max_turns {
            tracing::debug!(turns, max_turns, "sub-agent reached max_turns limit");
            break;
        }

        let turn_result = run_turn(
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
            debug_dump_sink,
            forward.as_ref(),
            secret_registry,
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;

        match turn_result {
            Ok(TurnOutcome::ToolCalled) => any_tool_called = true,
            Ok(TurnOutcome::NudgeSent | TurnOutcome::SecretHandled) => {}
            Ok(TurnOutcome::Done) => break,
            Ok(TurnOutcome::Cancelled) => {
                forward_terminal_state = SubAgentState::Canceled;
                break;
            }
            Err(e) => {
                // `call_provider_with_status` already published a terminal `Failed` status
                // (and forwarded terminal chunk) before returning this error — capture it and
                // fall through to the unconditional post-loop finalize instead of an early
                // return. `forward_terminal_state` is intentionally left untouched here: the
                // post-loop `publish_completed_status` call (its only reader) is unconditionally
                // skipped whenever `pending_error.is_some()`, which is always true on this arm,
                // so setting it would be a dead store.
                pending_error = Some(e);
                break;
            }
        }

        record_progress(progress_at.as_ref());

        trim_message_history(&mut messages, max_history_messages);
    }

    // Skipped on the error path: `call_provider_with_status` already published `Failed` (and
    // its matching forwarded terminal) before returning the error captured in `pending_error`
    // — publishing `Completed` here as well would overwrite that status and double-forward a
    // terminal chunk.
    if pending_error.is_none() {
        publish_completed_status(
            &status_tx,
            forward.as_ref(),
            forward_terminal_state,
            &last_result,
            turns,
            started_at,
        );
    }

    // Anchor the transcript (issue #6449): best-effort, logged rather than propagated — the
    // transcript file itself is already durably written, and a failed anchor put only means this
    // one file falls back to #6453-level chain-only protection, never data loss. Runs
    // unconditionally on every loop exit path, including the LLM-error path above (#6494) —
    // skipping it there left the transcript chained-but-unanchored, reopening the whole-strip
    // downgrade #6449/#6461 closed.
    if let Some(writer) = transcript_writer
        && let Err(e) = writer.finalize().await
    {
        tracing::warn!(error = %e, task_id = %loop_task_id, "transcript anchor finalize failed");
    }

    if let Some(e) = pending_error {
        return Err(e);
    }
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
mod record_progress_tests {
    use super::*;

    #[test]
    fn none_handle_is_a_noop() {
        // Must not panic and must not affect anything — this is the RunInline/untracked path.
        record_progress(None);
    }

    #[test]
    fn some_handle_stores_a_fresh_monotonic_reading() {
        // Seed with a sentinel a real monotonic_millis() reading can never produce (0 is not
        // safe here — an isolated test binary can be young enough that monotonic_millis()
        // itself legitimately reads 0, which would make an unwritten handle indistinguishable
        // from a written one). u64::MAX proves the write actually happened; the >= check
        // proves it reflects a reading taken at-or-after this test's own pre-call timestamp.
        let before = zeph_common::monotonic_millis();
        let handle = Arc::new(AtomicU64::new(u64::MAX));

        record_progress(Some(&handle));

        let stored = handle.load(Ordering::Relaxed);
        assert_ne!(
            stored,
            u64::MAX,
            "record_progress must overwrite the initial placeholder value"
        );
        assert!(
            stored >= before,
            "stored value ({stored}) must be a monotonic reading taken at-or-after the pre-call \
             timestamp ({before})"
        );
    }

    #[test]
    fn some_handle_overwrites_a_stale_previous_value() {
        let handle = Arc::new(AtomicU64::new(0));
        record_progress(Some(&handle));
        let first = handle.load(Ordering::Relaxed);

        std::thread::sleep(std::time::Duration::from_millis(5));
        record_progress(Some(&handle));
        let second = handle.load(Ordering::Relaxed);

        assert!(
            second >= first,
            "a second call must never move the stored heartbeat backward"
        );
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

    /// Executor whose tool output summary echoes back exactly the string it is constructed
    /// with — simulates a tool call that leaks a secret's raw value into its own output
    /// (e.g. `shell: echo $SOME_VAULT_KEY`), for #6492 masking regression tests.
    struct EchoingExecutor {
        echo: String,
    }

    impl ErasedToolExecutor for EchoingExecutor {
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
            let echo = self.echo.clone();
            let tool_name = call.tool_id.clone();
            Box::pin(async move {
                Ok(Some(ToolOutput {
                    tool_name,
                    summary: echo,
                    blocks_executed: 1,
                    filter_stats: None,
                    diff: None,
                    streamed: false,
                    terminal_id: None,
                    locations: None,
                    raw_response: None,
                    claim_source: None,
                    ..Default::default()
                }))
            })
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
        let tool_grants = Arc::new(Mutex::new(PermissionGrants::default()));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
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
        let tool_grants = Arc::new(Mutex::new(PermissionGrants::default()));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
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
        let tool_grants = Arc::new(Mutex::new(PermissionGrants::default()));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
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
        let tool_grants = Arc::new(Mutex::new(PermissionGrants::default()));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
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

    // --- #6492: tool-result content masking ---

    #[tokio::test]
    async fn tool_result_containing_a_registered_secret_is_masked_before_reaching_messages() {
        let secret_value = "sk-supersecretvalue12345678";
        let executor = FilteredToolExecutor::new(
            Arc::new(EchoingExecutor {
                echo: secret_value.to_owned(),
            }) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let mut messages = Vec::new();
        let mut granted_secrets = HashMap::new();
        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());
        let registry = zeph_sanitizer::secret_mask::SecretMaskRegistry::new();
        registry.register(
            "SOME_VAULT_KEY",
            secret_value,
            zeph_sanitizer::secret_mask::SecretCategory::ApiKey,
        );
        let tool_grants = Arc::new(Mutex::new(PermissionGrants::default()));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            Some(&registry),
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;
        assert!(!no_tool, "a tool call was made");

        let content = messages
            .iter()
            .find_map(|m| {
                m.parts.iter().find_map(|p| match p {
                    MessagePart::ToolResult { content, .. } => Some(content.clone()),
                    _ => None,
                })
            })
            .expect("a ToolResult part must be present");
        assert!(
            !content.contains(secret_value),
            "raw secret must not appear in the tool-result content pushed into messages \
             (would leak into transcript/LLM context/debug dump): {content}"
        );
        assert!(
            content.contains("<SECRET:api_key:"),
            "masked content must carry the typed placeholder: {content}"
        );
    }

    #[tokio::test]
    async fn tool_result_is_left_unmasked_when_no_secret_registry_is_wired() {
        // Regression guard: passing `None` (registry disabled / not wired) must leave
        // tool-result content byte-for-byte identical to pre-fix behavior — masking must
        // never be forced on when no registry is configured.
        let secret_value = "sk-supersecretvalue12345678";
        let executor = FilteredToolExecutor::new(
            Arc::new(EchoingExecutor {
                echo: secret_value.to_owned(),
            }) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let mut messages = Vec::new();
        let mut granted_secrets = HashMap::new();
        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());
        let tool_grants = Arc::new(Mutex::new(PermissionGrants::default()));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;
        assert!(!no_tool);

        let content = messages
            .iter()
            .find_map(|m| {
                m.parts.iter().find_map(|p| match p {
                    MessagePart::ToolResult { content, .. } => Some(content.clone()),
                    _ => None,
                })
            })
            .expect("a ToolResult part must be present");
        assert!(
            content.contains(secret_value),
            "with no registry wired, content must be unchanged (pre-fix baseline): {content}"
        );
    }

    // --- #6567: GrantKind::Tool enforcement ---
    // `tool_use_response()` always requests the "shell" tool, matching `GrantKind::Tool("shell")`.

    #[tokio::test]
    async fn tool_grant_absent_leaves_dispatch_unrestricted() {
        // US-003/FR-004: a sub-agent with zero GrantKind::Tool entries (today's universal
        // production state) must see no behavior change — dispatch proceeds normally.
        let recorder = Arc::new(RecordingExecutor::default());
        let executor = FilteredToolExecutor::new(
            Arc::clone(&recorder) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let mut messages = Vec::new();
        let mut granted_secrets = HashMap::new();
        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());
        let tool_grants = Arc::new(Mutex::new(PermissionGrants::default()));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;
        assert!(!no_tool, "a tool call was made");
        assert_eq!(
            recorder.calls.lock().unwrap().len(),
            1,
            "tool must dispatch"
        );
    }

    #[tokio::test]
    async fn tool_grant_active_allows_tool_dispatch() {
        // US-001 baseline: an active (not yet expired) GrantKind::Tool grant for the tool
        // being dispatched must not block the call.
        let recorder = Arc::new(RecordingExecutor::default());
        let executor = FilteredToolExecutor::new(
            Arc::clone(&recorder) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let mut messages = Vec::new();
        let mut granted_secrets = HashMap::new();
        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());
        let mut grants = PermissionGrants::default();
        grants.add(
            crate::grants::GrantKind::Tool("shell".to_owned()),
            Duration::from_mins(5),
        );
        let tool_grants = Arc::new(Mutex::new(grants));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;
        assert!(!no_tool, "a tool call was made");
        assert_eq!(
            recorder.calls.lock().unwrap().len(),
            1,
            "an active Tool grant must not block dispatch"
        );
    }

    #[tokio::test]
    async fn tool_grant_expired_rejects_tool_dispatch_and_is_evicted() {
        // US-001/FR-002: a GrantKind::Tool grant whose TTL has elapsed must reject the tool
        // call before the executor runs, and the expired grant must be evicted.
        let recorder = Arc::new(RecordingExecutor::default());
        let executor = FilteredToolExecutor::new(
            Arc::clone(&recorder) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let mut messages = Vec::new();
        let mut granted_secrets = HashMap::new();
        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());
        let mut grants = PermissionGrants::default();
        grants.add_test_grant(
            crate::grants::GrantKind::Tool("shell".to_owned()),
            Instant::now().checked_sub(Duration::from_secs(10)).unwrap(),
            Duration::from_secs(1),
        );
        let tool_grants = Arc::new(Mutex::new(grants));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;
        assert!(
            !no_tool,
            "a ToolResult was still produced (rejection, not silence)"
        );
        assert_eq!(
            recorder.calls.lock().unwrap().len(),
            0,
            "the executor must never run for an expired Tool grant"
        );

        let content = messages
            .iter()
            .find_map(|m| {
                m.parts.iter().find_map(|p| match p {
                    MessagePart::ToolResult {
                        content, is_error, ..
                    } if *is_error => Some(content.clone()),
                    _ => None,
                })
            })
            .expect("an error ToolResult part must be present");
        assert!(
            content.contains("expired") || content.contains("revoked"),
            "rejection reason must be actionable: {content}"
        );
        assert_eq!(
            tool_grants.lock().unwrap().check_tool_grant("shell"),
            ToolGrantCheck::NoGrant,
            "the expired grant must be evicted, not merely bypassed"
        );

        // Regression guard (critic C1): rejection must not un-remember that this tool name was
        // once actively granted — a naive implementation that clears `active_tool_grants_seen`
        // on rejection would read `NoGrant` again here (the Vec entry is already evicted) and
        // silently treat the THIRD dispatch as "never granted", re-permitting the tool.
        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;
        assert!(
            !no_tool,
            "a ToolResult was still produced (rejection, not silence)"
        );
        assert_eq!(
            recorder.calls.lock().unwrap().len(),
            0,
            "the executor must still never run on the third dispatch after expiry — a once-\
             enforced tool name must stay denied for the rest of the run"
        );
    }

    #[tokio::test]
    async fn tool_grant_revoked_via_revoke_all_rejects_subsequent_dispatch() {
        // US-002/FR-003: revoke_all() on the shared PermissionGrants — the same call the
        // manager makes on task collection/cancellation/drop — must be visible to this
        // running loop task's next dispatch attempt without any further plumbing. The first
        // call establishes the grant as actively observed (populating
        // `active_tool_grants_seen`); revoke_all() then runs between the two calls, exactly
        // like a manager-side cancel() racing an in-flight sub-agent loop.
        let recorder = Arc::new(RecordingExecutor::default());
        let executor = FilteredToolExecutor::new(
            Arc::clone(&recorder) as Arc<dyn ErasedToolExecutor>,
            ToolPolicy::InheritAll,
        );
        let hooks = SubagentHooks::default();
        let sanitizer = ContentSanitizer::new(&zeph_config::ContentIsolationConfig::default());
        let mut grants = PermissionGrants::default();
        grants.add(
            crate::grants::GrantKind::Tool("shell".to_owned()),
            Duration::from_mins(5),
        );
        let tool_grants = Arc::new(Mutex::new(grants));
        let mut active_tool_grants_seen = std::collections::HashSet::new();

        let mut messages = Vec::new();
        let mut granted_secrets = HashMap::new();
        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;
        assert!(!no_tool, "a tool call was made");
        assert_eq!(
            recorder.calls.lock().unwrap().len(),
            1,
            "the first dispatch must succeed while the grant is still active"
        );

        // Simulates the manager-side handle revoking grants concurrently (e.g. cancel()).
        tool_grants.lock().unwrap().revoke_all();

        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;
        assert!(
            !no_tool,
            "a ToolResult was still produced (rejection, not silence)"
        );
        assert_eq!(
            recorder.calls.lock().unwrap().len(),
            1,
            "the executor must not run again once the Tool grant has been revoked"
        );

        // Regression guard (critic C1): same as the expiry test's third-dispatch guard — a
        // naive implementation that clears `active_tool_grants_seen` on rejection would
        // silently re-permit this THIRD dispatch (the shared Vec has no tombstone for a
        // revoked grant, so a bare check would read `NoGrant` and look identical to "never
        // granted").
        let no_tool = handle_tool_step(
            &executor,
            tool_use_response(),
            &mut messages,
            &hooks,
            "task-1",
            "bot",
            &sanitizer,
            &mut granted_secrets,
            None,
            &tool_grants,
            &mut active_tool_grants_seen,
        )
        .await;
        assert!(
            !no_tool,
            "a ToolResult was still produced (rejection, not silence)"
        );
        assert_eq!(
            recorder.calls.lock().unwrap().len(),
            1,
            "the executor must still not run on the third dispatch after revocation — a once-\
             enforced tool name must stay denied for the rest of the run"
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

// ── #6456: token-level intra-turn transcript streaming (FR-002b) ──────────
#[cfg(test)]
mod token_streaming_tests {
    use zeph_llm::LlmError;
    use zeph_llm::mock::MockProvider;

    use super::*;
    use crate::forward::new_channel;

    fn tool_stream_from(events: Vec<ToolSseEvent>) -> ToolSseStream {
        Box::pin(tokio_stream::iter(events))
    }

    #[tokio::test]
    async fn drive_tool_stream_assembles_text_and_forwards_every_delta() {
        let (sender, mut rx) = new_channel(Arc::from("task-1"), Arc::from("agent-1"));
        let stream = tool_stream_from(vec![
            ToolSseEvent::ContentChunk("Hello".into()),
            ToolSseEvent::ContentChunk(", world".into()),
            ToolSseEvent::ContentChunk("!".into()),
        ]);

        let response = drive_tool_stream(stream, &sender)
            .await
            .expect("stream must assemble successfully");

        match response {
            ChatResponse::Text(t) => assert_eq!(t, "Hello, world!"),
            other => panic!("expected ChatResponse::Text, got {other:?}"),
        }

        // Every content delta must have been forwarded individually (not just the final
        // accumulated text) — this is the actual FR-002b behavior under test.
        let mut received = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            received.push(format!("{chunk:?}"));
        }
        assert_eq!(received.len(), 3, "each delta must be forwarded separately");
        assert!(received[0].contains("Hello"));
        assert!(received[1].contains(", world"));
        assert!(received[2].contains('!'));
    }

    #[tokio::test]
    async fn drive_tool_stream_forwards_thinking_deltas_and_keeps_final_block() {
        let (sender, mut rx) = new_channel(Arc::from("task-2"), Arc::from("agent-2"));
        let stream = tool_stream_from(vec![
            ToolSseEvent::ThinkingChunk("step one".into()),
            ToolSseEvent::ThinkingChunk(" step two".into()),
            ToolSseEvent::ThinkingBlockDone(ThinkingBlock::Thinking {
                thinking: "step one step two".into(),
                signature: "sig".into(),
            }),
        ]);

        let response = drive_tool_stream(stream, &sender)
            .await
            .expect("stream must assemble successfully");

        match response {
            ChatResponse::Text(t) => assert!(t.is_empty(), "no content chunks were sent"),
            other => panic!("expected empty ChatResponse::Text, got {other:?}"),
        }

        let mut forwarded_thinking = 0;
        while let Ok(chunk) = rx.try_recv() {
            if format!("{chunk:?}").contains("Thinking") {
                forwarded_thinking += 1;
            }
        }
        assert_eq!(
            forwarded_thinking, 2,
            "both thinking deltas must be forwarded incrementally"
        );
    }

    #[tokio::test]
    async fn drive_tool_stream_assembles_tool_use_from_complete_events() {
        let (sender, _rx) = new_channel(Arc::from("task-3"), Arc::from("agent-3"));
        let stream = tool_stream_from(vec![
            ToolSseEvent::ToolBlockStart {
                index: 0,
                id: "call-1".into(),
                name: "shell".into(),
            },
            ToolSseEvent::InputJsonDelta {
                index: 0,
                delta: r#"{"cmd":"ls"}"#.into(),
            },
            ToolSseEvent::ToolCallComplete {
                index: 0,
                id: "call-1".into(),
                name: "shell".into(),
                full_json: r#"{"cmd":"ls"}"#.into(),
            },
        ]);

        let response = drive_tool_stream(stream, &sender)
            .await
            .expect("stream must assemble successfully");

        match response {
            ChatResponse::ToolUse { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name.as_str(), "shell");
                assert_eq!(tool_calls[0].input["cmd"], "ls");
            }
            other => panic!("expected ChatResponse::ToolUse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_tool_stream_propagates_stream_error() {
        let (sender, _rx) = new_channel(Arc::from("task-4"), Arc::from("agent-4"));
        let stream = tool_stream_from(vec![
            ToolSseEvent::ContentChunk("partial".into()),
            ToolSseEvent::Error(LlmError::Unavailable),
        ]);

        let result = drive_tool_stream(stream, &sender).await;
        assert!(result.is_err(), "a stream Error event must propagate");
    }

    /// Critic concern (c): the guaranteed source of truth is the accumulated `ChatResponse`,
    /// never the forwarded deltas. Send far more chunks than the forward channel's capacity
    /// without ever draining the receiver, so every `try_send` past capacity silently
    /// tail-drops — and confirm the returned `ChatResponse` is still complete regardless.
    #[tokio::test]
    async fn drive_tool_stream_accumulation_survives_forward_channel_backpressure() {
        let (sender, _rx) = new_channel(Arc::from("task-5"), Arc::from("agent-5"));
        let mut events = Vec::new();
        let mut expected = String::new();
        for i in 0..300 {
            let piece = format!("chunk{i} ");
            expected.push_str(&piece);
            events.push(ToolSseEvent::ContentChunk(piece));
        }
        let stream = tool_stream_from(events);

        let response = drive_tool_stream(stream, &sender)
            .await
            .expect("stream must assemble successfully even under channel backpressure");

        match response {
            ChatResponse::Text(t) => assert_eq!(
                t, expected,
                "accumulated response text must be complete even though most forwarded \
                 deltas were tail-dropped (never drained)"
            ),
            other => panic!("expected ChatResponse::Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_provider_streaming_or_plain_falls_back_for_non_streaming_provider() {
        // MockProvider has no `AnyProvider::chat_with_tools_stream` branch — it hits the
        // `_ => Err(Unavailable)` arm — so this must fall back to `chat_with_tools`
        // unchanged, exactly like the pre-#6456 behavior (FR-002a).
        let (mock, _counter) =
            MockProvider::default().with_tool_use(vec![ChatResponse::Text("plain reply".into())]);
        let provider = AnyProvider::Mock(mock);
        let (sender, mut rx) = new_channel(Arc::from("task-6"), Arc::from("agent-6"));
        let messages = vec![make_message(Role::User, "hi".into())];

        let (response, streamed) =
            call_provider_streaming_or_plain(&provider, &messages, &[], Some(&sender))
                .await
                .expect("fallback call must succeed");

        assert!(
            !streamed,
            "a non-streaming provider must report streamed = false"
        );
        match response {
            ChatResponse::Text(t) => assert_eq!(t, "plain reply"),
            other => panic!("expected ChatResponse::Text, got {other:?}"),
        }
        // No deltas were ever streamed for this provider — the forward channel must stay
        // empty here; `run_turn` is responsible for the single whole-turn forward in this
        // case (unchanged FR-002a call site, tested separately at the loop level).
        assert!(rx.try_recv().is_err(), "no deltas forwarded on fallback");
    }

    #[tokio::test]
    async fn call_provider_streaming_or_plain_skips_streaming_attempt_when_forward_is_none() {
        let (mock, _counter) =
            MockProvider::default().with_tool_use(vec![ChatResponse::Text("no forward".into())]);
        let provider = AnyProvider::Mock(mock);
        let messages = vec![make_message(Role::User, "hi".into())];

        let (response, streamed) =
            call_provider_streaming_or_plain(&provider, &messages, &[], None)
                .await
                .expect("call must succeed with forwarding disabled");

        assert!(!streamed);
        match response {
            ChatResponse::Text(t) => assert_eq!(t, "no forward"),
            other => panic!("expected ChatResponse::Text, got {other:?}"),
        }
    }
}
